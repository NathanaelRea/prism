#![allow(dead_code)]

use crate::config::Config;
use crate::git;
use crate::github::{PrCache, PrDetails, PrReview, PrReviewComment, PrSummary, RepoPolicyCache};
use crate::repo::Repository;
use crate::review::{ReviewFeedback, ReviewFeedbackFilter, actionable_review_feedback};
use crate::session::Session;
use crate::verify::run_merge_conflict_check_against;

use super::stabilization_model::*;

pub(crate) fn build_stabilization_snapshot(
    repo: &Repository,
    session: &Session,
    run: Option<&super::AutoRun>,
    config: &Config,
) -> StabilizationSnapshot {
    let local_head_sha = git::current_head_sha(&session.path, config).ok();
    let remote_head_sha = git::remote_branch_head_sha(&session.path, &session.branch, config)
        .ok()
        .flatten();
    let base_sha = session.pr.summary().and_then(|summary| {
        git::remote_branch_head_sha(&session.path, &summary.base_ref, config)
            .ok()
            .flatten()
    });
    let github_remote = crate::github::github_remote_repo(&session.path, config, "origin").ok();
    let policy_cache = github_remote
        .as_deref()
        .and_then(|remote| crate::github::load_repo_policy_cache(repo, remote));
    let merge_conflict = session.pr.summary().and_then(|summary| {
        (!session.is_default_branch(config) && !session.is_detached())
            .then(|| run_merge_conflict_check_against(config, &session.path, &summary.base_ref))
    });

    let mut pull_request = pull_request_facts_from_cache_with_baseline(
        &session.pr,
        config,
        base_sha,
        merge_conflict.as_ref(),
        policy_cache.as_ref(),
        run.and_then(|run| run.review_baseline_json.as_deref()),
    );
    if policy_cache
        .as_ref()
        .is_some_and(|policy| policy.required_approvals > 0 && policy.error.is_none())
        && let Some(pull_request) = pull_request.as_mut()
    {
        pull_request.review.approval_required = true;
    }
    let policy = policy_facts_from_cache(policy_cache.as_ref(), pull_request.as_ref());
    let default_base = policy_cache
        .as_ref()
        .and_then(|policy| policy.default_branch.clone())
        .or_else(|| config.default_base.clone());
    let is_default_branch = session.is_default_branch(config)
        || default_base.as_deref() == Some(session.branch.as_str());

    StabilizationSnapshot {
        run: run.map(AutoRunRef::from),
        repository: RepositoryFacts {
            root: repo.root.clone(),
            default_base,
            github_remote,
            policy_refreshed_unix_ms: policy_cache.as_ref().map(|policy| policy.refreshed_unix_ms),
            policy_error: policy_cache
                .as_ref()
                .and_then(|policy| policy.error.clone()),
        },
        worktree: WorktreeFacts {
            path: session.path.clone(),
            branch: session.branch.clone(),
            is_default_branch,
            detached: session.is_detached(),
            dirty: git::selected_dirty(&session.path, config)
                .unwrap_or_else(|_| status_label_dirty(&session.status_label)),
            local_head_sha,
            remote_head_sha,
        },
        pull_request,
        policy,
        goal: StabilizationGoal {
            auto_merge: config.auto.merge,
            cleanup_after_merge: config.auto.cleanup_after_merge,
        },
        pending_push: run.and_then(|run| run.pending_push.clone()),
    }
}

