use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use crate::remote::{PrCache, PrDetails, PrReviewComment};
use crate::repo::Repository;
use crate::view::{ChoiceList, RepoMainView};

use super::super::{GitAction, PanelFocus, PrPollResult, Tui, selectable_choice_key};
use super::support::{
    test_change_request_identity, test_change_request_identity_for, test_config, test_pr_summary,
    test_session, unique_temp_dir,
};

#[test]
fn git_actions_allow_push_before_remote_capabilities_or_a_pr_are_known() {
    let repo = Repository {
        root: PathBuf::from("/tmp/repo"),
    };
    let mut tui = Tui::new_single(
        repo,
        test_config(),
        vec![test_session(0, "/tmp/repo", "feature")],
    );
    tui.focused_panel = PanelFocus::Worktrees;

    assert!(tui.git_action_enabled(GitAction::Push));
    assert!(!tui.git_action_enabled(GitAction::OpenPr));
    assert!(!tui.git_action_enabled(GitAction::Merge));

    tui.repos[0].remote_capabilities = Some(crate::remote::Capabilities::for_provider(
        crate::remote::ProviderKind::GitHub,
    ));
    assert!(tui.git_action_enabled(GitAction::Push));

    tui.sessions[0].pr = PrCache::observed(test_pr_summary(false), None);
    assert!(tui.git_action_enabled(GitAction::OpenPr));
    assert!(tui.git_action_enabled(GitAction::Merge));
    assert!(!tui.git_action_enabled(GitAction::CiFix));
    tui.sessions[0].pr = PrCache::observed(
        test_pr_summary(false),
        Some(PrDetails {
            failing_checks: vec!["external-ci".to_string()],
            ..PrDetails::default()
        }),
    );
    assert!(tui.git_action_enabled(GitAction::CiFix));
    tui.sessions[0].pr = PrCache::observed(
        test_pr_summary(false),
        Some(PrDetails {
            ci_failures: vec![crate::remote::CachedCiFailure {
                name: "failed".to_string(),
                ..crate::remote::CachedCiFailure::default()
            }],
            ..PrDetails::default()
        }),
    );
    assert!(tui.git_action_enabled(GitAction::CiFix));

    tui.sessions[0].pr = PrCache::observed(test_pr_summary(true), None);
    assert!(tui.git_action_enabled(GitAction::OpenPr));
    assert!(!tui.git_action_enabled(GitAction::Merge));
    assert!(!tui.git_action_enabled(GitAction::ReviewFix));

    let mut closed = test_pr_summary(false);
    closed.state = "CLOSED".to_string();
    tui.sessions[0].pr = PrCache::observed(closed, None);
    assert!(tui.git_action_enabled(GitAction::OpenPr));
    assert!(!tui.git_action_enabled(GitAction::Merge));
    assert!(!tui.git_action_enabled(GitAction::CiFix));
}

