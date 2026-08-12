use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEventKind};

use crate::tui_runtime::{RuntimeEvent, TerminalRuntime};
use crate::view;

use super::Tui;

#[derive(Debug, PartialEq, Eq)]
struct WorkflowControlRequest {
    command: String,
    run_id: String,
}

impl Tui {
    pub(super) fn create_ai_workflow(
        &mut self,
        runtime: &mut TerminalRuntime,
    ) -> Result<(), String> {
        let Some(session_index) = self.selected_worktree_index() else {
            return self
                .show_message("select a Worktree Session before creating a one-off Workflow");
        };
        let session = self
            .sessions
            .get(session_index)
            .ok_or_else(|| "selected Worktree Session disappeared".to_string())?;
        let managed = self
            .repos
            .get(session.repo_index)
            .ok_or_else(|| "selected repository disappeared".to_string())?;
        let repository = managed.repo.clone();
        let config = managed.config.clone();
        let worktree = session.path.clone();
        let incarnation = session.incarnation.clone();
        let draft_path = crate::workflow::ai::draft_path(&repository, &worktree, &incarnation);
        let previous = crate::workflow::ai::load_draft(&draft_path)?;
        let initial = previous
            .as_ref()
            .map(|draft| draft.description.as_str())
            .unwrap_or_default();
        let Some(mut description) = self.prompt_line_dialog(
            runtime,
            "AI One-off Workflow",
            "Describe the serial and parallel work: ",
            initial,
        )?
        else {
            return Ok(());
        };
        if description.trim().is_empty() {
            return self.show_message("one-off Workflow description cannot be empty");
        }

        let mut draft = previous.filter(|draft| draft.description == description);
        let mut validation_error = None::<String>;
        loop {
            if draft.is_none() {
                self.dialog = Some(view::DialogModel::Progress {
                    title: "AI One-off Workflow".into(),
                    message: "Generating and validating Workflow TOML…".into(),
                });
                self.draw(runtime)?;
                let cancellation = crate::AgentCancellation::default();
                let worker_cancellation = cancellation.clone();
                let worker_repository = repository.clone();
                let worker_config = config.clone();
                let worker_worktree = worktree.clone();
                let worker_description = description.clone();
                let worker_error = validation_error.clone();
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                std::thread::spawn(move || {
                    let result =
                        crate::async_runtime::block_on(crate::workflow::ai::generate_source(
                            &worker_repository,
                            &worker_config,
                            &worker_worktree,
                            &worker_description,
                            worker_error.as_deref(),
                            worker_cancellation,
                        ))
                        .map_err(|error| error.to_string())
                        .and_then(|result| result);
                    let _ = tx.send(result);
                });
                let mut cancelled = false;
                let source = loop {
                    match rx.try_recv() {
                        Ok(result) => break result,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            break Err("AI Workflow generator stopped unexpectedly".into());
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    }
                    if self.tick_tui_action_jobs().any() {
                        self.draw(runtime)?;
                    }
                    if let Some(RuntimeEvent::Key(event)) =
                        runtime.poll_event(Duration::from_millis(100))?
                        && event.kind == KeyEventKind::Press
                        && event.code == KeyCode::Esc
                    {
                        cancelled = true;
                        cancellation.cancel();
                        self.dialog = Some(view::DialogModel::Progress {
                            title: "AI One-off Workflow".into(),
                            message: "Cancelling generator…".into(),
                        });
                        self.draw(runtime)?;
                    }
                };
                self.dialog = None;
                self.draw(runtime)?;
                if cancelled {
                    return Ok(());
                }
                let source = source?;
                let now = crate::workflow_now_unix_ms();
                draft = Some(crate::workflow::ai::OneOffWorkflowDraft {
                    name: format!("ai-one-off-{now}"),
                    description: description.clone(),
                    source,
                    worktree: worktree.clone(),
                    worktree_incarnation: incarnation.clone(),
                    updated_unix_ms: now,
                });
            }

            let current = draft.as_ref().expect("draft was generated");
            let triggers = crate::StepTriggerCatalog::builtins();
            let compiled =
                crate::workflow::ai::compile_generated(&current.name, &current.source, &triggers);
            validation_error = compiled.as_ref().err().cloned();
            crate::workflow::ai::save_draft(&draft_path, current)?;
            let mut lines = vec![view::DialogLine {
                text: "Experimental: parallel Steps run concurrently in the same worktree and may edit overlapping files.".into(),
                attention: true,
            }];
            if let Some(error) = &validation_error {
                lines.push(view::DialogLine {
                    text: format!("Validation failed: {error}"),
                    attention: true,
                });
            }
            lines.extend(current.source.lines().map(|line| view::DialogLine {
                text: line.to_string(),
                attention: false,
            }));
            self.notice_dialog(runtime, "Generated Workflow Preview", lines)?;
            let choices = view::ChoiceList {
                title: "AI One-off Workflow".into(),
                choices: vec![
                    view::KeyChoice::new("r", "run"),
                    view::KeyChoice::new("e", "edit TOML"),
                    view::KeyChoice::new("g", "regenerate"),
                    view::KeyChoice::new("d", "edit description"),
                    view::KeyChoice::new("c", "cancel (draft is retained)"),
                ],
            };
            match self.prompt_choice_dialog(runtime, choices)?.as_deref() {
                Some("r") => {
                    let mut workflow = compiled?;
                    crate::resolve_workflow_agent_selection(&mut workflow, &config)
                        .map_err(format_diagnostics)?;
                    let now = crate::workflow_now_unix_ms();
                    let run_id = format!(
                        "run-{:016x}-{now}",
                        crate::util::stable_hash(std::path::Path::new(&format!(
                            "{}:{}:{now}",
                            workflow.digest,
                            worktree.display()
                        )))
                    );
                    let subject = crate::TriggerSubject {
                        repository: repository.root.clone(),
                        worktree: worktree.clone(),
                        change_request: None,
                        change_request_head: None,
                    };
                    let launched =
                        crate::worker::launch_prompt_workflow(&workflow, &run_id, &subject)?;
                    return self.show_message(&format!("launched one-off Workflow {launched}"));
                }
                Some("e") => {
                    let current = draft.as_mut().expect("draft exists");
                    edit_draft_source(runtime, &draft_path, &mut current.source)?;
                    current.updated_unix_ms = crate::workflow_now_unix_ms();
                    validation_error = None;
                }
                Some("g") => {
                    draft = None;
                }
                Some("d") => {
                    let Some(updated) = self.prompt_line_dialog(
                        runtime,
                        "AI One-off Workflow",
                        "Describe the serial and parallel work: ",
                        &description,
                    )?
                    else {
                        continue;
                    };
                    description = updated;
                    draft = None;
                    validation_error = None;
                }
                _ => return Ok(()),
            }
        }
    }

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
        if request.command == "cancel"
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
                .args(["workflow", &request.command, &request.run_id])
                .status()
                .map_err(|error| format!("run Workflow control: {error}"))
        })?;
        if !status.success() {
            return Err(format!("Workflow control exited with {status}"));
        }
        self.show_message(&format!(
            "{} requested for {}",
            request.command, request.run_id
        ))?;
        Ok(true)
    }

    /// Open one flat, hot-discovered Workflow picker. Enter runs and Ctrl-E edits.
    pub(super) fn launch_workflow(&mut self, runtime: &mut TerminalRuntime) -> Result<(), String> {
        let global = crate::util::prism_config_dir();
        let local = self.repo.root.join(".prism");
        let repository_trusted =
            crate::repository_resources_are_trusted(&global, &self.repo.root, &local)
                .map_err(|error| error.to_string())?;
        let catalog = runtime.suspend_for(|| {
            crate::PromptWorkflowCatalog::discover(&global, Some(&local), repository_trusted)
                .map_err(format_diagnostics)
        })?;
        let candidates = catalog.list();
        if candidates.is_empty() {
            if !repository_trusted
                && crate::repository_resource_revision(&local)
                    .map_err(|error| error.to_string())?
                    .is_some()
            {
                return self.show_message(
                    "repository Workflow resources are untrusted; preview `prism workflow trust-repository` before applying trust",
                );
            }
            return self.show_message("no Workflow is available for the selected worktree");
        }
        let fzf = self.config.tool("fzf");
        let selected = runtime.suspend_for(|| select_prompt_workflow(&fzf, &candidates))?;
        let Some((action, name)) = selected else {
            return Ok(());
        };
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let worktree = self
            .selected_worktree_index()
            .and_then(|index| self.sessions.get(index))
            .map(|session| session.path.clone())
            .unwrap_or_else(|| self.repo.root.clone());
        let input_values = if action == "run" {
            let workflow = catalog
                .get(&name)
                .ok_or_else(|| format!("selected Workflow '{name}' disappeared"))?;
            let Some(values) =
                self.prompt_workflow_input_form(runtime, workflow, &worktree, &fzf)?
            else {
                return Ok(());
            };
            values
        } else {
            Default::default()
        };
        let status = runtime.suspend_for(|| {
            let mut command = Command::new(executable);
            command
                .arg("--repo")
                .arg(&self.repo.root)
                .args(["workflow", &action, &name]);
            if action == "run" {
                command.arg("--worktree").arg(&worktree);
                for (input_name, value) in &input_values {
                    command.arg("--input").arg(format!("{input_name}={value}"));
                }
            }
            command
                .status()
                .map_err(|error| format!("{action} Workflow: {error}"))
        })?;
        if !status.success() {
            return Err(format!("Workflow {action} exited with {status}"));
        }
        self.show_message(&format!("{action} {name}"))
    }

    pub(super) fn launch_stabilization_workflow(
        &mut self,
        runtime: &mut TerminalRuntime,
    ) -> Result<(), String> {
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let session = self
            .selected_worktree_index()
            .and_then(|index| self.sessions.get(index));
        let worktree = session
            .map(|session| session.path.clone())
            .unwrap_or_else(|| self.repo.root.clone());
        let change_request = session
            .and_then(|session| session.pr.summary())
            .and_then(workflow_change_request_arguments);
        let status = runtime.suspend_for(|| {
            let mut command = Command::new(executable);
            command
                .arg("--repo")
                .arg(&self.repo.root)
                .args(["workflow", "run", "stabilize", "--worktree"])
                .arg(worktree);
            if let Some((identity, head)) = change_request {
                command
                    .args(["--change-request", &identity])
                    .args(["--change-request-head", &head]);
            }
            command
                .status()
                .map_err(|error| format!("launch stabilization Workflow: {error}"))
        })?;
        if status.success() {
            self.show_message("launched stabilize")
        } else {
            Err(format!("workflow command exited with {status}"))
        }
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
}