pub(crate) fn build_auto_run_stabilization_snapshot(
    repo: &Repository,
    run: &super::AutoRun,
    config: &Config,
) -> StabilizationSnapshot {
    let policy_refresh_error = if config.auto.merge {
        crate::github::refresh_repo_policy_cache(repo, &run.worktree_path, config).err()
    } else {
        None
    };
    let mut cache = crate::github::load_pr_cache(repo, &run.branch);
    let _ = crate::github::refresh_pr_cache(
        repo,
        &run.branch,
        &mut cache,
        &run.worktree_path,
        config,
        true,
    );
    let local_head_sha = git::current_head_sha(&run.worktree_path, config).ok();
    let remote_head_sha = git::remote_branch_head_sha(&run.worktree_path, &run.branch, config)
        .ok()
        .flatten();
    let base_sha = cache.summary().and_then(|summary| {
        git::remote_branch_head_sha(&run.worktree_path, &summary.base_ref, config)
            .ok()
            .flatten()
    });
    let github_remote =
        crate::github::github_remote_repo(&run.worktree_path, config, "origin").ok();
    let policy_cache = github_remote
        .as_deref()
        .and_then(|remote| crate::github::load_repo_policy_cache(repo, remote));
    let is_default_branch = config.is_default_branch(&run.branch)
        || policy_cache
            .as_ref()
            .and_then(|policy| policy.default_branch.as_deref())
            == Some(run.branch.as_str());
    let detached = run.branch == "(detached)";
    let merge_conflict = cache.summary().and_then(|summary| {
        (!is_default_branch && !detached).then(|| {
            run_merge_conflict_check_against(config, &run.worktree_path, &summary.base_ref)
        })
    });

    let mut pull_request = pull_request_facts_from_cache_with_baseline(
        &cache,
        config,
        base_sha,
        merge_conflict.as_ref(),
        policy_cache.as_ref(),
        run.review_baseline_json.as_deref(),
    );
    if policy_cache
        .as_ref()
        .is_some_and(|policy| policy.required_approvals > 0 && policy.error.is_none())
        && let Some(pull_request) = pull_request.as_mut()
    {
        pull_request.review.approval_required = true;
    }
    let policy = policy_refresh_error.as_ref().map_or_else(
        || policy_facts_from_cache(policy_cache.as_ref(), pull_request.as_ref()),
        |error| PolicyFacts::Unknown {
            reason: Some(error.clone()),
        },
    );
    let default_base = policy_cache
        .as_ref()
        .and_then(|policy| policy.default_branch.clone())
        .or_else(|| config.default_base.clone());

    StabilizationSnapshot {
        run: Some(AutoRunRef::from(run)),
        repository: RepositoryFacts {
            root: repo.root.clone(),
            default_base,
            github_remote,
            policy_refreshed_unix_ms: policy_cache.as_ref().map(|policy| policy.refreshed_unix_ms),
            policy_error: policy_refresh_error.or_else(|| {
                policy_cache
                    .as_ref()
                    .and_then(|policy| policy.error.clone())
            }),
        },
        worktree: WorktreeFacts {
            path: run.worktree_path.clone(),
            branch: run.branch.clone(),
            is_default_branch,
            detached,
            dirty: git::selected_dirty(&run.worktree_path, config).unwrap_or(false),
            local_head_sha,
            remote_head_sha,
        },
        pull_request,
        policy,
        goal: StabilizationGoal {
            auto_merge: config.auto.merge,
            cleanup_after_merge: config.auto.cleanup_after_merge,
        },
        pending_push: run.pending_push.clone(),
    }
}

fn policy_facts_from_cache(
    policy: Option<&RepoPolicyCache>,
    pull_request: Option<&PullRequestFacts>,
) -> PolicyFacts {
    let Some(policy) = policy else {
        return PolicyFacts::Unknown {
            reason: Some("repository policy cache is not available yet".to_string()),
        };
    };
    if let Some(error) = &policy.error
        && !error.trim().is_empty()
    {
        return PolicyFacts::Unknown {
            reason: Some(error.clone()),
        };
    }
    let mut blockers = Vec::new();
    if let Some(pull_request) = pull_request {
        if policy.required_approvals > 0
            && !pull_request
                .review
                .decision
                .eq_ignore_ascii_case("APPROVED")
        {
            blockers.push(PolicyBlocker::RequiredApprovalMissing);
        }
        for check in &pull_request.ci.required {
            match check.state {
                crate::github::PrCheckState::Unknown => {
                    blockers.push(PolicyBlocker::RequiredCheckMissing(check.name.clone()));
                }
                crate::github::PrCheckState::Failed | crate::github::PrCheckState::Mixed => {
                    blockers.push(PolicyBlocker::RequiredCheckFailing(check.name.clone()));
                }
                crate::github::PrCheckState::Pending | crate::github::PrCheckState::Success => {}
            }
        }
        if policy.require_conversation_resolution
            && !pull_request.review.unresolved_threads.is_empty()
        {
            blockers.push(PolicyBlocker::ConversationsUnresolved);
        }
        if policy.require_branch_up_to_date
            && matches!(&pull_request.mergeability, MergeabilityFacts::Blocked { reason } if reason.contains("BEHIND"))
        {
            blockers.push(PolicyBlocker::BranchOutOfDate);
        }
    }
    if policy.merge_queue_required {
        blockers.push(PolicyBlocker::MergeQueueRequired);
    }
    if !blockers.is_empty() {
        return PolicyFacts::Blocked { blockers };
    }
    PolicyFacts::Satisfied
}

pub(crate) fn pull_request_facts_from_cache(
    cache: &PrCache,
    config: &Config,
    base_sha: Option<String>,
    merge_conflict: Option<&crate::verify::VerifyCheckResult>,
    policy: Option<&RepoPolicyCache>,
) -> Option<PullRequestFacts> {
    pull_request_facts_from_cache_with_baseline(
        cache,
        config,
        base_sha,
        merge_conflict,
        policy,
        None,
    )
}