#[test]
fn submit_review_requires_the_configured_gh_executable() {
    let temp = unique_temp_dir("prism-tui-submit-review-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    let mut tui = Tui::new_single(repo, config.clone(), Vec::new());
    tui.focused_panel = PanelFocus::Repos;
    tui.main_focused = true;
    tui.repos[0].pr_summaries = vec![test_pr_summary(false)];

    assert!(!tui.git_action_enabled(GitAction::SubmitReview));

    crate::test_support::install_tool(&mut config, &temp, "gh", "#!/bin/sh\nexit 0\n");
    tui.repos[0].config = config;

    assert!(!tui.git_action_enabled(GitAction::SubmitReview));

    tui.repos[0].pr_summaries[0].change_request_identity = Some(test_change_request_identity(
        crate::remote::ProviderKind::GitHub,
    ));
    assert!(tui.git_action_enabled(GitAction::SubmitReview));

    tui.repos[0].pr_summaries[0].state = "SUPERSEDED_BY_TRAIN".to_string();
    assert!(!tui.git_action_enabled(GitAction::SubmitReview));
    tui.repos[0].pr_summaries[0].state = "OPEN".to_string();

    for provider in [
        crate::remote::ProviderKind::GitLab,
        crate::remote::ProviderKind::Forgejo,
    ] {
        tui.repos[0].pr_summaries[0].change_request_identity =
            Some(test_change_request_identity(provider));
        assert!(!tui.git_action_enabled(GitAction::SubmitReview));
    }

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn repository_change_request_selection_uses_canonical_identity_across_reordering() {
    let repo = Repository {
        root: PathBuf::from("/tmp/repo"),
    };
    let mut tui = Tui::new_single(repo, test_config(), Vec::new());
    tui.focused_panel = PanelFocus::Repos;
    tui.repo_main_view = RepoMainView::ChangeRequests;

    let origin_identity = test_change_request_identity_for(
        crate::remote::ProviderKind::GitHub,
        "fork/widget",
        "PR_origin_42",
    );
    let upstream_identity = test_change_request_identity_for(
        crate::remote::ProviderKind::GitHub,
        "upstream/widget",
        "PR_upstream_42",
    );
    let mut origin = test_pr_summary(false);
    origin.number = 42;
    origin.title = "origin change".to_string();
    origin.change_request_identity = Some(origin_identity.clone());
    let mut upstream = origin.clone();
    upstream.title = "upstream change".to_string();
    upstream.change_request_identity = Some(upstream_identity.clone());
    tui.repos[0].pr_summaries = vec![origin.clone(), upstream.clone()];

    tui.ensure_selected_repo_pr();
    assert_eq!(
        tui.selected_repo_pr_summary()
            .unwrap()
            .change_request_identity,
        Some(origin_identity)
    );

    assert!(tui.move_repo_pr_selection(1));
    let selected_for_action = tui.selected_repo_pr_summary().unwrap();
    assert_eq!(selected_for_action.title, "upstream change");
    assert_eq!(
        selected_for_action.change_request_identity.as_ref(),
        Some(&upstream_identity)
    );

    let repository = tui.repos[0].identity.clone();
    tui.pr_poll_tx
        .send(PrPollResult::Summary {
            repository,
            sessions: Vec::new(),
            github_remote_configured: true,
            capabilities: Some(crate::remote::Capabilities::for_provider(
                crate::remote::ProviderKind::GitHub,
            )),
            summaries: Ok(vec![upstream, origin]),
            observations: Ok(Vec::new()),
            remote_branch_heads: BTreeMap::new(),
            refreshed: "now".to_string(),
            poll_started_at: Instant::now(),
        })
        .unwrap();
    tui.drain_pr_poll_results();

    assert_eq!(
        tui.selected_repo_pr_summary().unwrap().title,
        "upstream change"
    );
    let rows = tui.frame_model().repo_prs;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.iter().filter(|row| row.number == 42).count(), 2);
    assert_eq!(rows.iter().filter(|row| row.selected).count(), 1);
    assert!(rows.iter().any(|row| {
        row.selected && row.title == "upstream change" && row.repo_label == "upstream/widget"
    }));
}

#[test]
fn conditional_ci_repair_requires_loaded_failure_evidence() {
    let repo = Repository {
        root: PathBuf::from("/tmp/repo"),
    };
    let mut tui = Tui::new_single(
        repo,
        test_config(),
        vec![test_session(0, "/tmp/repo", "feature")],
    );
    tui.focused_panel = PanelFocus::Worktrees;
    tui.repos[0].remote_capabilities = Some(crate::remote::Capabilities::for_provider(
        crate::remote::ProviderKind::Forgejo,
    ));
    tui.sessions[0].pr = PrCache::observed(test_pr_summary(false), None);

    assert!(!tui.git_action_enabled(GitAction::CiFix));

    tui.sessions[0].pr = PrCache::observed(
        test_pr_summary(false),
        Some(PrDetails {
            ci_failures: vec![crate::remote::CachedCiFailure {
                name: "failed".to_string(),
                log_tail: "failure evidence".to_string(),
                ..crate::remote::CachedCiFailure::default()
            }],
            ..PrDetails::default()
        }),
    );
    assert!(tui.git_action_enabled(GitAction::CiFix));

    tui.repos[0].remote_capabilities.as_mut().unwrap().ci_logs =
        crate::remote::SupportLevel::Unsupported;
    assert!(!tui.git_action_enabled(GitAction::CiFix));
}

#[test]
fn review_resolution_action_requires_main_panel_and_unresolved_threads() {
    let repo = Repository {
        root: PathBuf::from("/tmp/repo"),
    };
    let mut tui = Tui::new_single(
        repo,
        test_config(),
        vec![test_session(0, "/tmp/repo", "feature")],
    );
    tui.focused_panel = PanelFocus::Worktrees;
    tui.sessions[0].pr = PrCache::observed(
        test_pr_summary(false),
        Some(PrDetails {
            review_comments: vec![PrReviewComment {
                thread_id: "thread-1".to_string(),
                body: "inline".to_string(),
                resolved: false,
                ..PrReviewComment::default()
            }],
            ..PrDetails::default()
        }),
    );

    assert!(!tui.git_action_enabled(GitAction::ResolveAllComments));

    tui.focus_main();
    assert!(tui.git_action_enabled(GitAction::ResolveAllComments));

    tui.sessions[0].pr.mark_preserved_stale();
    assert!(!tui.git_action_enabled(GitAction::ResolveAllComments));

    tui.sessions[0].pr = PrCache::observed(
        test_pr_summary(false),
        Some(PrDetails {
            review_comments: vec![PrReviewComment {
                thread_id: "  ".to_string(),
                resolved: false,
                ..PrReviewComment::default()
            }],
            ..PrDetails::default()
        }),
    );
    assert!(!tui.git_action_enabled(GitAction::ResolveAllComments));

    tui.sessions[0].pr = PrCache::observed(
        test_pr_summary(false),
        Some(PrDetails {
            review_comments: vec![PrReviewComment {
                thread_id: "thread-1".to_string(),
                resolved: true,
                ..PrReviewComment::default()
            }],
            ..PrDetails::default()
        }),
    );
    assert!(!tui.git_action_enabled(GitAction::ResolveAllComments));
}

#[test]
fn conditional_remote_operations_are_disabled_and_not_selectable() {
    let repo = Repository {
        root: PathBuf::from("/tmp/repo"),
    };
    let mut tui = Tui::new_single(
        repo,
        test_config(),
        vec![test_session(0, "/tmp/repo", "feature")],
    );
    tui.focused_panel = PanelFocus::Worktrees;
    tui.sessions[0].pr = PrCache::observed(test_pr_summary(false), None);
    let mut capabilities =
        crate::remote::Capabilities::for_provider(crate::remote::ProviderKind::GitHub);
    capabilities.guarded_merge = crate::remote::SupportLevel::Conditional;
    tui.repos[0].remote_capabilities = Some(capabilities);

    let choice = tui.git_choice(GitAction::Merge, "M", "merge");
    let choices = ChoiceList {
        title: "Git Actions".to_string(),
        choices: vec![choice.clone()],
    };

    assert!(choice.disabled);
    assert!(choice.label.contains("conditional support not established"));
    assert_eq!(selectable_choice_key(&choices, "M"), None);
    assert!(!tui.git_action_enabled(GitAction::Merge));
}

#[test]
fn gitlab_rebase_merge_choice_uses_adapter_capability_reason() {
    let repo = Repository {
        root: PathBuf::from("/tmp/repo"),
    };
    let mut tui = Tui::new_single(
        repo,
        test_config(),
        vec![test_session(0, "/tmp/repo", "feature")],
    );
    tui.focused_panel = PanelFocus::Worktrees;
    tui.sessions[0].pr = PrCache::observed(test_pr_summary(false), None);
    let mut capabilities =
        crate::remote::Capabilities::for_provider(crate::remote::ProviderKind::GitLab);
    capabilities.guarded_merge = crate::remote::SupportLevel::Unsupported;
    capabilities.guarded_merge_reason =
        Some("GitLab adapter does not support rebase merges".to_string());
    tui.repos[0].remote_capabilities = Some(capabilities);

    let choice = tui.git_choice(GitAction::Merge, "M", "merge");

    assert!(choice.disabled);
    assert!(
        choice
            .label
            .contains("GitLab adapter does not support rebase merges")
    );
}
