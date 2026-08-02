use std::path::PathBuf;

use crate::auto_flow::AutoRunStatus;
use crate::execution::{DispatchState, WorkflowKind};
use crate::plan_run::{PlanRunStatus, PlanStepStatus};
use crate::view::WorktreeMainView;
use crate::workspace_state::{
    AvailableControls, DispatchSnapshot, Progress, RepositorySnapshot, RepositoryTotals,
    WorkflowIdentity, WorkflowLifecycle, WorkflowSnapshot, WorktreeIdentity,
};

use super::super::PanelFocus;
use super::support::{test_auto_run, test_plan_run, test_plan_run_with_steps, test_tui};

#[test]
fn status_auto_dashboard_uses_selected_run() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Status;
    tui.remember_auto_run(test_auto_run("run-a", "/repo-one/a-worktree", 10));
    tui.remember_auto_run(test_auto_run("run-b", "/repo-one/z-worktree", 20));
    tui.selected_auto_run = Some("run-b".to_string());

    let dashboard = tui.current_auto_dashboard().unwrap();

    assert_eq!(dashboard.run.run.id, "run-b");
    assert_eq!(
        dashboard.run.run.worktree_path,
        PathBuf::from("/repo-one/z-worktree")
    );
}

#[test]
fn terminal_auto_run_remains_in_history_but_is_no_longer_active() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Worktrees;
    tui.select_worktree(1);
    let mut run = test_auto_run("run", "/repo-one/feature-one", 10);
    run.run.variant = "repair".to_string();

    tui.remember_auto_run(run.clone());
    assert_eq!(
        tui.active_auto_runs.get(&run.run.worktree_path),
        Some(&run.run.id)
    );

    run.run.status = AutoRunStatus::Aborted;
    tui.remember_auto_run(run.clone());

    assert!(!tui.active_auto_runs.contains_key(&run.run.worktree_path));
    assert_eq!(tui.auto_runs.get(&run.run.id), Some(&run));
    assert_eq!(tui.current_auto_dashboard().unwrap().run.run.id, run.run.id);

    tui.remember_plan_run(test_plan_run("plan", "/repo-one/feature-one"));
    assert!(tui.current_auto_dashboard().is_none());
    assert_eq!(tui.current_plan_dashboard().unwrap().run.run.id, "plan");
}

#[test]
fn standalone_plan_dashboard_is_hidden_outside_worktrees() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Status;
    tui.remember_plan_run(test_plan_run("plan", "/repo-one"));

    assert!(tui.current_plan_dashboard().is_none());

    tui.focused_panel = PanelFocus::Repos;

    assert!(tui.current_plan_dashboard().is_none());
}

#[test]
fn plan_step_selection_follows_persisted_active_step_until_manual_navigation() {
    let mut tui = test_tui();
    let mut run = test_plan_run_with_steps("plan", "/repo-one/feature-one", 1);

    tui.remember_plan_run(run.clone());
    assert_eq!(tui.selected_plan_step_by_run.get("plan"), Some(&1));

    run.run.selected_step = 2;
    run.steps[0].status = PlanStepStatus::Done;
    run.steps[0].finished_unix_ms = Some(20);
    run.steps[1].status = PlanStepStatus::Running;
    run.steps[1].started_unix_ms = Some(30);
    tui.remember_plan_run(run.clone());
    assert_eq!(tui.selected_plan_step_by_run.get("plan"), Some(&2));

    tui.focused_panel = PanelFocus::Worktrees;
    tui.select_worktree(1);
    tui.worktree_main_view = WorktreeMainView::Plan;
    tui.move_plan_step_selection(-1);
    assert_eq!(tui.selected_plan_step_by_run.get("plan"), Some(&1));

    run.run.selected_step = 3;
    run.steps[1].status = PlanStepStatus::Done;
    run.steps[1].finished_unix_ms = Some(40);
    run.steps[2].status = PlanStepStatus::Running;
    run.steps[2].started_unix_ms = Some(50);
    tui.remember_plan_run(run);
    assert_eq!(tui.selected_plan_step_by_run.get("plan"), Some(&1));
}

#[test]
fn plan_step_selection_prefers_latest_finished_step_after_completion() {
    let mut tui = test_tui();
    let mut run = test_plan_run_with_steps("plan", "/repo-one", 1);
    run.run.status = PlanRunStatus::Done;
    run.run.selected_step = 1;
    for (index, step) in run.steps.iter_mut().enumerate() {
        step.status = PlanStepStatus::Done;
        step.finished_unix_ms = Some(10 + index as u64);
    }

    tui.remember_plan_run(run);

    assert_eq!(tui.selected_plan_step_by_run.get("plan"), Some(&3));
}

