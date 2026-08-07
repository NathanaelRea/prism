use crate::remote::PrSummary;
use crate::view;

use super::{PanelFocus, Tui};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GitAction {
    LazyGit,
    OpenPr,
    SubmitReview,
    ResolveAllComments,
}

impl Tui {
    pub(super) fn git_action_enabled(&self, action: GitAction) -> bool {
        if action == GitAction::LazyGit {
            let program = match self.focused_panel {
                PanelFocus::Status => None,
                PanelFocus::Repos => self
                    .selected_repo_context()
                    .map(|context| context.config.tool("lazygit")),
                PanelFocus::Worktrees => self
                    .selected_worktree_context()
                    .map(|context| context.config.tool("lazygit")),
            };
            return program.is_some_and(|program| crate::process::command_exists(&program));
        }
        if action == GitAction::SubmitReview {
            return self
                .selected_repo_context()
                .is_some_and(|context| crate::process::command_exists(&context.config.tool("gh")))
                && self.focused_panel == PanelFocus::Repos
                && self.main_focused
                && self.selected_repo_pr_summary().is_some_and(|summary| {
                    !summary.merged
                        && summary.state.eq_ignore_ascii_case("OPEN")
                        && summary.change_request_identity.as_ref().is_some_and(|_| {
                            crate::remote::dispatcher::capabilities_for_summary(&summary)
                                .submit_review
                                == crate::remote::SupportLevel::Supported
                        })
                });
        }
        if self.focused_panel != PanelFocus::Worktrees {
            return false;
        }
        let Some(context) = self.selected_worktree_context() else {
            return false;
        };
        let Some(session) = self.sessions.get(context.session_index) else {
            return false;
        };
        if !session.is_task_branch(&context.config) {
            return false;
        }
        let Some(summary) = session.pr.summary() else {
            return false;
        };
        if action == GitAction::OpenPr {
            return self.remote_support_for_action(action, Some(summary))
                == Some(crate::remote::SupportLevel::Supported);
        }
        if summary.merged || !summary.state.eq_ignore_ascii_case("OPEN") {
            return false;
        }
        if action == GitAction::ResolveAllComments {
            return self.remote_support_for_action(action, Some(summary))
                == Some(crate::remote::SupportLevel::Supported)
                && self.main_focused
                && session.pr.trusted_details().is_ok_and(|details| {
                    details.is_some_and(|details| {
                        details.review_comments.iter().any(|comment| {
                            !comment.resolved && !comment.thread_id.trim().is_empty()
                        })
                    })
                });
        }
        true
    }

    pub(crate) fn remote_support_for_action(
        &self,
        action: GitAction,
        summary: Option<&PrSummary>,
    ) -> Option<crate::remote::SupportLevel> {
        let runtime = self
            .selected_worktree_context()
            .and_then(|context| self.sessions.get(context.session_index))
            .and_then(|session| self.repos.get(session.repo_index))
            .and_then(|repo| repo.remote_capabilities.as_ref());
        let capabilities = runtime
            .cloned()
            .or_else(|| summary.map(crate::remote::dispatcher::capabilities_for_summary))?;
        Some(match action {
            GitAction::OpenPr => capabilities.fetch_change_request,
            GitAction::ResolveAllComments => capabilities.resolve_review_thread,
            GitAction::LazyGit | GitAction::SubmitReview => return None,
        })
    }

    pub(super) fn remote_action_reason(&self, action: GitAction) -> Option<String> {
        let summary = self
            .selected_worktree_context()
            .and_then(|context| self.sessions.get(context.session_index))
            .and_then(|session| session.pr.summary());
        match self.remote_support_for_action(action, summary) {
            Some(crate::remote::SupportLevel::Conditional) => {
                Some("conditional support not established".to_string())
            }
            Some(crate::remote::SupportLevel::Unknown) => Some("support unknown".to_string()),
            Some(crate::remote::SupportLevel::Unsupported) => {
                Some("unsupported by provider".to_string())
            }
            None => None,
            Some(crate::remote::SupportLevel::Supported) => None,
        }
    }

    pub(super) fn git_choice(&self, action: GitAction, key: &str, label: &str) -> view::KeyChoice {
        if self.git_action_enabled(action) {
            view::KeyChoice::new(key, label)
        } else {
            let label = self
                .remote_action_reason(action)
                .map(|reason| format!("{label} ({reason})"))
                .unwrap_or_else(|| label.to_string());
            view::KeyChoice::disabled(key, label)
        }
    }

    pub(super) fn selected_repo_list_support(&self) -> Option<crate::remote::SupportLevel> {
        self.repos
            .get(self.current_repo)
            .and_then(|repo| repo.remote_capabilities.as_ref())
            .map(|capabilities| capabilities.list_change_requests)
    }

    pub(super) fn remote_pr_list_choice(&self) -> view::KeyChoice {
        match self.selected_repo_list_support() {
            Some(crate::remote::SupportLevel::Supported) => {
                view::KeyChoice::new("C", "open remote PR")
            }
            Some(level) => view::KeyChoice::disabled(
                "C",
                format!("open remote PR ({} support)", level.label()),
            ),
            None => view::KeyChoice::disabled("C", "open remote PR (adapter unavailable)"),
        }
    }
}
