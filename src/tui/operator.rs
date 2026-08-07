use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::process::{Command, Stdio};

use crate::resource::{ResourceKind, discover};
use crate::tui_runtime::TerminalRuntime;
use crate::view;
use crate::workflow::definition::{DefinitionSnapshot, LaunchMode, WorkflowDefinition};

use super::{PanelFocus, Tui};

#[derive(Clone)]
struct LaunchCandidate {
    definition: WorkflowDefinition,
    scope: String,
}

#[derive(Debug, PartialEq, Eq)]
struct WorkflowControlRequest {
    command: String,
    run_id: String,
    step: Option<String>,
}

impl Tui {
    pub(super) fn control_selected_workflow(
        &mut self,
        runtime: &mut TerminalRuntime,
        requested: &str,
    ) -> Result<bool, String> {
        let Some(dashboard) = self.current_workflow_dashboard() else {
            return Ok(false);
        };
        let request = match workflow_control_request(&dashboard, requested) {
            Ok(request) => request,
            Err(message) => {
                self.show_message(&message)?;
                return Ok(true);
            }
        };
        let command = request.command.as_str();
        let mut arguments = vec!["workflow", command, request.run_id.as_str()];
        if let Some(step) = request.step.as_deref() {
            let executable = std::env::current_exe().map_err(|error| error.to_string())?;
            let preview = runtime.suspend_for(|| {
                Command::new(executable)
                    .arg("--repo")
                    .arg(&self.repo.root)
                    .args([
                        "workflow",
                        command,
                        request.run_id.as_str(),
                        step,
                        "--preview",
                    ])
                    .output()
                    .map_err(|error| format!("preview Workflow control: {error}"))
            })?;
            if !preview.status.success() {
                return Err(String::from_utf8_lossy(&preview.stderr).trim().to_string());
            }
            self.notice_dialog(
                runtime,
                "Downstream Invalidation Preview",
                String::from_utf8_lossy(&preview.stdout)
                    .lines()
                    .map(|line| view::DialogLine {
                        text: line.to_string(),
                        attention: false,
                    })
                    .collect(),
            )?;
            if !self.confirm_action_dialog(
                runtime,
                "Workflow Step Control",
                &format!("{command} {step} in {}?", request.run_id),
                false,
            )? {
                return Ok(true);
            }
            arguments.push(step);
        } else if command == "cancel"
            && !self.confirm_action_dialog(
                runtime,
                "Cancel Workflow Run",
                &format!("cancel {}?", request.run_id),
                false,
            )?
        {
            return Ok(true);
        }
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let status = runtime.suspend_for(|| {
            Command::new(executable)
                .arg("--repo")
                .arg(&self.repo.root)
                .args(arguments)
                .status()
                .map_err(|error| format!("run Workflow control: {error}"))
        })?;
        if !status.success() {
            return Err(format!("Workflow control exited with {status}"));
        }
        self.show_message(&format!("{command} requested for {}", request.run_id))?;
        Ok(true)
    }

    pub(super) fn launch_workflow(&mut self, runtime: &mut TerminalRuntime) -> Result<(), String> {
        let context = self.workflow_context();
        let global = crate::util::prism_config_dir();
        let local = self.repo.root.join(".prism");
        // Rescan on every launcher opening. Workflow TOMLs are runtime resources, not an
        // installed registry, so files added while the TUI is open are immediately available.
        let candidates =
            runtime.suspend_for(|| manual_workflow_candidates(&global, &local, &context))?;
        if candidates.is_empty() {
            return self.show_message("no manual workflow is compatible with the selected context");
        }

        let fzf = self.config.tool("fzf");
        let selected = runtime.suspend_for(|| select_with_fzf(&fzf, &candidates))?;
        let Some(id) = selected else {
            return Ok(());
        };
        let candidate = candidates
            .iter()
            .find(|candidate| candidate.definition.id == id)
            .ok_or_else(|| "fzf returned an unknown workflow identity".to_string())?;

        let inputs = context_inputs(&candidate.definition, &context);
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let snapshot =
            runtime.suspend_for(|| workflow_launch_snapshot(&executable, &self.repo.root, &id))?;
        let Some(inputs) = self.prompt_workflow_input_form(runtime, &snapshot, inputs)? else {
            return Ok(());
        };
        let arguments = workflow_launch_arguments(&self.repo.root, &id, &inputs)?;
        let status = runtime.suspend_for(|| {
            Command::new(executable)
                .args(arguments)
                .status()
                .map_err(|error| format!("launch workflow command: {error}"))
        })?;
        if !status.success() {
            return Err(format!("workflow command exited with {status}"));
        }
        self.show_message(&format!("launched {id}"))
    }

