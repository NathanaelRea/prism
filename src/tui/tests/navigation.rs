use std::fs;
use std::path::PathBuf;

use crate::auto_flow::AutoRunStatus;
use crate::remote::PrCache;
use crate::view::{RepoMainView, WorktreeMainView};

use super::super::{OpenTmuxSessionTarget, PanelFocus, Tui, WorktreeListMode};
use super::support::{
    test_auto_run, test_plan_run, test_plan_run_with_steps, test_pr_summary, test_tui,
    unique_temp_dir,
};

#[test]
fn tui_defaults_to_repos_panel_focus() {
    let tui = test_tui();

    assert_eq!(tui.focused_panel, PanelFocus::Repos);
}

#[test]
fn switching_repos_does_not_change_worktree_selection_until_worktrees_focus() {
    let mut tui = test_tui();

    tui.select_worktree(1);
    tui.select_repo(1);

    assert_eq!(tui.selected, 1);

    tui.focus_worktrees();

    assert_eq!(tui.selected_worktree_index(), Some(3));
}

#[test]
fn repeated_worktree_focus_does_not_change_list_mode() {
    let mut tui = test_tui();
    tui.focus_worktrees();

    assert_eq!(tui.worktree_list_mode, WorktreeListMode::Repo);
    assert_eq!(tui.visible_session_indices(), vec![1]);

    tui.focus_worktrees();

    assert_eq!(tui.worktree_list_mode, WorktreeListMode::Repo);
    assert_eq!(tui.visible_session_indices(), vec![1]);
}

#[test]
fn switching_from_global_to_repo_mode_preserves_selected_worktree() {
    let mut tui = test_tui();
    tui.worktree_list_mode = WorktreeListMode::Global;
    tui.focus_worktrees();
    tui.select_worktree(1);
    tui.select_repo(1);
    tui.sessions[3].hidden = true;

    tui.switch_worktree_list_mode(WorktreeListMode::Repo);

    assert_eq!(tui.current_repo, 0);
    assert_eq!(tui.selected_worktree_index(), Some(1));
}

