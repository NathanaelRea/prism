use std::path::PathBuf;
use std::time::Instant;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::session::Session;
use crate::tui_runtime::TerminalDriver;
use crate::view;

use super::Tui;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelFocus {
    Status,
    Repos,
    Worktrees,
    Merges,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorktreeListMode {
    Repo,
    Global,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OpenTmuxSessionTarget {
    WorktreeAgent,
    RepoPr,
    RepoDefaultAgent(usize),
    Blocked(&'static str),
}

#[derive(Clone)]
pub(crate) struct NavigationSnapshot {
    focused_panel: PanelFocus,
    main_focused: bool,
    main_scroll: usize,
    current_repo_root: Option<PathBuf>,
    selected_worktree_path: Option<PathBuf>,
    selected_comment: usize,
    worktree_list_mode: WorktreeListMode,
}

fn worktree_sort_name(session: &Session) -> String {
    session
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&session.branch)
        .to_ascii_lowercase()
}

fn worktree_priority_rank(visibility: i16) -> u8 {
    match visibility.cmp(&0) {
        std::cmp::Ordering::Greater => 0,
        std::cmp::Ordering::Equal => 1,
        std::cmp::Ordering::Less => 2,
    }
}

pub(super) fn worktree_updated_label(session: &Session) -> String {
    if let Some(label) = session.pr.last_refreshed() {
        return label.to_string();
    }
    if let Some(summary) = session.pr.summary() {
        return summary.updated_at.chars().take(10).collect();
    }
    "-".to_string()
}

fn point_in_rect(x: u16, y: u16, rect: Rect) -> bool {
    x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

impl Tui {
    pub(crate) fn is_worktree_session_panel(&self) -> bool {
        matches!(
            self.focused_panel,
            PanelFocus::Worktrees | PanelFocus::Merges
        )
    }

    pub(super) fn move_down(&mut self) {
        if self.main_focused {
            if self.move_workflow_step_selection(1) {
                self.main_scroll = self.main_scroll.saturating_add(1);
                return;
            }
            if self.move_repo_pr_selection(1) {
                return;
            }
            self.move_comment_selection(1);
            self.main_scroll = self.main_scroll.saturating_add(1);
            return;
        }
        match self.focused_panel {
            PanelFocus::Status => {}
            PanelFocus::Repos => self.move_repo_selection(1),
            PanelFocus::Worktrees | PanelFocus::Merges => self.move_worktree_selection(1),
        }
    }

    pub(super) fn move_up(&mut self) {
        if self.main_focused {
            if self.move_workflow_step_selection(-1) {
                self.main_scroll = self.main_scroll.saturating_sub(1);
                return;
            }
            if self.move_repo_pr_selection(-1) {
                return;
            }
            self.move_comment_selection(-1);
            self.main_scroll = self.main_scroll.saturating_sub(1);
            return;
        }
        match self.focused_panel {
            PanelFocus::Status => {}
            PanelFocus::Repos => self.move_repo_selection(-1),
            PanelFocus::Worktrees | PanelFocus::Merges => self.move_worktree_selection(-1),
        }
    }

    pub(super) fn move_left(&mut self) {
        if !self.main_focused {
            return;
        }
        match self.focused_panel {
            PanelFocus::Status => {}
            PanelFocus::Repos => {
                self.repo_main_view = view::RepoMainView::ChangeRequests;
            }
            PanelFocus::Worktrees | PanelFocus::Merges => {}
        }
    }

    pub(super) fn move_right(&mut self) {
        if !self.main_focused {
            return;
        }
        match self.focused_panel {
            PanelFocus::Status => {}
            PanelFocus::Repos => {
                self.repo_main_view = view::RepoMainView::Kanban;
            }
            PanelFocus::Worktrees | PanelFocus::Merges => {}
        }
    }

    pub(super) fn focus_next_panel(&mut self) {
        self.main_scroll = 0;
        self.focused_panel = match self.focused_panel {
            PanelFocus::Status => PanelFocus::Repos,
            PanelFocus::Repos => PanelFocus::Worktrees,
            PanelFocus::Worktrees => PanelFocus::Merges,
            PanelFocus::Merges => PanelFocus::Status,
        };
        self.main_focused = false;
    }

    pub(super) fn focus_previous_panel(&mut self) {
        self.main_scroll = 0;
        self.focused_panel = match self.focused_panel {
            PanelFocus::Status => PanelFocus::Merges,
            PanelFocus::Repos => PanelFocus::Status,
            PanelFocus::Worktrees => PanelFocus::Repos,
            PanelFocus::Merges => PanelFocus::Worktrees,
        };
        self.main_focused = false;
    }

    pub(crate) fn focus_status(&mut self) {
        self.main_scroll = 0;
        self.focused_panel = PanelFocus::Status;
        self.main_focused = false;
    }

    pub(super) fn focus_repos(&mut self) {
        self.main_scroll = 0;
        self.focused_panel = PanelFocus::Repos;
        self.main_focused = false;
    }

    pub(crate) fn focus_worktrees(&mut self) {
        self.main_scroll = 0;
        self.focused_panel = PanelFocus::Worktrees;
        self.main_focused = false;
        self.restore_selected_worktree_for_repo();
    }

    pub(crate) fn focus_merges(&mut self) {
        self.main_scroll = 0;
        self.focused_panel = PanelFocus::Merges;
        self.main_focused = false;
        self.restore_selected_worktree_for_repo();
    }

    pub(super) fn switch_worktree_list_mode(&mut self, mode: WorktreeListMode) {
        if !self.is_worktree_session_panel() || self.worktree_list_mode == mode {
            return;
        }
        let selected = self.selected_worktree_index();
        self.worktree_list_mode = mode;
        self.persist_worktree_list_mode();
        if mode == WorktreeListMode::Repo {
            if let Some(index) = selected {
                self.select_worktree(index);
            } else {
                self.restore_selected_worktree_for_repo();
            }
        }
    }

    pub(super) fn persist_worktree_list_mode(&self) {
        let Some(path) = self.ui_state_path.as_deref() else {
            return;
        };
        if let Err(error) = crate::ui_state::save_to_path(path, self.worktree_list_mode) {
            let _ = crate::observability::append_runtime_message(
                &self.repo,
                &format!("UI state save failed: {error}"),
            );
        }
    }

    pub(super) fn focus_main(&mut self) {
        self.main_focused = true;
        self.ensure_selected_repo_pr();
    }

    pub(super) fn open_tmux_session_target(&self) -> OpenTmuxSessionTarget {
        match self.focused_panel {
            PanelFocus::Status => OpenTmuxSessionTarget::Blocked("status has no Enter action"),
            PanelFocus::Repos => {
                if self.main_focused && self.selected_repo_pr_summary().is_some() {
                    return OpenTmuxSessionTarget::RepoPr;
                }
                if let Some(index) = self.selected_repo_default_session_index() {
                    OpenTmuxSessionTarget::RepoDefaultAgent(index)
                } else {
                    OpenTmuxSessionTarget::Blocked("selected repository has no default worktree")
                }
            }
            PanelFocus::Worktrees | PanelFocus::Merges => {
                if self.selected_worktree_context().is_none() {
                    return OpenTmuxSessionTarget::Blocked(
                        "selected repository has no visible worktrees",
                    );
                }
                OpenTmuxSessionTarget::WorktreeAgent
            }
        }
    }

    pub(super) fn move_repo_selection(&mut self, direction: isize) {
        let indices = self.visible_repo_indices();
        let current = indices
            .iter()
            .position(|index| *index == self.current_repo)
            .unwrap_or(0);
        let next = current as isize + direction;
        if next < 0 {
            return;
        }
        if let Some(repo_index) = indices.get(next as usize).copied() {
            self.select_repo(repo_index);
        }
    }

    pub(super) fn move_worktree_selection(&mut self, direction: isize) {
        let indices = self.visible_session_indices();
        let current = indices
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        let next = current as isize + direction;
        if next < 0 {
            return;
        }
        if let Some(next) = indices.get(next as usize).copied() {
            self.select_worktree(next);
        }
    }

    pub(super) fn move_repo_pr_selection(&mut self, direction: isize) -> bool {
        if self.focused_panel != PanelFocus::Repos
            || self.repo_main_view != view::RepoMainView::ChangeRequests
        {
            return false;
        }
        let prs = self.current_repo_change_request_summaries();
        if prs.is_empty() {
            return false;
        }
        let current_identity = self.selected_repo_pr_identity();
        let current = current_identity
            .and_then(|identity| {
                prs.iter()
                    .position(|summary| summary.change_request_identity.as_ref() == Some(identity))
            })
            .unwrap_or(0);
        let next = current as isize + direction;
        if next < 0 {
            return true;
        }
        if let Some(identity) = prs
            .get(next as usize)
            .and_then(|summary| summary.change_request_identity.clone())
            && let Some(repo) = self.repos.get(self.current_repo)
        {
            self.selected_pr_by_repo
                .insert(repo.repo.root.clone(), identity);
        }
        true
    }

    pub(super) fn current_repo_change_request_summaries(&self) -> Vec<crate::remote::PrSummary> {
        self.repos
            .get(self.current_repo)
            .map(|managed| {
                managed
                    .pr_summaries
                    .iter()
                    .filter(|summary| {
                        !summary.merged
                            && !matches!(
                                summary.state.trim().to_ascii_uppercase().as_str(),
                                "CLOSED" | "MERGED"
                            )
                            && summary.change_request_identity.is_some()
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn selected_repo_pr_identity(
        &self,
    ) -> Option<&crate::remote::CanonicalChangeRequestIdentity> {
        let root = &self.repos.get(self.current_repo)?.repo.root;
        self.selected_pr_by_repo.get(root)
    }

    pub(crate) fn selected_repo_pr_summary(&self) -> Option<crate::remote::PrSummary> {
        let prs = self.current_repo_change_request_summaries();
        let selected = self.selected_repo_pr_identity();
        selected
            .and_then(|identity| {
                prs.iter()
                    .find(|summary| summary.change_request_identity.as_ref() == Some(identity))
                    .cloned()
            })
            .or_else(|| prs.first().cloned())
    }

    pub(crate) fn ensure_selected_repo_pr(&mut self) {
        let prs = self.current_repo_change_request_summaries();
        let Some(first) = prs.first() else {
            if let Some(repo) = self.repos.get(self.current_repo) {
                self.selected_pr_by_repo.remove(&repo.repo.root);
            }
            return;
        };
        let selected = self.selected_repo_pr_identity();
        if selected.is_some_and(|identity| {
            prs.iter()
                .any(|summary| summary.change_request_identity.as_ref() == Some(identity))
        }) {
            return;
        }
        if let Some(identity) = first.change_request_identity.clone()
            && let Some(repo) = self.repos.get(self.current_repo)
        {
            self.selected_pr_by_repo
                .insert(repo.repo.root.clone(), identity);
        }
    }

    pub(crate) fn select_top_visible(&mut self) {
        if self.main_focused {
            return;
        }
        match self.focused_panel {
            PanelFocus::Status => {}
            PanelFocus::Repos => {
                if let Some(index) = self.visible_repo_indices().first().copied() {
                    self.select_repo(index);
                }
            }
            PanelFocus::Worktrees | PanelFocus::Merges => {
                if let Some(index) = self.visible_session_indices().first().copied() {
                    self.select_worktree(index);
                }
            }
        }
    }

    pub(super) fn select_bottom_visible(&mut self) {
        if self.main_focused {
            return;
        }
        match self.focused_panel {
            PanelFocus::Status => {}
            PanelFocus::Repos => {
                if let Some(index) = self.visible_repo_indices().last().copied() {
                    self.select_repo(index);
                }
            }
            PanelFocus::Worktrees | PanelFocus::Merges => {
                if let Some(index) = self.visible_session_indices().last().copied() {
                    self.select_worktree(index);
                }
            }
        }
    }

    pub(crate) fn visible_repo_indices(&self) -> Vec<usize> {
        let filter = self.repo_filter.trim().to_ascii_lowercase();
        self.repos
            .iter()
            .enumerate()
            .filter_map(|(index, repo)| {
                (filter.is_empty()
                    || repo.label.to_ascii_lowercase().contains(&filter)
                    || repo
                        .repo
                        .root
                        .display()
                        .to_string()
                        .to_ascii_lowercase()
                        .contains(&filter)
                    || repo.key.is_some_and(|key| key.to_string() == filter))
                .then_some(index)
            })
            .collect()
    }

    pub(crate) fn visible_session_indices(&self) -> Vec<usize> {
        let finishing = self.focused_panel == PanelFocus::Merges;
        self.visible_session_indices_for(finishing)
    }

    pub(crate) fn visible_worktree_indices(&self) -> Vec<usize> {
        self.visible_session_indices_for(false)
    }

    pub(crate) fn visible_merge_indices(&self) -> Vec<usize> {
        self.visible_session_indices_for(true)
    }

    fn visible_session_indices_for(&self, finishing: bool) -> Vec<usize> {
        let filter = self.worktree_filter.trim().to_ascii_lowercase();
        let mut indices = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| {
                let merge_finishing = session.pr.summary().is_some_and(|summary| {
                    summary.merge_progress() != crate::remote::PrMergeProgress::Active
                });
                (!session.hidden
                    && merge_finishing == finishing
                    && (self.worktree_list_mode == WorktreeListMode::Global
                        || session.repo_index == self.current_repo)
                    && !self
                        .repos
                        .get(session.repo_index)
                        .is_some_and(|repo| repo.config.is_default_branch(&session.branch))
                    && (filter.is_empty()
                        || session.branch.to_ascii_lowercase().contains(&filter)
                        || session.repo_label.to_ascii_lowercase().contains(&filter)
                        || session
                            .prompt_summary
                            .to_ascii_lowercase()
                            .contains(&filter)
                        || session.path_display.to_ascii_lowercase().contains(&filter)
                        || session
                            .wt_columns
                            .values()
                            .any(|value| value.to_ascii_lowercase().contains(&filter))))
                .then_some(index)
            })
            .collect::<Vec<_>>();
        indices.sort_by_key(|index| self.worktree_sort_key(*index));
        indices
    }

    pub(super) fn worktree_sort_key(&self, index: usize) -> (u8, String, String) {
        let Some(session) = self.sessions.get(index) else {
            return (1, String::new(), String::new());
        };
        (
            worktree_priority_rank(session.visibility),
            session.repo_label.clone(),
            worktree_sort_name(session),
        )
    }

    pub(super) fn mark_selected_seen(&mut self) {
        if let Some(session) = self.sessions.get_mut(self.selected) {
            session.unseen_comments = false;
        }
    }

    pub(crate) fn select_worktree(&mut self, index: usize) {
        self.main_scroll = 0;
        let Some(session) = self.sessions.get(index) else {
            return;
        };
        let repo_index = session.repo_index;
        let path = session.path.clone();
        self.selected = index;
        self.selected_comment = 0;
        self.selected_workflow_run = None;
        self.selected_workflow_step = None;
        self.workflow_step_selection_manual = false;
        if let Some(repo) = self.repos.get(repo_index) {
            let repo_root = repo.repo.root.clone();
            self.current_repo = repo_index;
            self.selected_repo_root = Some(repo_root.clone());
            self.sync_selected_repo_context();
            self.selected_worktree_by_repo.insert(repo_root, path);
        }
        self.mark_selected_seen();
    }

    pub(crate) fn navigation_snapshot(&self) -> NavigationSnapshot {
        NavigationSnapshot {
            focused_panel: self.focused_panel,
            main_focused: self.main_focused,
            main_scroll: self.main_scroll,
            current_repo_root: self
                .repos
                .get(self.current_repo)
                .map(|repo| repo.repo.root.clone()),
            selected_worktree_path: self
                .selected_worktree_index()
                .and_then(|index| self.sessions.get(index))
                .map(|session| session.path.clone()),
            selected_comment: self.selected_comment,
            worktree_list_mode: self.worktree_list_mode,
        }
    }

    pub(crate) fn restore_navigation_snapshot(&mut self, snapshot: NavigationSnapshot) {
        self.worktree_list_mode = snapshot.worktree_list_mode;
        if let Some(root) = snapshot.current_repo_root.as_ref()
            && let Some(index) = self.repos.iter().position(|repo| repo.repo.root == *root)
        {
            self.current_repo = index;
            self.selected_repo_root = Some(root.clone());
            self.sync_selected_repo_context();
        }
        if let Some(path) = snapshot.selected_worktree_path.as_ref()
            && let Some(index) = self
                .sessions
                .iter()
                .position(|session| session.path == *path)
        {
            self.selected = index;
            if let Some(session) = self.sessions.get(index)
                && let Some(repo) = self.repos.get(session.repo_index)
            {
                self.selected_worktree_by_repo
                    .insert(repo.repo.root.clone(), session.path.clone());
            }
        } else if self.selected_worktree_index().is_none() {
            self.restore_selected_worktree_for_repo();
        }
        self.selected_comment = snapshot.selected_comment;
        self.focused_panel = snapshot.focused_panel;
        self.main_focused = snapshot.main_focused;
        self.main_scroll = snapshot.main_scroll;
    }

    pub(super) fn selected_repo_default_session_index(&self) -> Option<usize> {
        let config = self.repos.get(self.current_repo).map(|repo| &repo.config)?;
        self.sessions
            .iter()
            .enumerate()
            .find_map(|(index, session)| {
                (session.repo_index == self.current_repo
                    && config.is_default_branch(&session.branch))
                .then_some(index)
            })
    }

    pub(super) fn adjust_selected_visibility(&mut self, delta: i16) -> Result<(), String> {
        let Some(index) = self.selected_worktree_index() else {
            return Ok(());
        };
        let Some(session) = self.sessions.get(index) else {
            return Ok(());
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return Ok(());
        };
        let visibility = session.visibility.saturating_add(delta).clamp(-9, 9);
        crate::session::set_worktree_visibility(&managed.repo, session, visibility)?;
        if let Some(session) = self.sessions.get_mut(index) {
            session.visibility = visibility;
        }
        Ok(())
    }

    pub(super) fn selected_comment_rows(&self) -> Vec<view::PrCommentDisplayRow> {
        let Some(index) = self.selected_worktree_index() else {
            return Vec::new();
        };
        self.sessions
            .get(index)
            .and_then(|session| session.pr.details())
            .map(view::pr_comment_rows)
            .unwrap_or_default()
    }

    pub(super) fn move_comment_selection(&mut self, direction: isize) -> bool {
        if !self.is_worktree_session_panel() {
            return false;
        }
        let rows = self.selected_comment_rows();
        if rows.is_empty() {
            self.selected_comment = 0;
            return false;
        }
        let current = self.selected_comment.min(rows.len().saturating_sub(1));
        let next = current as isize + direction;
        self.selected_comment = if next < 0 {
            0
        } else {
            (next as usize).min(rows.len().saturating_sub(1))
        };
        true
    }

    pub(super) fn open_selected_comment_dialog(
        &mut self,
        runtime: &mut dyn TerminalDriver,
    ) -> Result<bool, String> {
        if !self.main_focused || !self.is_worktree_session_panel() {
            return Ok(false);
        }
        let rows = self.selected_comment_rows();
        let Some(row) = rows.get(self.selected_comment) else {
            return Ok(false);
        };
        let mut lines = vec![
            view::DialogLine {
                text: format!("kind: {}", row.kind),
                attention: false,
            },
            view::DialogLine {
                text: format!("author: {}", row.author),
                attention: false,
            },
            view::DialogLine {
                text: format!("resolved: {}", row.resolved),
                attention: row.resolved.eq_ignore_ascii_case("no"),
            },
        ];
        if !row.context.trim().is_empty() {
            lines.push(view::DialogLine {
                text: format!("context: {}", row.context),
                attention: false,
            });
        }
        lines.push(view::DialogLine {
            text: String::new(),
            attention: false,
        });
        lines.push(view::DialogLine {
            text: row.body.clone(),
            attention: false,
        });
        self.notice_dialog(runtime, "Comment Details", lines)?;
        Ok(true)
    }

    pub(crate) fn selected_worktree_index(&self) -> Option<usize> {
        self.visible_session_indices()
            .contains(&self.selected)
            .then_some(self.selected)
    }

    pub(crate) fn ensure_navigation_valid(&mut self) {
        if self.repos.is_empty() {
            self.current_repo = 0;
            self.selected_repo_root = None;
            self.selected = self.sessions.len();
            return;
        }
        if let Some(root) = &self.selected_repo_root
            && let Some(index) = self.repos.iter().position(|repo| repo.repo.root == *root)
        {
            self.current_repo = index;
        }
        self.current_repo = self.current_repo.min(self.repos.len().saturating_sub(1));
        if !self.visible_repo_indices().contains(&self.current_repo)
            && let Some(repo_index) = self.visible_repo_indices().first().copied()
        {
            self.current_repo = repo_index;
        }
        self.selected_repo_root = self
            .repos
            .get(self.current_repo)
            .map(|repo| repo.repo.root.clone());
        self.sync_selected_repo_context();
        self.ensure_selected_repo_pr();
        self.restore_selected_worktree_for_repo();
    }

    pub(super) fn restore_selected_worktree_for_repo(&mut self) {
        let indices = self.visible_session_indices();
        let remembered = self
            .repos
            .get(self.current_repo)
            .and_then(|repo| self.selected_worktree_by_repo.get(&repo.repo.root));
        if let Some(index) = remembered.and_then(|path| {
            indices.iter().copied().find(|index| {
                self.sessions
                    .get(*index)
                    .is_some_and(|session| session.path == *path)
            })
        }) {
            self.selected = index;
            self.selected_comment = 0;
            return;
        }
        self.selected = indices
            .iter()
            .copied()
            .find(|index| {
                self.sessions
                    .get(*index)
                    .is_some_and(|session| session.repo_index == self.current_repo)
            })
            .or_else(|| indices.first().copied())
            .unwrap_or(self.sessions.len());
        self.selected_comment = 0;
    }

    pub(super) fn select_repo_by_key(&mut self, key: char) -> Result<(), String> {
        let Some(repo_index) = self.repos.iter().position(|repo| repo.key == Some(key)) else {
            self.show_message(&format!("no repository is bound to {key}"))?;
            return Ok(());
        };
        if !self.visible_repo_indices().contains(&repo_index) {
            self.repo_filter.clear();
        }
        self.select_repo(repo_index);
        Ok(())
    }

    pub(crate) fn select_repo(&mut self, repo_index: usize) {
        self.main_scroll = 0;
        self.current_repo = repo_index.min(self.repos.len().saturating_sub(1));
        self.selected_repo_root = self
            .repos
            .get(self.current_repo)
            .map(|repo| repo.repo.root.clone());
        self.sync_selected_repo_context();
        self.ensure_selected_repo_pr();
    }

    pub(super) fn clear_leader_hint(&mut self) {
        self.leader_hint = None;
    }

    pub(super) fn search_sessions(
        &mut self,
        runtime: &mut dyn TerminalDriver,
    ) -> Result<(), String> {
        match self.focused_panel {
            PanelFocus::Status => {
                self.show_message("status panel has no filter")?;
            }
            PanelFocus::Repos => {
                let initial = self.repo_filter.clone();
                let Some(input) = self.prompt_line_dialog(
                    runtime,
                    "Search Repositories",
                    "Filter (empty clears): ",
                    &initial,
                )?
                else {
                    return Ok(());
                };
                self.repo_filter = input;
                self.ensure_navigation_valid();
            }
            PanelFocus::Worktrees | PanelFocus::Merges => {
                let initial = self.worktree_filter.clone();
                let Some(input) = self.prompt_line_dialog(
                    runtime,
                    "Search Worktrees",
                    "Filter (empty clears): ",
                    &initial,
                )?
                else {
                    return Ok(());
                };
                self.worktree_filter = input;
                self.restore_selected_worktree_for_repo();
            }
        }
        Ok(())
    }

    pub(super) fn handle_mouse_event(&mut self, event: MouseEvent, area: Rect) -> bool {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_mouse_click(event.column, event.row, area);
                true
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let body_height = area.height.saturating_sub(1);
                let sidebar_width =
                    view::sidebar_width_for(area.width, self.config.layout.sidebar_width);
                if event.column < sidebar_width
                    || event.column >= area.width
                    || event.row >= body_height
                {
                    return false;
                }
                if event.kind == MouseEventKind::ScrollDown {
                    self.main_scroll = self.main_scroll.saturating_add(1);
                } else {
                    self.main_scroll = self.main_scroll.saturating_sub(1);
                }
                true
            }
            _ => false,
        }
    }

    fn handle_mouse_click(&mut self, x: u16, y: u16, area: Rect) {
        let body_height = area.height.saturating_sub(1);
        if x >= view::sidebar_width_for(area.width, self.config.layout.sidebar_width)
            || y >= body_height
        {
            return;
        }
        let sidebar = Rect::new(
            0,
            0,
            view::sidebar_width_for(area.width, self.config.layout.sidebar_width),
            body_height,
        );
        let (_, repos, worktrees, merges) = view::sidebar_areas(sidebar);
        if point_in_rect(x, y, repos) {
            let row = y.saturating_sub(repos.y).saturating_sub(1) as usize;
            if let Some(index) = self.visible_repo_indices().get(row).copied() {
                self.select_repo(index);
                self.focus_repos();
            }
            return;
        }
        if point_in_rect(x, y, worktrees) {
            let row = y.saturating_sub(worktrees.y).saturating_sub(2) as usize;
            if let Some(index) = self.visible_worktree_indices().get(row).copied() {
                self.select_worktree(index);
                self.focus_worktrees();
            }
            return;
        }
        if point_in_rect(x, y, merges) {
            let row = y.saturating_sub(merges.y).saturating_sub(2) as usize;
            if let Some(index) = self.visible_merge_indices().get(row).copied() {
                self.select_worktree(index);
                self.focus_merges();
            }
        }
    }

    pub(super) fn expire_status_message(&mut self) -> bool {
        if self
            .status_message_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.status_message = None;
            self.status_message_until = None;
            return true;
        }
        false
    }
}