fn edit_draft_source(
    runtime: &mut TerminalRuntime,
    draft_path: &std::path::Path,
    source: &mut String,
) -> Result<(), String> {
    let editor = crate::terminal::editor_argv_from_env()?
        .ok_or_else(|| "no editor found; set VISUAL or EDITOR".to_string())?;
    let source_path = draft_path.with_extension("toml");
    std::fs::write(&source_path, source.as_bytes())
        .map_err(|error| format!("write editable Workflow draft: {error}"))?;
    let result = runtime
        .suspend_for(|| {
            Command::new(&editor[0])
                .args(&editor[1..])
                .arg(&source_path)
                .status()
                .map_err(|error| format!("edit one-off Workflow: {error}"))
        })
        .and_then(|status| {
            if !status.success() {
                return Err(format!("editor exited with {status}"));
            }
            *source = std::fs::read_to_string(&source_path)
                .map_err(|error| format!("read edited Workflow draft: {error}"))?;
            Ok(())
        });
    let _ = std::fs::remove_file(source_path);
    result
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
        _ => false,
    };
    if !available {
        let message = match (command, dashboard.status.as_str()) {
            ("cancel", "failed") => "can't cancel a failed Workflow Run".to_string(),
            ("cancel", "done") => "can't cancel a completed Workflow Run".to_string(),
            ("cancel", "aborted") => "can't cancel an already cancelled Workflow Run".to_string(),
            _ => format!("{command} is unavailable for the selected Workflow Run"),
        };
        return Err(message);
    }
    Ok(WorkflowControlRequest {
        command: command.into(),
        run_id: dashboard.run_id.clone(),
    })
}