fn pull_request_facts_from_cache_with_baseline(
    cache: &PrCache,
    config: &Config,
    base_sha: Option<String>,
    merge_conflict: Option<&crate::verify::VerifyCheckResult>,
    policy: Option<&RepoPolicyCache>,
    review_baseline_json: Option<&str>,
) -> Option<PullRequestFacts> {
    let summary = cache.summary()?;
    let details = cache.details();
    let observation_error = match cache.trusted_summary() {
        Err(error) => Some(error),
        Ok(None) => Some("pull request summary is unavailable".to_string()),
        Ok(Some(_)) => match cache.trusted_details() {
            Err(error) => Some(error),
            Ok(None) => Some("pull request details have not been observed".to_string()),
            Ok(Some(_)) => None,
        },
    };
    Some(PullRequestFacts {
        number: summary.number,
        url: summary.url.clone(),
        state: pull_request_state(summary),
        draft: summary.draft,
        head_sha: summary.head_sha.clone(),
        base_ref: summary.base_ref.clone(),
        base_sha,
        updated_at: summary.updated_at.clone(),
        ci: ci_facts(summary, details, policy),
        review: review_facts(summary, details, config, review_baseline_json),
        mergeability: mergeability_facts(summary, merge_conflict),
        top_level_comment_count: details
            .map(|details| details.comments.len())
            .unwrap_or(summary.comment_count as usize),
        observation_error,
    })
}

impl From<&super::AutoRun> for AutoRunRef {
    fn from(run: &super::AutoRun) -> Self {
        Self {
            id: run.id.clone(),
            status: run.status,
            pr_number: run.pr_number,
            pr_url: run.pr_url.clone(),
            current_head_sha: run.current_head_sha.clone(),
        }
    }
}

fn ci_facts(
    summary: &PrSummary,
    details: Option<&PrDetails>,
    policy: Option<&RepoPolicyCache>,
) -> CiFacts {
    let required = policy
        .filter(|policy| {
            policy
                .error
                .as_ref()
                .is_none_or(|error| error.trim().is_empty())
        })
        .map(|policy| required_check_facts(policy, details))
        .unwrap_or_default();
    let optional_failures = details
        .map(|details| optional_failures(details, &required))
        .unwrap_or_default();
    CiFacts {
        aggregate: summary.check_state(),
        required,
        optional_failures,
        failures: details
            .map(|details| details.ci_failures.clone())
            .unwrap_or_default(),
    }
}

fn required_check_facts(policy: &RepoPolicyCache, details: Option<&PrDetails>) -> Vec<CheckFact> {
    policy
        .required_checks
        .iter()
        .filter_map(|name| {
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            let context = details.and_then(|details| {
                details
                    .check_contexts
                    .iter()
                    .find(|context| context.name.eq_ignore_ascii_case(name))
            });
            Some(CheckFact {
                name: name.to_string(),
                state: context
                    .map(|context| context.state)
                    .unwrap_or(crate::github::PrCheckState::Unknown),
                required: true,
                head_sha: None,
            })
        })
        .collect()
}

fn optional_failures(details: &PrDetails, required: &[CheckFact]) -> Vec<String> {
    details
        .failing_checks
        .iter()
        .filter(|name| {
            !required
                .iter()
                .any(|check| check.name.eq_ignore_ascii_case(name))
        })
        .cloned()
        .collect()
}

fn review_facts(
    summary: &PrSummary,
    details: Option<&PrDetails>,
    config: &Config,
    review_baseline_json: Option<&str>,
) -> ReviewFacts {
    let mut actionable_reviews = Vec::new();
    let mut unresolved_threads = Vec::new();
    let top_level_comments = details.map(|details| details.comments.len()).unwrap_or(0);

    if let Some(details) = details {
        let feedback = stabilization_review_feedback(details, review_baseline_json);
        let mut review_bodies = feedback.review_bodies;
        review_bodies.retain(|review| {
            if crate::review::is_copilot_reviewer(&review.author) {
                return false;
            }
            let superseded = details.reviews.iter().any(|candidate| {
                candidate.author.eq_ignore_ascii_case(&review.author)
                    && candidate.submitted_at > review.submitted_at
                    && matches!(
                        candidate.state.trim().to_ascii_uppercase().as_str(),
                        "APPROVED" | "DISMISSED" | "CHANGES_REQUESTED"
                    )
            });
            !superseded
                && !matches!(
                    review.state.trim().to_ascii_uppercase().as_str(),
                    "APPROVED" | "DISMISSED"
                )
        });
        actionable_reviews.extend(review_bodies.into_iter().map(review_body_item));
        for comment in feedback.inline_comments {
            let fact = review_thread_fact(comment);
            if !fact.resolved {
                unresolved_threads.push(fact.clone());
            }
            actionable_reviews.push(ActionableReviewItem::ReviewThreadComment(fact));
        }
    }

    ReviewFacts {
        decision: summary.review_decision.clone(),
        approval_required: config.auto.require_review_approval,
        actionable_reviews,
        unresolved_threads,
        top_level_comments,
    }
}