    pub(super) fn show_workflow_management(
        &mut self,
        runtime: &mut TerminalRuntime,
    ) -> Result<(), String> {
        let choices = view::ChoiceList {
            title: "Workflow Management".into(),
            choices: [
                ("w", "workflows"),
                ("t", "Triggers"),
                ("p", "packages"),
                ("e", "extensions"),
                ("s", "skills"),
                ("m", "templates"),
            ]
            .into_iter()
            .map(|(key, label)| view::KeyChoice::new(key, label))
            .collect(),
        };
        let Some(selected) = self.prompt_choice_dialog(runtime, choices)? else {
            return Ok(());
        };
        let (prefix, operations): (&[&str], &[(&str, &str, &str)]) = match selected.as_str() {
            "w" => (
                &["workflow"],
                &[
                    ("l", "list", ""),
                    ("s", "show", "<id>"),
                    ("n", "new", "<id>"),
                    ("c", "copy", "<source-id> <new-id>"),
                    ("e", "edit", "<id>"),
                    ("v", "validate", "<id-or-path>"),
                    ("p", "preview", "<id>"),
                    ("h", "history", "[run-id]"),
                    ("m", "migrate", "<id-or-path> [--apply]"),
                    ("u", "updates", ""),
                ],
            ),
            "t" => (
                &["workflow", "trigger"],
                &[
                    ("l", "list", ""),
                    ("s", "show", "<id>"),
                    ("e", "enable", "<id>"),
                    ("d", "disable", "<id>"),
                    ("r", "run-now", "<id>"),
                    ("h", "history", "<id>"),
                    ("o", "doctor", ""),
                ],
            ),
            "p" => (
                &["package"],
                &[
                    ("l", "list", ""),
                    ("s", "show", "<id>"),
                    ("n", "new", "<id> [path]"),
                    ("v", "validate", "<path>"),
                    ("i", "install", "<source>"),
                    ("u", "update", "<id> [source]"),
                    ("r", "remove", "<id>"),
                ],
            ),
            "e" => (
                &["extension"],
                &[
                    ("l", "list", ""),
                    ("s", "show", "<id>"),
                    ("n", "new", "<id>"),
                    ("e", "edit", "<id>"),
                    ("c", "check", "<path>"),
                    ("b", "build", "<id> [path]"),
                    ("r", "reload", "<id> [path]"),
                    ("d", "doctor", "[id]"),
                ],
            ),
            "s" => (
                &["skill"],
                &[
                    ("l", "list", ""),
                    ("s", "show", "<id>"),
                    ("i", "install", "<id>"),
                    ("r", "remove", "<id>"),
                ],
            ),
            _ => (
                &["template"],
                &[
                    ("l", "list", ""),
                    ("s", "show", "<id>"),
                    ("c", "copy", "<id> [path]"),
                ],
            ),
        };
        let choices = view::ChoiceList {
            title: "Resource Operation".into(),
            choices: operations
                .iter()
                .map(|(key, operation, operands)| {
                    view::KeyChoice::new(*key, format!("{operation} {operands}"))
                })
                .collect(),
        };
        let Some(operation_key) = self.prompt_choice_dialog(runtime, choices)? else {
            return Ok(());
        };
        let (_, operation, operands) = operations
            .iter()
            .find(|(key, _, _)| *key == operation_key)
            .ok_or_else(|| "unknown management operation".to_string())?;
        let remainder = if operands.is_empty() {
            String::new()
        } else {
            let Some(value) = self.prompt_line_dialog(
                runtime,
                "Operation Arguments",
                &format!("{operation} {operands}: "),
                "",
            )?
            else {
                return Ok(());
            };
            value
        };
        let arguments = management_arguments(prefix, operation, &remainder);
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let output = runtime.suspend_for(|| {
            Command::new(executable)
                .arg("--repo")
                .arg(&self.repo.root)
                .args(arguments)
                .output()
                .map_err(|error| format!("run management operation: {error}"))
        })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let body = if output.status.success() {
            stdout
        } else {
            stderr
        };
        let lines = if body.trim().is_empty() {
            vec![view::DialogLine {
                text: "No resources found.".into(),
                attention: false,
            }]
        } else {
            body.lines()
                .map(|line| view::DialogLine {
                    text: line.to_string(),
                    attention: !output.status.success(),
                })
                .collect()
        };
        self.notice_dialog(runtime, "Workflow Management", lines)
    }