fn select_prompt_workflow(
    fzf: &str,
    candidates: &[crate::DiscoveredWorkflow],
) -> Result<Option<(String, String)>, String> {
    let mut child = Command::new(fzf)
        .args([
            "--delimiter=\t",
            "--prompt=Workflow> ",
            "--with-nth=1..",
            "--header=Enter: run · Ctrl-E: edit",
            "--expect=enter,ctrl-e",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start fzf '{fzf}': {error}"))?;
    {
        let input = child.stdin.as_mut().expect("fzf stdin is piped");
        for candidate in candidates {
            writeln!(
                input,
                "{}\t{:?}\t{}",
                candidate.name,
                candidate.scope,
                candidate.path.display()
            )
            .map_err(|error| format!("write Workflow choices: {error}"))?;
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for fzf: {error}"))?;
    if output
        .status
        .code()
        .is_some_and(|code| matches!(code, 1 | 130))
    {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(format!("fzf exited with {}", output.status));
    }
    let output = String::from_utf8(output.stdout)
        .map_err(|error| format!("fzf returned invalid UTF-8: {error}"))?;
    let mut lines = output.lines();
    let key = lines.next().unwrap_or("enter");
    let name = lines
        .next()
        .and_then(|line| line.split('\t').next())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "fzf returned no Workflow name".to_string())?;
    Ok(Some((
        if key == "ctrl-e" { "edit" } else { "run" }.into(),
        name.into(),
    )))
}

fn workflow_change_request_arguments(
    summary: &crate::remote::PrSummary,
) -> Option<(String, String)> {
    let identity = summary.change_request_identity.as_ref()?;
    if summary.head_sha.trim().is_empty() {
        return None;
    }
    Some((
        format!(
            "{}:{}:{}:change_request:{}",
            identity.provider().config_label(),
            identity.canonical_host(),
            identity.project_path(),
            identity.native_id()
        ),
        summary.head_sha.clone(),
    ))
}

fn format_diagnostics(diagnostics: Vec<crate::WorkflowDiagnostic>) -> String {
    diagnostics
        .into_iter()
        .map(|item| format!("{}: {}", item.path.display(), item.message))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dashboard(status: &str) -> view::WorkflowDashboard {
        view::WorkflowDashboard {
            run_id: "run-1".into(),
            status: status.into(),
            current_step: Some("verify".into()),
            selected_step: Some("verify".into()),
            completed_steps: 1,
            total_steps: 3,
            run_position: 1,
            run_count: 1,
            detail: None,
            can_pause: status != "paused",
            can_resume: status == "paused",
            can_cancel: !matches!(status, "done" | "failed" | "aborted"),
            can_retry: status == "failed",
        }
    }

    #[test]
    fn stabilization_subject_uses_selected_session_change_request() {
        let summary = crate::remote::PrSummary {
            number: 42,
            change_request_identity: Some(crate::remote::test_change_request_identity()),
            native_state_evidence: Default::default(),
            title: String::new(),
            author: String::new(),
            body: String::new(),
            url: String::new(),
            state: "OPEN".into(),
            review_decision: String::new(),
            requested_reviewers: Vec::new(),
            head_ref: "feature".into(),
            base_ref: "main".into(),
            head_sha: "abc123".into(),
            updated_at: String::new(),
            check_status: String::new(),
            merge_state_status: String::new(),
            queue_state: String::new(),
            comment_count: 0,
            merged: false,
            draft: false,
        };

        let (identity, head) = workflow_change_request_arguments(&summary).unwrap();
        assert_eq!(
            identity,
            "github:github.com:example/repo:change_request:PR_test"
        );
        assert_eq!(head, "abc123");
    }

    #[test]
    fn workflow_control_is_flat_and_contextual() {
        assert_eq!(
            workflow_control_request(&dashboard("paused"), "toggle")
                .unwrap()
                .command,
            "resume"
        );
        assert_eq!(
            workflow_control_request(&dashboard("failed"), "retry")
                .unwrap()
                .command,
            "retry"
        );
        assert!(workflow_control_request(&dashboard("running"), "skip").is_err());
    }

    #[test]
    fn terminal_cancel_messages_are_specific() {
        assert_eq!(
            workflow_control_request(&dashboard("failed"), "cancel").unwrap_err(),
            "can't cancel a failed Workflow Run"
        );
        assert_eq!(
            workflow_control_request(&dashboard("done"), "cancel").unwrap_err(),
            "can't cancel a completed Workflow Run"
        );
    }
}
