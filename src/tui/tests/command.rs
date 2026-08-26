use std::path::PathBuf;

use crossterm::event::KeyCode;

use crate::remote::{PrCache, PrComment, PrDetails};
use crate::workspace_state::{
    AvailableControls, DispatchSnapshot, DispatchState, Progress, RepositorySnapshot,
    RepositoryTotals, StepState, StepSummary, WorkflowIdentity, WorkflowLifecycle,
    WorkflowSnapshot, WorktreeIdentity,
};
use crate::{
    PromptStepPhase, PromptWorkflowRunStatus, PromptWorkflowStepState, TriggerSubject,
    WorkflowRunState,
};

use super::super::PanelFocus;
use super::super::command::{CommandOutcome, CommandState, DashboardCommand};
use super::support::{ScriptedTerminal, test_pr_summary, test_tui};

#[test]
fn semantic_commands_dispatch_without_the_crossterm_adapter() {
    let mut tui = test_tui();
    let mut terminal = ScriptedTerminal::default();
    let mut state = CommandState::default();

    let outcome = tui
        .dispatch_command(&mut terminal, DashboardCommand::FocusRepos, &mut state)
        .unwrap();

    assert_eq!(outcome, CommandOutcome::Continue);
    assert_eq!(tui.focused_panel, PanelFocus::Repos);
    assert_eq!(terminal.draws, 0);
}

#[test]
fn command_state_is_retained_across_dispatches() {
    let mut tui = test_tui();
    let mut terminal = ScriptedTerminal::default();
    let mut state = CommandState::default();

    for command in [
        DashboardCommand::FocusWorktrees,
        DashboardCommand::PreviousView,
        DashboardCommand::Bottom,
    ] {
        tui.dispatch_command(&mut terminal, command, &mut state)
            .unwrap();
    }
    assert_eq!(tui.selected_worktree_index(), Some(3));

    tui.dispatch_command(&mut terminal, DashboardCommand::G, &mut state)
        .unwrap();
    assert_eq!(tui.selected_worktree_index(), Some(3));

    tui.dispatch_command(&mut terminal, DashboardCommand::G, &mut state)
        .unwrap();
    assert_eq!(tui.selected_worktree_index(), Some(1));
}

#[test]
fn interactive_commands_use_the_terminal_driver_seam() {
    let mut tui = test_tui();
    let mut terminal = ScriptedTerminal::default();
    terminal.push_key(KeyCode::Enter);
    let mut state = CommandState::default();

    let outcome = tui
        .dispatch_command(&mut terminal, DashboardCommand::Help, &mut state)
        .unwrap();

    assert_eq!(outcome, CommandOutcome::Continue);
    assert!(terminal.draws >= 2);
    assert!(tui.dialog.is_none());
}

#[test]
fn enter_on_workflow_stage_opens_workflow_details_before_comment_details() {
    let mut tui = test_tui();
    tui.focus_worktrees();
    tui.select_worktree(1);
    tui.focus_main();
    tui.sessions[1].pr = PrCache::observed(
        test_pr_summary(false),
        Some(PrDetails {
            comments: vec![PrComment {
                body: "comment must not win Enter routing".to_string(),
                ..PrComment::default()
            }],
            ..PrDetails::default()
        }),
    );
    let repository = PathBuf::from("/repo-one");
    let worktree = tui.sessions[1].path.clone();
    let run = WorkflowRunState {
        id: "run-1".to_string(),
        workflow_digest: "digest".to_string(),
        workflow_name: "stabilize".to_string(),
        subject: TriggerSubject {
            repository: repository.clone(),
            worktree: worktree.clone(),
            change_request: None,
            change_request_head: None,
        },
        status: PromptWorkflowRunStatus::Running,
        cycle: 1,
        cycle_started_unix_ms: 1,
        max_agent_runs: 10,
        agent_runs_consumed: 1,
        cancellation_requested: false,
        created_unix_ms: 1,
        updated_unix_ms: 2,
        revision: 1,
        steps: vec![PromptWorkflowStepState {
            key: "review".to_string(),
            dependencies: Vec::new(),
            explicit_dependencies: false,
            phase: PromptStepPhase::RunningAgent,
            summary: Some("reviewing changes".to_string()),
            wake_at_unix_ms: None,
            satisfied_cycle: None,
            unconditional_completed: false,
            attempts: Vec::new(),
        }],
        events: Vec::new(),
    };
    let snapshot = WorkflowSnapshot {
        identity: WorkflowIdentity {
            repository: repository.clone(),
            run_id: run.id.clone(),
            display_id: "wf-1".to_string(),
        },
        worktree: WorktreeIdentity {
            path: worktree,
            display: "feature-one".to_string(),
        },
        lifecycle: WorkflowLifecycle::Running,
        pause_requested: false,
        dispatch: DispatchSnapshot {
            state: Some(DispatchState::Claimed),
            daemon_instance_id: None,
            worker_id: None,
            lease_expires_unix_ms: None,
            heartbeat_unix_ms: None,
            interruption_generation: 0,
            updated_unix_ms: Some(2),
        },
        current_step: Some(StepSummary {
            label: "review".to_string(),
            state: StepState::Running,
        }),
        progress: Progress {
            completed: 0,
            total: 1,
        },
        available_controls: AvailableControls::default(),
        updated_unix_ms: 2,
    };
    tui.workspace_repositories.insert(
        tui.repos[0].identity.clone(),
        RepositorySnapshot {
            root: repository,
            label: "repo-one".to_string(),
            shortcut: Some('1'),
            worktrees: Vec::new(),
            workflows: vec![snapshot],
            workflow_details: vec![run],
            totals: RepositoryTotals::default(),
        },
    );
    let mut terminal = ScriptedTerminal::default();
    terminal.push_key(KeyCode::Enter);
    let mut state = CommandState::default();

    tui.dispatch_command(&mut terminal, DashboardCommand::OpenTmuxSession, &mut state)
        .unwrap();

    let dialog_titles = terminal
        .frames
        .iter()
        .filter_map(|frame| frame.dialog_title.as_deref())
        .collect::<Vec<_>>();
    assert!(dialog_titles.contains(&"Workflow Details"));
    assert!(!dialog_titles.contains(&"Comment Details"));
}

#[test]
fn suspended_operations_use_the_terminal_driver_seam() {
    let mut terminal = ScriptedTerminal::default();

    let value = crate::tui_runtime::suspend_for(&mut terminal, || Ok(42)).unwrap();

    assert_eq!(value, 42);
    assert_eq!(terminal.suspends, 1);
    assert_eq!(terminal.resumes, 1);
}