#[test]
fn persisted_worktree_list_mode_loads_and_updates_on_switch() {
    let temp = unique_temp_dir("prism-tui-ui-state-test");
    let path = temp.join("ui-state.toml");
    crate::ui_state::save_to_path(&path, WorktreeListMode::Global).unwrap();
    let mut tui = test_tui();

    tui.use_persisted_ui_state(path.clone()).unwrap();

    assert_eq!(tui.worktree_list_mode, WorktreeListMode::Global);

    tui.focus_worktrees();
    tui.switch_worktree_list_mode(WorktreeListMode::Repo);

    assert_eq!(tui.worktree_list_mode, WorktreeListMode::Repo);
    assert_eq!(
        crate::ui_state::load_from_path(&path).unwrap(),
        Some(WorktreeListMode::Repo)
    );

    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn invalid_persisted_ui_state_keeps_current_mode_and_reports_error() {
    let temp = unique_temp_dir("prism-tui-invalid-ui-state-test");
    fs::create_dir_all(&temp).unwrap();
    let path = temp.join("ui-state.toml");
    fs::write(&path, "worktree_list_mode = 42\n").unwrap();
    let mut tui = Tui::new(Vec::new(), 0, Vec::new());

    tui.use_persisted_ui_state(path.clone()).unwrap();

    assert_eq!(tui.worktree_list_mode, WorktreeListMode::Repo);
    assert!(
        tui.status_message
            .as_deref()
            .is_some_and(|message| message.contains(&path.display().to_string()))
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "worktree_list_mode = 42\n"
    );
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn worktree_filter_clear_restores_remembered_worktree() {
    let mut tui = test_tui();
    tui.select_worktree(1);

    tui.worktree_filter = "main".to_string();
    tui.restore_selected_worktree_for_repo();

    assert_eq!(tui.selected_worktree_index(), None);

    tui.worktree_filter.clear();
    tui.restore_selected_worktree_for_repo();

    assert_eq!(tui.selected_worktree_index(), Some(1));
}

#[test]
fn hidden_sessions_are_not_visible_in_normal_worktree_list() {
    let mut tui = test_tui();
    tui.sessions[1].hidden = true;
    tui.selected = 1;

    assert!(!tui.visible_session_indices().contains(&1));
    assert_eq!(tui.selected_worktree_index(), None);
}

#[test]
fn horizontal_keys_switch_repo_view_without_changing_focus() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Repos;

    tui.move_right();

    assert_eq!(tui.focused_panel, PanelFocus::Repos);
    assert_eq!(tui.repo_main_view, RepoMainView::ChangeRequests);

    tui.focus_main();
    tui.move_right();

    assert_eq!(tui.focused_panel, PanelFocus::Repos);
    assert_eq!(tui.repo_main_view, RepoMainView::Kanban);

    tui.move_left();

    assert_eq!(tui.focused_panel, PanelFocus::Repos);
    assert_eq!(tui.repo_main_view, RepoMainView::ChangeRequests);

    tui.focused_panel = PanelFocus::Worktrees;
    tui.main_focused = false;
    tui.move_left();

    assert_eq!(tui.focused_panel, PanelFocus::Worktrees);
    assert_eq!(tui.repo_main_view, RepoMainView::ChangeRequests);
}

#[test]
fn main_panel_scrolls_when_pr_comments_are_selectable() {
    let mut tui = test_tui();
    tui.focus_worktrees();
    tui.select_worktree(1);
    tui.sessions[1].pr = PrCache::observed(
        test_pr_summary(false),
        Some(crate::remote::PrDetails {
            comments: vec![
                crate::remote::PrComment {
                    body: "first comment".to_string(),
                    ..crate::remote::PrComment::default()
                },
                crate::remote::PrComment {
                    body: "second comment".to_string(),
                    ..crate::remote::PrComment::default()
                },
            ],
            ..crate::remote::PrDetails::default()
        }),
    );
    tui.focus_main();

    tui.move_down();

    assert_eq!(tui.main_scroll, 1);
    assert_eq!(tui.selected_comment, 1);

    tui.move_up();

    assert_eq!(tui.main_scroll, 0);
    assert_eq!(tui.selected_comment, 0);
}

#[test]
fn sidebar_navigation_leaves_main_focus() {
    let mut tui = test_tui();
    tui.focus_main();

    tui.focus_repos();
    assert!(!tui.main_focused);
    assert_eq!(tui.focused_panel, PanelFocus::Repos);

    tui.focus_main();
    tui.focus_next_panel();
    assert!(!tui.main_focused);
    assert_eq!(tui.focused_panel, PanelFocus::Worktrees);
}

#[test]
fn worktree_plan_dashboard_is_not_gated_by_horizontal_keys() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Worktrees;
    tui.select_worktree(1);
    tui.remember_plan_run(test_plan_run("plan", "/repo-one/feature-one"));

    assert_eq!(tui.worktree_main_view, WorktreeMainView::Details);
    assert!(tui.current_plan_dashboard().is_some());

    tui.move_left();

    assert_eq!(tui.focused_panel, PanelFocus::Worktrees);
    assert_eq!(tui.worktree_main_view, WorktreeMainView::Details);
    assert!(tui.current_plan_dashboard().is_some());

    tui.focus_main();
    tui.move_right();

    assert_eq!(tui.focused_panel, PanelFocus::Worktrees);
    assert_eq!(tui.worktree_main_view, WorktreeMainView::Details);
    assert!(tui.current_plan_dashboard().is_some());

    tui.move_left();

    assert_eq!(tui.focused_panel, PanelFocus::Worktrees);
    assert_eq!(tui.worktree_main_view, WorktreeMainView::Details);
    assert!(tui.current_plan_dashboard().is_some());
}