pub(crate) fn stabilization_review_feedback<'a>(
    details: &'a PrDetails,
    review_baseline_json: Option<&str>,
) -> ReviewFeedback<'a> {
    let baseline = super::non_agent::parse_review_baseline(review_baseline_json);
    actionable_review_feedback(
        details,
        ReviewFeedbackFilter {
            after: baseline
                .as_ref()
                .map(|baseline| baseline.updated_at.as_str()),
            authors: &[],
        },
    )
}

fn review_body_item(review: &PrReview) -> ActionableReviewItem {
    ActionableReviewItem::ReviewBody {
        review_id: review.id.clone(),
        author: review.author.clone(),
        state: review.state.clone(),
        body: review.body.clone(),
        submitted_at: review.submitted_at.clone(),
    }
}

fn review_thread_fact(comment: &PrReviewComment) -> ReviewThreadFact {
    ReviewThreadFact {
        thread_id: comment.thread_id.clone(),
        comment_id: comment.id.clone(),
        path: comment.path.clone(),
        line: comment.line.parse().ok(),
        body: comment.body.clone(),
        author: comment.author.clone(),
        resolved: comment.resolved,
        created_at: comment.created_at.clone(),
    }
}

fn mergeability_facts(
    summary: &PrSummary,
    merge_conflict: Option<&crate::verify::VerifyCheckResult>,
) -> MergeabilityFacts {
    if let Some(check) = merge_conflict
        && !check.passed
    {
        return MergeabilityFacts::Blocked {
            reason: check.message.clone(),
        };
    }

    match summary
        .merge_state_status
        .trim()
        .to_ascii_uppercase()
        .as_str()
    {
        "CLEAN" | "HAS_HOOKS" | "UNSTABLE" => MergeabilityFacts::Clean,
        "" | "UNKNOWN" => MergeabilityFacts::Unknown,
        other => MergeabilityFacts::Blocked {
            reason: format!("GitHub merge state is {other}"),
        },
    }
}

fn pull_request_state(summary: &PrSummary) -> PullRequestState {
    if summary.merged {
        return PullRequestState::Merged;
    }
    match summary.state.trim().to_ascii_uppercase().as_str() {
        "OPEN" => PullRequestState::Open,
        "CLOSED" => PullRequestState::Closed,
        "MERGED" => PullRequestState::Merged,
        _ => PullRequestState::Unknown,
    }
}