    pub(super) fn show_configuration_tree(
        &mut self,
        runtime: &mut TerminalRuntime,
    ) -> Result<(), String> {
        let choices = view::ChoiceList {
            title: "Configuration".into(),
            choices: [
                ("g", "global Prism settings"),
                ("r", "selected repository settings"),
                ("t", "tracked repositories and keybindings"),
                ("w", "Worktrunk configuration"),
                ("c", "worktree columns"),
                ("h", "Harness selection"),
            ]
            .into_iter()
            .map(|(key, label)| view::KeyChoice::new(key, label))
            .collect(),
        };
        match self.prompt_choice_dialog(runtime, choices)?.as_deref() {
            Some("g") => self.edit_user_config(runtime),
            Some("r") => self.edit_config(runtime),
            Some("t") => self.edit_repositories(runtime),
            Some("w") => self.edit_worktrunk_user_config(runtime),
            Some("c") => self.edit_worktree_columns(runtime),
            Some("h") => self.select_default_harness(runtime),
            _ => Ok(()),
        }
    }

    pub(super) fn workflow_context(&self) -> BTreeMap<String, ContextValue> {
        let mut context = BTreeMap::from([(
            "repository".into(),
            ContextValue {
                schema: "prism.repository/v1".into(),
                value: serde_json::json!({"root": self.repo.root}),
            },
        )]);
        let selected_worktree = (self.focused_panel == PanelFocus::Worktrees)
            .then(|| self.selected_worktree_index())
            .flatten()
            .and_then(|index| self.sessions.get(index));
        if let Some(session) = selected_worktree {
            context.insert(
                "worktree".into(),
                ContextValue {
                    schema: "prism.worktree-session/v1".into(),
                    value: serde_json::json!({"id": format!("{}:{}", self.repo.root.display(), session.path.display()), "revision": session.incarnation, "repository": self.repo.root, "path": session.path, "branch": session.branch}),
                },
            );
        }
        // The selected worktree's persisted association is available immediately at startup and
        // is more specific than the repository-wide selection populated by an asynchronous poll.
        let change_request = selected_worktree
            .and_then(|session| session.pr.summary())
            .and_then(|summary| summary.change_request_identity.as_ref())
            .or_else(|| self.selected_repo_pr_identity());
        if let Some(change_request) = change_request {
            context.insert(
                "change_request".into(),
                ContextValue {
                    schema: "prism.change-request/v1".into(),
                    value: serde_json::json!({"provider": change_request.provider().to_string(), "host": change_request.canonical_host(), "project": change_request.project_path(), "native_id": change_request.native_id()}),
                },
            );
        }
        context
    }
}

fn workflow_control_request(
    dashboard: &view::WorkflowDashboard,
    requested: &str,
) -> Result<WorkflowControlRequest, String> {
    let command = match (requested, dashboard.status.as_str()) {
        ("toggle", "paused") => "resume",
        ("toggle", _) => "pause",
        (command, _) => command,
    };
    let available = match command {
        "pause" => dashboard.can_pause,
        "resume" => dashboard.can_resume,
        "cancel" => dashboard.can_cancel,
        "retry" => dashboard.can_retry,
        "skip" => dashboard.current_step_skippable,
        "restart" => dashboard.can_retry,
        _ => false,
    };
    if !available {
        return Err(format!(
            "{command} is unavailable for the selected Workflow Run"
        ));
    }
    let step = if matches!(command, "restart" | "skip") {
        Some(
            dashboard
                .current_step
                .clone()
                .ok_or_else(|| "the selected Workflow Run has no current Step".to_string())?,
        )
    } else {
        None
    };
    Ok(WorkflowControlRequest {
        command: command.into(),
        run_id: dashboard.run_id.clone(),
        step,
    })
}

