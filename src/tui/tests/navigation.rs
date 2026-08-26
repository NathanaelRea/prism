use std::fs;
use std::path::PathBuf;

use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::remote::PrCache;
use crate::view::RepoMainView;

use super::super::{OpenTmuxSessionTarget, PanelFocus, Tui, WorktreeListMode};
use super::support::{test_pr_summary, test_tui, unique_temp_dir};

#[tokio::test(flavor = "multi_thread")]
async fn tui_defaults_to_repos_panel_focus() {
    let tui = test_tui();

    assert_eq!(tui.focused_panel, PanelFocus::Repos);
}

#[tokio::test(flavor = "multi_thread")]
async fn switching_repos_does_not_change_worktree_selection_until_worktrees_focus() {
    let mut tui = test_tui();

    tui.select_worktree(1);
    tui.select_repo(1);

    assert_eq!(tui.selected, 1);

    tui.focus_worktrees();

    assert_eq!(tui.selected_worktree_index(), Some(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_worktree_focus_does_not_change_list_mode() {
    let mut tui = test_tui();
    tui.focus_worktrees();

    assert_eq!(tui.worktree_list_mode, WorktreeListMode::Repo);
    assert_eq!(tui.visible_session_indices(), vec![1]);

    tui.focus_worktrees();

    assert_eq!(tui.worktree_list_mode, WorktreeListMode::Repo);
    assert_eq!(tui.visible_session_indices(), vec![1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_panel_separates_authoritative_merge_progress_from_active_worktrees() {
    let mut tui = test_tui();
    tui.worktree_list_mode = WorktreeListMode::Global;

    tui.sessions[1].pr = PrCache::observed(test_pr_summary(true), None);
    let mut queued = test_pr_summary(false);
    queued.queue_state = "queued".to_string();
    queued.check_status = "successful".to_string();
    tui.sessions[3].pr = PrCache::observed(queued, None);
    tui.focus_worktrees();

    assert!(tui.visible_worktree_indices().is_empty());
    assert_eq!(tui.visible_merge_indices(), vec![1, 3]);

    tui.focus_merges();
    assert_eq!(tui.focused_panel, PanelFocus::Merges);
    assert_eq!(tui.selected_worktree_index(), Some(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_merge_evidence_remains_in_active_worktrees() {
    let mut tui = test_tui();
    tui.worktree_list_mode = WorktreeListMode::Global;
    let mut queued = test_pr_summary(false);
    queued.queue_state = "queued".to_string();
    queued.check_status = "successful".to_string();
    tui.sessions[1].pr = PrCache::observed(queued, None);
    tui.sessions[1].pr.mark_preserved_stale();

    assert!(tui.sessions[1].pr.trusted_summary().is_err());
    assert!(tui.visible_worktree_indices().contains(&1));
    assert!(!tui.visible_merge_indices().contains(&1));
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_queued_merge_returns_to_active_worktrees() {
    let mut tui = test_tui();
    tui.worktree_list_mode = WorktreeListMode::Global;
    let mut failed = test_pr_summary(false);
    failed.queue_state = "queued".to_string();
    failed.check_status = "failed".to_string();
    tui.sessions[1].pr = PrCache::observed(failed, None);
    tui.focus_worktrees();

    assert!(tui.visible_worktree_indices().contains(&1));
    assert!(!tui.visible_merge_indices().contains(&1));
}

#[tokio::test(flavor = "multi_thread")]
async fn switching_from_global_to_repo_mode_preserves_selected_worktree() {
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

#[tokio::test(flavor = "multi_thread")]
async fn persisted_worktree_list_mode_loads_and_updates_on_switch() {
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

#[tokio::test(flavor = "multi_thread")]
async fn invalid_persisted_ui_state_keeps_current_mode_and_reports_error() {
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

#[tokio::test(flavor = "multi_thread")]
async fn worktree_filter_clear_restores_remembered_worktree() {
    let mut tui = test_tui();
    tui.select_worktree(1);

    tui.worktree_filter = "main".to_string();
    tui.restore_selected_worktree_for_repo();

    assert_eq!(tui.selected_worktree_index(), None);

    tui.worktree_filter.clear();
    tui.restore_selected_worktree_for_repo();

    assert_eq!(tui.selected_worktree_index(), Some(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn hidden_sessions_are_not_visible_in_normal_worktree_list() {
    let mut tui = test_tui();
    tui.sessions[1].hidden = true;
    tui.selected = 1;

    assert!(!tui.visible_session_indices().contains(&1));
    assert_eq!(tui.selected_worktree_index(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn horizontal_keys_switch_repo_view_without_changing_focus() {
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

#[tokio::test(flavor = "multi_thread")]
async fn main_panel_scrolls_when_pr_comments_are_selectable() {
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

#[tokio::test(flavor = "multi_thread")]
async fn mouse_wheel_scrolls_only_when_pointer_is_over_main_panel() {
    let mut tui = test_tui();
    let area = Rect::new(0, 0, 120, 30);
    let mouse = |kind, column| MouseEvent {
        kind,
        column,
        row: 10,
        modifiers: KeyModifiers::NONE,
    };

    assert!(tui.handle_mouse_event(mouse(MouseEventKind::ScrollDown, 80), area));
    assert_eq!(tui.main_scroll, 1);

    assert!(tui.handle_mouse_event(mouse(MouseEventKind::ScrollUp, 80), area));
    assert_eq!(tui.main_scroll, 0);

    assert!(!tui.handle_mouse_event(mouse(MouseEventKind::ScrollDown, 10), area));
    assert_eq!(tui.main_scroll, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn panel_traversal_restores_selection_for_each_worktree_list() {
    let mut tui = test_tui();
    tui.worktree_list_mode = WorktreeListMode::Global;
    tui.sessions[1].pr = PrCache::observed(test_pr_summary(true), None);

    tui.focus_repos();
    tui.focus_next_panel();
    assert_eq!(tui.focused_panel, PanelFocus::Worktrees);
    assert_eq!(tui.selected_worktree_index(), Some(3));
    tui.focus_next_panel();
    assert_eq!(tui.focused_panel, PanelFocus::Merges);
    assert_eq!(tui.selected_worktree_index(), Some(1));

    tui.focus_status();
    tui.focus_previous_panel();
    assert_eq!(tui.focused_panel, PanelFocus::Merges);
    assert_eq!(tui.selected_worktree_index(), Some(1));
    tui.focus_previous_panel();
    assert_eq!(tui.focused_panel, PanelFocus::Worktrees);
    assert_eq!(tui.selected_worktree_index(), Some(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn sidebar_navigation_leaves_main_focus() {
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

#[tokio::test(flavor = "multi_thread")]
async fn open_tmux_session_target_blocks_status_enter() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Status;

    assert_eq!(
        tui.open_tmux_session_target(),
        OpenTmuxSessionTarget::Blocked("status has no Enter action")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn open_tmux_session_target_opens_repo_default_from_repos() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Repos;

    assert_eq!(
        tui.open_tmux_session_target(),
        OpenTmuxSessionTarget::RepoDefaultAgent(0)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn open_tmux_session_target_ignores_worktree_filter_for_repo_default() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Repos;
    tui.worktree_filter = "missing".to_string();

    assert_eq!(
        tui.open_tmux_session_target(),
        OpenTmuxSessionTarget::RepoDefaultAgent(0)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn open_tmux_session_target_opens_feature_worktree_agent() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Worktrees;
    tui.select_worktree(1);

    assert_eq!(
        tui.open_tmux_session_target(),
        OpenTmuxSessionTarget::WorktreeAgent
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn open_tmux_session_target_blocks_default_branch_in_worktree_panel() {
    let mut tui = test_tui();
    tui.focused_panel = PanelFocus::Worktrees;
    tui.select_worktree(0);

    assert_eq!(
        tui.open_tmux_session_target(),
        OpenTmuxSessionTarget::Blocked("selected repository has no visible worktrees")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn selected_repo_identity_survives_repo_reordering() {
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