fn status_label_dirty(status_label: &str) -> bool {
    status_label
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|parts| parts[0] == "dirty" && parts[1].parse::<usize>().unwrap_or(0) > 0)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    #[cfg(unix)]
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::agent::AgentState;
    use crate::config::{AutoConfig, Config};
    use crate::github::{
        CiFailure, PrCache, PrCheckContext, PrCheckState, PrComment, PrDetails, PrReview,
        PrReviewComment, PrSummary, RepoPolicyCache, save_repo_policy_cache,
    };
    use crate::repo::Repository;
    use crate::session::{Session, SessionClassification};
    use crate::test_support::write_executable;

    use super::*;

    #[test]
    fn old_review_body_is_not_actionable_after_baseline() {
        let summary = test_summary();
        let details = PrDetails {
            reviews: vec![PrReview {
                id: "review-1".to_string(),
                author: "reviewer".to_string(),
                state: "CHANGES_REQUESTED".to_string(),
                body: "old request".to_string(),
                submitted_at: "2026-01-01T00:00:00Z".to_string(),
            }],
            ..PrDetails::default()
        };
        let config = test_config(false);

        let facts = review_facts(
            &summary,
            Some(&details),
            &config,
            Some(r#"{"head_sha":"head","updated_at":"2026-01-01T00:01:00Z"}"#),
        );

        assert!(facts.actionable_reviews.is_empty());
    }

    #[test]
    fn copilot_review_overview_does_not_block_stabilization() {
        let summary = test_summary();
        let details = PrDetails {
            reviews: vec![PrReview {
                id: "review-1".to_string(),
                author: "copilot-pull-request-reviewer".to_string(),
                state: "COMMENTED".to_string(),
                body: "## Pull request overview\n\nCopilot reviewed 2 out of 2 files.".to_string(),
                submitted_at: "2026-01-01T00:00:00Z".to_string(),
            }],
            ..PrDetails::default()
        };

        let facts = review_facts(&summary, Some(&details), &test_config(false), None);

        assert!(facts.actionable_reviews.is_empty());
        assert!(facts.unresolved_threads.is_empty());
    }

    #[test]
    fn review_guard_population_excludes_old_non_actionable_unresolved_threads() {
        let summary = test_summary();
        let details = PrDetails {
            review_comments: vec![
                PrReviewComment {
                    thread_id: "thread-old".to_string(),
                    id: "old".to_string(),
                    author: "reviewer".to_string(),
                    path: "src/lib.rs".to_string(),
                    line: "1".to_string(),
                    body: "old unresolved feedback".to_string(),
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    resolved: false,
                },
                PrReviewComment {
                    thread_id: "thread-new".to_string(),
                    id: "new".to_string(),
                    author: "reviewer".to_string(),
                    path: "src/lib.rs".to_string(),
                    line: "2".to_string(),
                    body: "new actionable feedback".to_string(),
                    created_at: "2026-01-01T00:02:00Z".to_string(),
                    resolved: false,
                },
            ],
            ..PrDetails::default()
        };
        let config = test_config(false);

        let facts = review_facts(
            &summary,
            Some(&details),
            &config,
            Some(r#"{"head_sha":"head","updated_at":"2026-01-01T00:01:00Z"}"#),
        );

        assert_eq!(facts.unresolved_threads.len(), 1);
        assert_eq!(facts.unresolved_threads[0].thread_id, "thread-new");
        let feedback = stabilization_review_feedback(
            &details,
            Some(r#"{"head_sha":"head","updated_at":"2026-01-01T00:01:00Z"}"#),
        );
        assert_eq!(
            crate::review::review_thread_ids(&feedback),
            vec!["thread-new".to_string()]
        );
    }

    #[test]
    fn later_approval_addresses_earlier_review_body() {
        let summary = test_summary();
        let details = PrDetails {
            reviews: vec![
                PrReview {
                    id: "review-1".to_string(),
                    author: "reviewer".to_string(),
                    state: "CHANGES_REQUESTED".to_string(),
                    body: "please fix".to_string(),
                    submitted_at: "2026-01-01T00:00:00Z".to_string(),
                },
                PrReview {
                    id: "review-2".to_string(),
                    author: "reviewer".to_string(),
                    state: "APPROVED".to_string(),
                    body: "looks good".to_string(),
                    submitted_at: "2026-01-01T00:01:00Z".to_string(),
                },
            ],
            ..PrDetails::default()
        };
        let config = test_config(false);

        let facts = review_facts(&summary, Some(&details), &config, None);

        assert!(facts.actionable_reviews.is_empty());
    }

    #[test]
    fn later_commented_review_does_not_erase_requested_changes() {
        let summary = test_summary();
        let details = PrDetails {
            reviews: vec![
                PrReview {
                    id: "review-1".to_string(),
                    author: "reviewer".to_string(),
                    state: "CHANGES_REQUESTED".to_string(),
                    body: "please fix".to_string(),
                    submitted_at: "2026-01-01T00:00:00Z".to_string(),
                },
                PrReview {
                    id: "review-2".to_string(),
                    author: "reviewer".to_string(),
                    state: "COMMENTED".to_string(),
                    body: "one more note".to_string(),
                    submitted_at: "2026-01-01T00:01:00Z".to_string(),
                },
            ],
            ..PrDetails::default()
        };
        let config = test_config(false);

        let facts = review_facts(&summary, Some(&details), &config, None);

        assert!(facts.actionable_reviews.iter().any(|review| {
            matches!(
                review,
                ActionableReviewItem::ReviewBody { state, body, .. }
                    if state == "CHANGES_REQUESTED" && body == "please fix"
            )
        }));
    }

    #[test]
    fn pull_request_facts_keep_top_level_comments_advisory() {
        let cache = PrCache::observed(
            test_summary(),
            Some(PrDetails {
                comments: vec![PrComment {
                    id: "c1".to_string(),
                    author: "alice".to_string(),
                    body: "top level advisory".to_string(),
                    created_at: "2026-07-01T00:00:00Z".to_string(),
                }],
                reviews: vec![PrReview {
                    id: "r1".to_string(),
                    author: "bob".to_string(),
                    state: "CHANGES_REQUESTED".to_string(),
                    body: "please adjust".to_string(),
                    submitted_at: "2026-07-01T00:01:00Z".to_string(),
                }],
                review_comments: vec![PrReviewComment {
                    thread_id: "thread-1".to_string(),
                    id: "rc1".to_string(),
                    author: "carol".to_string(),
                    path: "src/lib.rs".to_string(),
                    line: "42".to_string(),
                    body: "inline fix".to_string(),
                    created_at: "2026-07-01T00:02:00Z".to_string(),
                    resolved: false,
                }],
                failing_checks: vec!["lint".to_string()],
                ci_failures: vec![CiFailure {
                    workflow: "ci".to_string(),
                    name: "lint".to_string(),
                    conclusion: "FAILURE".to_string(),
                    url: "https://example.test/run".to_string(),
                    run_id: "1".to_string(),
                    log_tail: "failed".to_string(),
                }],
                files: Vec::new(),
                check_contexts: Vec::new(),
            }),
        );

        let facts = pull_request_facts_from_cache(
            &cache,
            &test_config(false),
            Some("base123".to_string()),
            None,
            None,
        )
        .unwrap();

        assert_eq!(facts.number, 123);
        assert_eq!(facts.state, PullRequestState::Open);
        assert_eq!(facts.base_sha.as_deref(), Some("base123"));
        assert_eq!(facts.ci.aggregate, PrCheckState::Failed);
        assert_eq!(facts.ci.optional_failures, vec!["lint".to_string()]);
        assert_eq!(facts.ci.failures.len(), 1);
        assert_eq!(facts.review.top_level_comments, 1);
        assert_eq!(facts.review.actionable_reviews.len(), 2);
        assert_eq!(facts.review.unresolved_threads.len(), 1);
        assert_eq!(facts.review.unresolved_threads[0].thread_id, "thread-1");
    }

    #[test]
    fn pull_request_facts_apply_configured_review_approval_requirement() {
        let cache = PrCache::observed(test_summary(), None);

        let facts =
            pull_request_facts_from_cache(&cache, &test_config(true), None, None, None).unwrap();

        assert!(facts.review.approval_required);
        assert!(facts.review.actionable_reviews.is_empty());
        assert_eq!(facts.mergeability, MergeabilityFacts::Clean);
    }

    #[test]
    fn pull_request_facts_use_policy_required_checks_and_optional_failures() {
        let cache = PrCache::observed(
            PrSummary {
                check_status: "failed".to_string(),
                ..test_summary()
            },
            Some(PrDetails {
                failing_checks: vec!["docs".to_string()],
                check_contexts: vec![
                    PrCheckContext {
                        name: "ci".to_string(),
                        state: PrCheckState::Success,
                    },
                    PrCheckContext {
                        name: "lint".to_string(),
                        state: PrCheckState::Pending,
                    },
                ],
                ..PrDetails::default()
            }),
        );
        let policy = RepoPolicyCache {
            required_checks: vec!["ci".to_string(), "lint".to_string(), "missing".to_string()],
            ..RepoPolicyCache::default()
        };

        let facts =
            pull_request_facts_from_cache(&cache, &test_config(false), None, None, Some(&policy))
                .unwrap();

        assert_eq!(facts.ci.required.len(), 3);
        assert_eq!(facts.ci.required[0].name, "ci");
        assert_eq!(facts.ci.required[0].state, PrCheckState::Success);
        assert_eq!(facts.ci.required[1].name, "lint");
        assert_eq!(facts.ci.required[1].state, PrCheckState::Pending);
        assert_eq!(facts.ci.required[2].name, "missing");
        assert_eq!(facts.ci.required[2].state, PrCheckState::Unknown);
        assert_eq!(facts.ci.optional_failures, vec!["docs".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_construction_combines_run_session_git_and_pr_cache_facts() {
        let temp = unique_temp_dir("prism-stabilization-snapshot-test");
        fs::create_dir_all(&temp).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            r#"#!/bin/sh
case "$*" in
  *"remote get-url origin"*) printf '%s\n' 'git@github.com:owner/repo.git'; exit 0 ;;
  *"rev-parse HEAD"*) printf '%s\n' 'local123'; exit 0 ;;
  *"rev-parse --verify --quiet refs/remotes/origin/feature"*) printf '%s\n' 'remote123'; exit 0 ;;
  *"rev-parse --verify --quiet refs/remotes/origin/main"*) printf '%s\n' 'base123'; exit 0 ;;
  *"status --short"*) exit 0 ;;
  *"fetch origin main"*) exit 0 ;;
  *"merge-tree --write-tree HEAD origin/main"*) printf '%s\n' 'tree123'; exit 0 ;;