fn workflow_launch_snapshot(
    executable: &std::path::Path,
    repository: &std::path::Path,
    id: &str,
) -> Result<DefinitionSnapshot, String> {
    let output = Command::new(executable)
        .arg("--repo")
        .arg(repository)
        .args(["workflow", "preview", id, "--json"])
        .output()
        .map_err(|error| format!("inspect Workflow inputs: {error}"))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if message.is_empty() {
            format!("Workflow preview exited with {}", output.status)
        } else {
            message
        });
    }
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("read Workflow preview: {error}"))?;
    serde_json::from_value(
        envelope
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )
    .map_err(|error| format!("read Workflow input schemas: {error}"))
}

fn workflow_launch_arguments(
    repository: &std::path::Path,
    id: &str,
    inputs: &BTreeMap<String, serde_json::Value>,
) -> Result<Vec<String>, String> {
    let mut arguments = vec![
        "--repo".into(),
        repository.to_string_lossy().into_owned(),
        "workflow".into(),
        "run".into(),
        id.into(),
    ];
    for (name, value) in inputs {
        arguments.push("--input".into());
        arguments.push(format!(
            "{name}={}",
            serde_json::to_string(value).map_err(|error| error.to_string())?
        ));
    }
    Ok(arguments)
}

fn management_arguments(prefix: &[&str], operation: &str, remainder: &str) -> Vec<String> {
    prefix
        .iter()
        .copied()
        .chain(std::iter::once(operation))
        .map(str::to_string)
        .chain(remainder.split_whitespace().map(str::to_string))
        .collect()
}

pub(super) struct ContextValue {
    schema: String,
    value: serde_json::Value,
}

fn manual_workflow_candidates(
    global_root: &std::path::Path,
    repository_root: &std::path::Path,
    context: &BTreeMap<String, ContextValue>,
) -> Result<Vec<LaunchCandidate>, String> {
    discover(global_root, Some(repository_root))
        .map_err(|error| error.to_string())
        .map(|resources| {
            resources
                .into_iter()
                .filter(|resource| resource.kind == ResourceKind::Workflow)
                .filter_map(|resource| {
                    let source = std::fs::read_to_string(resource.path).ok()?;
                    let definition = WorkflowDefinition::parse(&source).ok()?;
                    definition
                        .launch
                        .contains(&LaunchMode::Manual)
                        .then_some(LaunchCandidate {
                            definition,
                            scope: match resource.scope {
                                crate::resource::ResourceScope::Global => "global".into(),
                                crate::resource::ResourceScope::Repository => "repository".into(),
                            },
                        })
                })
                .filter(|candidate| context_compatible(&candidate.definition, context))
                .collect()
        })
}

fn context_compatible(
    definition: &WorkflowDefinition,
    context: &BTreeMap<String, ContextValue>,
) -> bool {
    definition.inputs.values().all(|input| {
        !input.from_context
            || !input.required
            || context
                .values()
                .filter(|value| value.schema == input.schema)
                .count()
                == 1
    })
}

fn context_inputs(
    definition: &WorkflowDefinition,
    context: &BTreeMap<String, ContextValue>,
) -> BTreeMap<String, serde_json::Value> {
    definition
        .inputs
        .iter()
        .filter_map(|(name, input)| {
            input.from_context.then(|| {
                context
                    .values()
                    .find(|value| value.schema == input.schema)
                    .map(|value| (name.clone(), value.value.clone()))
            })?
        })
        .collect()
}