#[test]
fn workflow_controls_come_from_snapshot_without_replacing_tui_selection() {
    let mut tui = test_tui();
    tui.selected_auto_run = Some("selected-run".to_string());
    let repository = tui.repos[0].identity.clone();
    tui.workspace_repositories.insert(
        repository,
        RepositorySnapshot {
            root: PathBuf::from("/repo-one"),
            label: "repo-one".to_string(),
            shortcut: Some('1'),
            worktrees: Vec::new(),
            workflows: vec![WorkflowSnapshot {
                identity: WorkflowIdentity {
                    repository: PathBuf::from("/repo-one"),
                    kind: WorkflowKind::Auto,
                    run_id: "snapshot-run".to_string(),
                    display_id: "a:snapshot".to_string(),
                },
                owner: None,
                worktree: WorktreeIdentity {
                    path: PathBuf::from("/repo-one/feature-one"),
                    display: "feature-one".to_string(),
                },
                lifecycle: WorkflowLifecycle::Paused,
                pause_requested: true,
                dispatch: DispatchSnapshot {
                    state: Some(DispatchState::Paused),
                    daemon_instance_id: None,
                    worker_id: None,
                    lease_expires_unix_ms: None,
                    heartbeat_unix_ms: None,
                    interruption_generation: 0,
                    updated_unix_ms: Some(20),
                },
                current_step: None,
                progress: Progress::default(),
                available_controls: AvailableControls {
                    resume: true,
                    ..AvailableControls::default()
                },
                updated_unix_ms: 20,
            }],
            totals: RepositoryTotals {
                workflows: 1,
                ..RepositoryTotals::default()
            },
        },
    );

    let controls = tui
        .workflow_controls(
            std::path::Path::new("/repo-one"),
            WorkflowKind::Auto,
            "snapshot-run",
        )
        .unwrap();

    assert!(controls.resume);
    assert!(!controls.pause);
    assert_eq!(tui.selected_auto_run.as_deref(), Some("selected-run"));

    let repository = tui.repos[1].identity.clone();
    tui.workspace_repositories.insert(
        repository,
        RepositorySnapshot {
            root: PathBuf::from("/repo-two"),
            label: "repo-two".to_string(),
            shortcut: Some('2'),
            worktrees: Vec::new(),
            workflows: vec![WorkflowSnapshot {
                identity: WorkflowIdentity {
                    repository: PathBuf::from("/repo-two"),
                    kind: WorkflowKind::Auto,
                    run_id: "snapshot-run".to_string(),
                    display_id: "a:snapshot".to_string(),
                },
                owner: None,
                worktree: WorktreeIdentity {
                    path: PathBuf::from("/repo-two/feature-two"),
                    display: "feature-two".to_string(),
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
                    updated_unix_ms: Some(30),
                },
                current_step: None,
                progress: Progress::default(),
                available_controls: AvailableControls {
                    pause: true,
                    ..AvailableControls::default()
                },
                updated_unix_ms: 30,
            }],
            totals: RepositoryTotals {
                workflows: 1,
                ..RepositoryTotals::default()
            },
        },
    );

    let repo_two_controls = tui
        .workflow_controls(
            std::path::Path::new("/repo-two"),
            WorkflowKind::Auto,
            "snapshot-run",
        )
        .unwrap();
    assert!(repo_two_controls.pause);
    assert!(!repo_two_controls.resume);
}

#[test]
fn worktree_snapshot_prefers_active_workflow_over_newer_history() {
    let mut tui = test_tui();
    let repository = tui.repos[0].identity.clone();
    let workflow = |run_id: &str, lifecycle, updated_unix_ms| WorkflowSnapshot {
        identity: WorkflowIdentity {
            repository: PathBuf::from("/repo-one"),
            kind: WorkflowKind::Plan,
            run_id: run_id.to_string(),
            display_id: format!("p:{run_id}"),
        },
        owner: None,
        worktree: WorktreeIdentity {
            path: PathBuf::from("/repo-one/feature-one"),
            display: "feature-one".to_string(),
        },
        lifecycle,
        pause_requested: false,
        dispatch: DispatchSnapshot {
            state: Some(if lifecycle.terminal() {
                DispatchState::Terminal
            } else {
                DispatchState::Queued
            }),
            daemon_instance_id: None,
            worker_id: None,
            lease_expires_unix_ms: None,
            heartbeat_unix_ms: None,
            interruption_generation: 0,
            updated_unix_ms: Some(updated_unix_ms),
        },
        current_step: None,
        progress: Progress::default(),
        available_controls: AvailableControls::default(),
        updated_unix_ms,
    };
    tui.workspace_repositories.insert(
        repository,
        RepositorySnapshot {
            root: PathBuf::from("/repo-one"),
            label: "repo-one".to_string(),
            shortcut: Some('1'),
            worktrees: Vec::new(),
            workflows: vec![
                workflow("done-newer", WorkflowLifecycle::Done, 30),
                workflow("queued-older", WorkflowLifecycle::Queued, 20),
            ],
            totals: RepositoryTotals::default(),
        },
    );

    let selected = tui
        .worktree_workflow_snapshot(
            std::path::Path::new("/repo-one"),
            std::path::Path::new("/repo-one/feature-one"),
            WorkflowKind::Plan,
        )
        .unwrap();

    assert_eq!(selected.identity.run_id, "queued-older");
}