esac
exit 1
"#,
        );
        let mut config = test_config(false);
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let worktree = temp.join("worktree");
        let run = super::super::AutoLaunch::new(&repo.root, &worktree, "feature", "Implement")
            .unwrap()
            .create_run()
            .run;
        let session = Session {
            repo_index: 0,
            repo_label: "repo".to_string(),
            repo_key: None,
            path: worktree.clone(),
            incarnation: String::new(),
            path_display: worktree.display().to_string(),
            branch: "feature".to_string(),
            prompt_summary: String::new(),
            classification: SessionClassification::Work,
            visibility: 0,
            adopted: true,
            hidden: false,
            status_label: "clean".to_string(),
            agent_state: AgentState::Idle,
            opencode_status: None,
            pr: PrCache::observed(
                test_summary(),
                Some(PrDetails {
                    review_comments: vec![PrReviewComment {
                        thread_id: "thread-1".to_string(),
                        id: "comment-1".to_string(),
                        author: "reviewer".to_string(),
                        path: "src/lib.rs".to_string(),
                        line: "7".to_string(),
                        body: "please fix".to_string(),
                        created_at: "2026-07-01T00:02:00Z".to_string(),
                        resolved: false,
                    }],
                    ..PrDetails::default()
                }),
            ),
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        };

        let snapshot = build_stabilization_snapshot(&repo, &session, Some(&run), &config);

        assert_eq!(
            snapshot.run.as_ref().map(|run| run.id.as_str()),
            Some(run.id.as_str())
        );
        assert_eq!(snapshot.repository.root, repo.root);
        assert_eq!(
            snapshot.repository.github_remote.as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            snapshot.worktree.local_head_sha.as_deref(),
            Some("local123")
        );
        assert_eq!(
            snapshot.worktree.remote_head_sha.as_deref(),
            Some("remote123")
        );
        assert!(!snapshot.worktree.dirty);
        assert_eq!(snapshot.goal.auto_merge, config.auto.merge);
        assert!(matches!(snapshot.policy, PolicyFacts::Unknown { .. }));
        let pull_request = snapshot.pull_request.unwrap();
        assert_eq!(pull_request.base_sha.as_deref(), Some("base123"));
        assert_eq!(pull_request.mergeability, MergeabilityFacts::Clean);
        assert_eq!(
            pull_request.review.unresolved_threads[0].thread_id,
            "thread-1"
        );

        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_uses_cached_policy_and_required_approvals() {
        let temp = unique_temp_dir("prism-stabilization-policy-test");
        fs::create_dir_all(&temp).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            r#"#!/bin/sh
case "$*" in
  *"remote get-url origin"*) printf '%s\n' 'git@github.com:owner/repo.git'; exit 0 ;;
  *"rev-parse HEAD"*) printf '%s\n' 'local123'; exit 0 ;;
  *"rev-parse --verify --quiet refs/remotes/origin/feature"*) printf '%s\n' 'remote123'; exit 0 ;;
  *"rev-parse --verify --quiet refs/remotes/origin/main"*) printf '%s\n' 'base123'; exit 0 ;;
  *"status --short"*) exit 0 ;;
  *"fetch origin main"*) exit 0 ;;
  *"merge-tree --write-tree HEAD origin/main"*) printf '%s\n' 'tree123'; exit 0 ;;