#[test]
fn plan_runs_for_same_worktree_keep_independent_selection_history() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Worktrees;
    tui.select_worktree(1);
    tui.worktree_main_view = WorktreeMainView::Plan;
    let mut first = test_plan_run("plan-a", "/repo-one/feature-one");
    first.run.updated_unix_ms = 10;
    let mut second = test_plan_run("plan-b", "/repo-one/feature-one");
    second.run.updated_unix_ms = 20;

    tui.remember_plan_run(first);
    tui.remember_plan_run(second);

    let dashboard = tui.current_plan_dashboard().unwrap();
    assert_eq!(dashboard.run.run.id, "plan-a");
    assert_eq!(dashboard.runs.len(), 2);

    assert!(tui.move_plan_run_selection(1));

    let dashboard = tui.current_plan_dashboard().unwrap();
    assert_eq!(dashboard.run.run.id, "plan-b");
    assert_eq!(dashboard.runs.iter().filter(|run| run.selected).count(), 1);
}

#[test]
fn open_tmux_session_target_blocks_status_enter() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Status;

    assert_eq!(
        tui.open_tmux_session_target(),
        OpenTmuxSessionTarget::Blocked("status has no Enter action")
    );
}

#[test]
fn open_tmux_session_target_blocks_status_enter_with_auto_run() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Status;
    tui.remember_auto_run(test_auto_run("auto", "/repo-one/feature-one", 20));

    assert_eq!(
        tui.open_tmux_session_target(),
        OpenTmuxSessionTarget::Blocked("status has no Enter action")
    );
}

#[test]
fn historical_auto_run_does_not_replace_active_worktree_owner() {
    let mut tui = test_tui();
    let active = test_auto_run("active", "/repo-one/feature-one", 20);
    let mut historical = test_auto_run("historical", "/repo-one/feature-one", 30);
    historical.run.status = AutoRunStatus::Failed;

    tui.remember_auto_run(active);
    tui.remember_auto_run(historical);

    assert_eq!(
        tui.active_auto_runs
            .get(std::path::Path::new("/repo-one/feature-one"))
            .map(String::as_str),
        Some("active")
    );
}

#[test]
fn permanent_delete_targets_worktree_even_with_active_auto_dashboard() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Worktrees;
    tui.select_worktree(1);
    tui.remember_auto_run(test_auto_run("active", "/repo-one/feature-one", 20));

    assert!(tui.current_auto_dashboard().is_some());
    assert!(tui.permanent_delete_targets_worktree());
}

#[test]
fn open_tmux_session_target_opens_repo_default_from_repos() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Repos;

    assert_eq!(
        tui.open_tmux_session_target(),
        OpenTmuxSessionTarget::RepoDefaultAgent(0)
    );
}

#[test]
fn open_tmux_session_target_ignores_worktree_filter_for_repo_default() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Repos;
    tui.worktree_filter = "missing".to_string();

    assert_eq!(
        tui.open_tmux_session_target(),
        OpenTmuxSessionTarget::RepoDefaultAgent(0)
    );
}

#[test]
fn open_tmux_session_target_opens_feature_worktree_agent() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Worktrees;
    tui.select_worktree(1);

    assert_eq!(
        tui.open_tmux_session_target(),
        OpenTmuxSessionTarget::WorktreeAgent
    );
}

#[test]
fn open_tmux_session_target_opens_selected_plan_phase_from_main() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Worktrees;
    tui.select_worktree(1);
    tui.focus_main();
    tui.remember_plan_run(test_plan_run_with_steps("plan", "/repo-one/feature-one", 1));

    assert_eq!(
        tui.open_tmux_session_target(),
        OpenTmuxSessionTarget::PlanPhaseAgent
    );
}

#[test]
fn open_tmux_session_target_blocks_default_branch_in_worktree_panel() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Worktrees;
    tui.select_worktree(0);

    assert_eq!(
        tui.open_tmux_session_target(),
        OpenTmuxSessionTarget::Blocked("selected repository has no visible worktrees")
    );
}

#[test]
fn selected_repo_identity_survives_repo_reordering() {
    let mut tui = test_tui();
    tui.select_repo(1);
    tui.repos.swap(0, 1);
    for session in &mut tui.sessions {
        session.repo_index = 1 - session.repo_index;
    }

    tui.ensure_navigation_valid();

    assert_eq!(tui.current_repo, 0);
    assert_eq!(
        tui.selected_repo_context().unwrap().repo.root,
        PathBuf::from("/repo-two")
    );
}