fn select_with_fzf(fzf: &str, candidates: &[LaunchCandidate]) -> Result<Option<String>, String> {
    let mut child = Command::new(fzf)
        .args([
            "--delimiter=\t",
            "--prompt=Workflow> ",
            "--with-nth=2..5",
            "--header=Name  Description  Tags  Implementations",
            "--preview=printf 'scope: %s\\nrequired inputs: %s\\n' {6} {7}",
            "--preview-window=down,3,wrap",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start fzf '{fzf}': {error}"))?;
    {
        let input = child.stdin.as_mut().expect("fzf stdin is piped");
        for candidate in candidates {
            let definition = &candidate.definition;
            let implementations = definition
                .steps
                .iter()
                .filter_map(|step| step.implementation.as_deref().or(step.workflow.as_deref()))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(",");
            let required_inputs = definition
                .inputs
                .iter()
                .filter(|(_, input)| input.required)
                .map(|(name, input)| format!("{name}:{}", input.schema))
                .collect::<Vec<_>>()
                .join(",");
            writeln!(
                input,
                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                definition.id,
                definition.name.replace('\t', " "),
                definition.description.replace(['\t', '\n'], " "),
                definition.tags.join(","),
                implementations,
                candidate.scope,
                required_inputs
            )
            .map_err(|error| format!("write fzf choices: {error}"))?;
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for fzf: {error}"))?;
    if output.status.code() == Some(130) || output.status.code() == Some(1) {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(format!("fzf exited with {}", output.status));
    }
    let selected = String::from_utf8(output.stdout)
        .map_err(|error| format!("fzf returned invalid UTF-8: {error}"))?;
    Ok(selected
        .split_once('\t')
        .map(|(id, _)| id.trim().to_string())
        .filter(|id| !id.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_rediscovers_loose_workflows_while_process_is_running() {
        let root = std::env::temp_dir().join(format!(
            "prism-hot-workflow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let global = root.join("global");
        let repository = root.join("repository/.prism");
        let context = BTreeMap::new();

        assert!(
            manual_workflow_candidates(&global, &repository, &context)
                .unwrap()
                .is_empty()
        );

        std::fs::create_dir_all(global.join("workflows")).unwrap();
        let workflow = global.join("workflows/hot.toml");
        std::fs::write(
            &workflow,
            "schema_version=2\nid='acme.test/hot'\nname='hot'\nlaunch=['manual']\n[[steps]]\nid='run'\nclass='action'\nuse='acme.test/run'\n",
        )
        .unwrap();
        assert_eq!(
            manual_workflow_candidates(&global, &repository, &context).unwrap()[0]
                .definition
                .id,
            "acme.test/hot"
        );

        std::fs::write(
            &workflow,
            "schema_version=2\nid='acme.test/hot'\nname='hot'\nlaunch=['trigger']\n[[steps]]\nid='run'\nclass='action'\nuse='acme.test/run'\n",
        )
        .unwrap();
        assert!(
            manual_workflow_candidates(&global, &repository, &context)
                .unwrap()
                .is_empty()
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn context_filter_requires_exact_schema_match() {
        let definition = WorkflowDefinition::parse(
            "schema_version=2\nid='acme.test/run'\nname='run'\nlaunch=['manual']\n[inputs.worktree]\ntype='prism.worktree-session/v1'\nrequired=true\nfrom_context=true\n[[steps]]\nid='run'\nclass='action'\nuse='acme.test/run'",
        )
        .unwrap();
        assert!(!context_compatible(&definition, &BTreeMap::new()));
        let context = BTreeMap::from([(
            "worktree".into(),
            ContextValue {
                schema: "prism.worktree-session/v1".into(),
                value: serde_json::json!({}),
            },
        )]);
        assert!(context_compatible(&definition, &context));
    }

    #[test]
    fn workflow_control_decisions_do_not_require_a_terminal() {
        let dashboard = view::WorkflowDashboard {
            run_id: "run-1".into(),
            status: "paused".into(),
            current_step: Some("verify".into()),
            completed_steps: 1,
            total_steps: 3,
            parent_run_id: None,
            children: Vec::new(),
            detail: None,
            can_pause: false,
            can_resume: true,
            can_cancel: true,
            can_retry: true,
            current_step_skippable: true,
        };
        assert_eq!(
            workflow_control_request(&dashboard, "toggle")
                .unwrap()
                .command,
            "resume"
        );
        assert_eq!(
            workflow_control_request(&dashboard, "restart")
                .unwrap()
                .step,
            Some("verify".into())
        );
    }

    #[test]
    fn command_builders_do_not_require_a_terminal() {
        assert_eq!(
            workflow_launch_arguments(
                std::path::Path::new("/repo"),
                "acme/run",
                &BTreeMap::from([("count".into(), serde_json::json!(2))]),
            )
            .unwrap(),
            [
                "--repo", "/repo", "workflow", "run", "acme/run", "--input", "count=2"
            ]
        );
        assert_eq!(
            management_arguments(&["workflow", "trigger"], "show", "nightly"),
            ["workflow", "trigger", "show", "nightly"]
        );
    }
}