esac
exit 1
"#,
        );
        let mut config = test_config(false);
        config.default_base = Some("configured-but-stale".to_string());
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        save_repo_policy_cache(
            &repo,
            &RepoPolicyCache {
                repo_remote: "owner/repo".to_string(),
                default_branch: Some("main".to_string()),
                required_approvals: 1,
                refreshed_unix_ms: 123,
                ..RepoPolicyCache::default()
            },
        )
        .unwrap();
        let session = Session {
            repo_index: 0,
            repo_label: "repo".to_string(),
            repo_key: None,
            path: temp.join("worktree"),
            incarnation: String::new(),
            path_display: temp.join("worktree").display().to_string(),
            branch: "feature".to_string(),
            prompt_summary: String::new(),
            classification: SessionClassification::Work,
            visibility: 0,
            adopted: true,
            hidden: false,
            status_label: "clean".to_string(),
            agent_state: AgentState::Idle,
            opencode_status: None,
            pr: PrCache::observed(test_summary(), None),
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        };

        let snapshot = build_stabilization_snapshot(&repo, &session, None, &config);

        assert_eq!(snapshot.repository.policy_refreshed_unix_ms, Some(123));
        assert_eq!(snapshot.repository.default_base.as_deref(), Some("main"));
        assert_eq!(snapshot.repository.policy_error, None);
        assert_eq!(
            snapshot.policy,
            PolicyFacts::Blocked {
                blockers: vec![PolicyBlocker::RequiredApprovalMissing]
            }
        );
        assert!(snapshot.pull_request.unwrap().review.approval_required);

        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[test]
    fn headless_policy_refresh_failure_does_not_reuse_satisfied_cache() {
        let temp = unique_temp_dir("prism-headless-policy-refresh-failure-test");
        fs::create_dir_all(&temp).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            "#!/bin/sh\ncase \"$*\" in\n  *\"remote get-url origin\"*) printf '%s\\n' 'git@github.com:owner/repo.git'; exit 0 ;;\nesac\nexit 1\n",
        );
        let gh = temp.join("gh");
        write_executable(
            &gh,
            "#!/bin/sh\nif [ \"$1\" = api ] && [ \"$2\" = graphql ]; then\n  printf '%s\\n' '{\"data\":{\"repository\":{\"defaultBranchRef\":{\"name\":\"main\"},\"branchProtectionRules\":{\"nodes\":[]}}}}'\n  exit 0\nfi\nexit 1\n",
        );
        let mut config = test_config(true);
        config.auto.merge = true;
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        save_repo_policy_cache(
            &repo,
            &RepoPolicyCache {
                repo_remote: "owner/repo".to_string(),
                default_branch: Some("main".to_string()),
                refreshed_unix_ms: 123,
                ..RepoPolicyCache::default()
            },
        )
        .unwrap();
        crate::observability::with_writable_db(&repo, |conn| {
            conn.execute_batch(
                "create trigger reject_policy_refresh before update on repo_policy_cache
                 begin select raise(abort, 'policy refresh rejected'); end;",
            )
            .map_err(|error| error.to_string())
        })
        .unwrap();
        let run = super::super::AutoLaunch::new(
            &repo.root,
            &temp.join("worktree"),
            "feature",
            "Implement",
        )
        .unwrap()
        .create_run()
        .run;

        let snapshot = build_auto_run_stabilization_snapshot(&repo, &run, &config);

        assert!(matches!(
            snapshot.policy,
            PolicyFacts::Unknown { ref reason }
                if reason.as_deref().is_some_and(|reason| reason.contains("policy refresh rejected"))
        ));
        assert!(
            snapshot
                .repository
                .policy_error
                .as_deref()
                .is_some_and(|error| error.contains("policy refresh rejected"))
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[test]
    fn phase_1_mergeability_conflict_check_uses_actual_pr_base() {
        let temp = unique_temp_dir("prism-stabilization-pr-base-test");
        fs::create_dir_all(&temp).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            r#"#!/bin/sh
case "$*" in
  *"remote get-url origin"*) printf '%s\n' 'git@github.com:owner/repo.git'; exit 0 ;;
  *"rev-parse HEAD"*) printf '%s\n' 'head123'; exit 0 ;;
  *"rev-parse --verify --quiet refs/remotes/origin/feature"*) printf '%s\n' 'head123'; exit 0 ;;
  *"rev-parse --verify --quiet refs/remotes/origin/release"*) printf '%s\n' 'base123'; exit 0 ;;
  *"status --short"*) exit 0 ;;
  *"fetch origin main"*) exit 0 ;;
  *"fetch origin release"*) exit 0 ;;
  *"merge-tree --write-tree HEAD origin/main"*) printf '%s\n' 'tree123'; exit 0 ;;
  *"merge-tree --write-tree HEAD origin/release"*) printf '%s\n' 'conflict' >&2; exit 1 ;;
