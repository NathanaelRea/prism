use std::io::Write as _;
use std::process::{Command, Stdio};

use crate::tui_runtime::TerminalRuntime;
use crate::view;

use super::Tui;

#[derive(Debug, PartialEq, Eq)]
struct WorkflowControlRequest {
    command: String,
    run_id: String,
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