esac
exit 1
"#,
        );
        let mut config = test_config(false);
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let mut summary = test_summary();
        summary.base_ref = "release".to_string();
        let session = Session {
            repo_index: 0,
            repo_label: "repo".to_string(),
            repo_key: None,
            path: temp.join("worktree"),
            incarnation: String::new(),
            path_display: temp.join("worktree").display().to_string(),
            branch: "feature".to_string(),
            prompt_summary: String::new(),
            classification: SessionClassification::Work,
            visibility: 0,
            adopted: true,
            hidden: false,
            status_label: "clean".to_string(),
            agent_state: AgentState::Idle,
            opencode_status: None,
            pr: PrCache::observed(summary, None),
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        };

        let snapshot = build_stabilization_snapshot(&repo, &session, None, &config);
        let pull_request = snapshot.pull_request.unwrap();

        assert_eq!(pull_request.base_ref, "release");
        assert!(matches!(
            pull_request.mergeability,
            MergeabilityFacts::Blocked { ref reason } if reason.contains("origin/release")
        ));

        let _ = fs::remove_dir_all(temp);
    }

    fn test_summary() -> PrSummary {
        PrSummary {
            number: 123,
            title: "Title".to_string(),
            body: String::new(),
            url: "https://example.test/pr/123".to_string(),
            state: "OPEN".to_string(),
            review_decision: "REVIEW_REQUIRED".to_string(),
            requested_reviewers: Vec::new(),
            head_ref: "feature".to_string(),
            base_ref: "main".to_string(),
            head_sha: "head123".to_string(),
            updated_at: "2026-07-01T00:00:00Z".to_string(),
            check_status: "failed".to_string(),
            merge_state_status: "CLEAN".to_string(),
            comment_count: 3,
            merged: false,
            draft: false,
        }
    }

    fn test_config(require_review_approval: bool) -> Config {
        let mut config = crate::test_support::test_config();
        config.default_agent = "opencode".to_string();
        config.default_base = Some("main".to_string());
        config.auto = AutoConfig {
            require_review_approval,
            ..AutoConfig::default()
        };
        config
    }

    #[cfg(unix)]
    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
