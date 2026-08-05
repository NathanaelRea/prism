#![allow(dead_code)]

use std::collections::BTreeSet;

use crate::config::Config;
use crate::git;
use crate::remote::{PrCache, PrDetails, PrReview, PrReviewComment, PrSummary, RepoPolicyCache};
use crate::repo::Repository;
use crate::review::{ReviewFeedback, ReviewFeedbackFilter, actionable_review_feedback};
use crate::session::Session;
use crate::verify::{VerifyCheckKind, VerifyCheckResult, run_merge_conflict_check_against_remote};

use super::stabilization_model::*;

pub(super) fn push_remote_head_sha(
    path: &std::path::Path,
    branch: &str,
    config: &Config,
) -> Result<Option<String>, String> {
    let source_push = crate::remote::dispatcher::prepare_push(path, config, branch)?;
    git::push_remote_branch_head_sha(
        path,
        &source_push.remote,
        &source_push.remote_branch,
        config,
    )
}

pub(super) fn reauthorize_pending_push_cache(
    cache: &mut PrCache,
    path: &std::path::Path,
    guard: &PendingPushGuard,
    config: &Config,
) {
    let local_head = git::current_head_sha(path, config).ok();
    if local_head.as_deref() != Some(guard.expected_local_head_sha.as_str()) {
        return;
    }
    let Some(identity) = guard.change_request_identity.as_ref() else {
        return;
    };
    if let Some(pr_head) = guard.expected_pr_head_sha.as_deref() {
        cache.reauthorize_guarded_summary(identity, pr_head);
    }
    if !guard.commit_sha.is_empty() {
        cache.reauthorize_guarded_summary(identity, &guard.commit_sha);
    }
}

pub(crate) fn build_stabilization_snapshot(
    repo: &Repository,
    session: &Session,
    run: Option<&super::AutoRun>,
    config: &Config,
) -> StabilizationSnapshot {
    let local_head_sha = git::current_head_sha(&session.path, config).ok();
    let remote_head_sha = push_remote_head_sha(&session.path, &session.branch, config)
        .ok()
        .flatten();
    let remote = crate::remote::discover_git_remote(
        &session.path,
        config,
        "origin",
        crate::remote::RemoteUrlKind::Fetch,
    )
    .ok();
    let target_repository = session
        .pr
        .summary()
        .and_then(|summary| summary.change_request_identity.as_ref())
        .and_then(|identity| identity.target_repository().ok())
        .or_else(|| remote.as_ref().map(|remote| remote.repository.id.clone()));
    let remote_project = target_repository
        .as_ref()
        .map(|repository| repository.project_path().to_string());
    let policy_cache = target_repository.as_ref().and_then(|repository| {
        crate::remote::load_repo_policy_cache_for_repository(repo, repository)
    });
    let target_remote = target_remote_name(&session.path, config, target_repository.as_ref());
    let merge_conflict = session.pr.summary().and_then(|summary| {
        (!session.is_default_branch(config) && !session.is_detached()).then(|| {
            target_remote.as_deref().map_or_else(
                |error| target_remote_check_error(error),
                |remote| {
                    run_merge_conflict_check_against_remote(
                        config,
                        &session.path,
                        remote,
                        &summary.base_ref,
                    )
                },
            )
        })
    });
    let base_sha = session.pr.summary().and_then(|summary| {
        target_remote.as_ref().ok().and_then(|remote| {
            git::remote_branch_head_sha_on(&session.path, remote, &summary.base_ref, config)
                .ok()
                .flatten()
        })
    });
    let (worktree_dirty, worktree_observation_error) =
        observe_worktree_dirty(&session.path, config);

    let mut pull_request = pull_request_facts_from_cache_with_baseline(
        &session.pr,
        config,
        base_sha,
        merge_conflict.as_ref(),
        policy_cache.as_ref(),
        run.and_then(|run| run.review_baseline_json.as_deref()),
    );
    record_worktree_observation_error(&mut pull_request, worktree_observation_error);
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
            remote_project,
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
            dirty: worktree_dirty,
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
    let mut cache = crate::remote::load_pr_cache(repo, &run.branch);
    if let Some(guard) = run.pending_push.as_ref() {
        reauthorize_pending_push_cache(&mut cache, &run.worktree_path, guard, config);
    }
    let _ = crate::remote::dispatcher::refresh_change_request_cache(
        repo,
        &run.branch,
        &mut cache,
        &run.worktree_path,
        config,
        true,
    );
    let target_repository = cache
        .summary()
        .and_then(|summary| summary.change_request_identity.as_ref())
        .and_then(|identity| identity.target_repository().ok());
    let policy_refresh_error = if config.auto.merge {
        crate::remote::dispatcher::refresh_repository_policy_for(
            repo,
            &run.worktree_path,
            config,
            target_repository.as_ref(),
        )
        .err()
    } else {
        None
    };
    let local_head_sha = git::current_head_sha(&run.worktree_path, config).ok();
    let remote_head_sha = push_remote_head_sha(&run.worktree_path, &run.branch, config)
        .ok()
        .flatten();
    let remote = crate::remote::discover_git_remote(
        &run.worktree_path,
        config,
        "origin",
        crate::remote::RemoteUrlKind::Fetch,
    )
    .ok();
    let target_repository =
        target_repository.or_else(|| remote.as_ref().map(|remote| remote.repository.id.clone()));
    let remote_project = target_repository
        .as_ref()
        .map(|repository| repository.project_path().to_string());
    let policy_cache = target_repository.as_ref().and_then(|repository| {
        crate::remote::load_repo_policy_cache_for_repository(repo, repository)
    });
    let is_default_branch = config.is_default_branch(&run.branch)
        || policy_cache
            .as_ref()
            .and_then(|policy| policy.default_branch.as_deref())
            == Some(run.branch.as_str());
    let detached = run.branch == "(detached)";
    let target_remote = target_remote_name(&run.worktree_path, config, target_repository.as_ref());
    let merge_conflict = cache.summary().and_then(|summary| {
        (!is_default_branch && !detached).then(|| {
            target_remote.as_deref().map_or_else(
                |error| target_remote_check_error(error),
                |remote| {
                    run_merge_conflict_check_against_remote(
                        config,
                        &run.worktree_path,
                        remote,
                        &summary.base_ref,
                    )
                },
            )
        })
    });
    let base_sha = cache.summary().and_then(|summary| {
        target_remote.as_ref().ok().and_then(|remote| {
            git::remote_branch_head_sha_on(&run.worktree_path, remote, &summary.base_ref, config)
                .ok()
                .flatten()
        })
    });
    let (worktree_dirty, worktree_observation_error) =
        observe_worktree_dirty(&run.worktree_path, config);

    let mut pull_request = pull_request_facts_from_cache_with_baseline(
        &cache,
        config,
        base_sha,
        merge_conflict.as_ref(),
        policy_cache.as_ref(),
        run.review_baseline_json.as_deref(),
    );
    record_worktree_observation_error(&mut pull_request, worktree_observation_error);
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
            remote_project,
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
            dirty: worktree_dirty,
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

fn target_remote_name(
    path: &std::path::Path,
    config: &Config,
    target_repository: Option<&crate::remote::RemoteRepositoryId>,
) -> Result<String, String> {
    match target_repository {
        Some(repository) => {
            crate::remote::dispatcher::fetch_remote_name_for_repository(path, config, repository)
        }
        None => Ok("origin".to_string()),
    }
}

fn target_remote_check_error(error: &str) -> VerifyCheckResult {
    VerifyCheckResult {
        kind: VerifyCheckKind::MergeConflict,
        label: "merge conflict".to_string(),
        passed: false,
        message: format!("change request target remote is unavailable: {error}"),
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
        if policy.required_approvals > 0 && !pull_request.review.approval_requirement_satisfied() {
            blockers.push(PolicyBlocker::RequiredApprovalMissing);
        }
        for check in &pull_request.ci.required {
            match check.state {
                crate::remote::PrCheckState::Unknown => {
                    blockers.push(PolicyBlocker::RequiredCheckMissing(check.name.clone()));
                }
                crate::remote::PrCheckState::Failed | crate::remote::PrCheckState::Mixed => {
                    blockers.push(PolicyBlocker::RequiredCheckFailing(check.name.clone()));
                }
                crate::remote::PrCheckState::Pending | crate::remote::PrCheckState::Success => {}
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
    let trusted_details = cache.trusted_details().ok().flatten();
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
        change_request_identity: summary.change_request_identity.clone(),
        url: summary.url.clone(),
        state: pull_request_state(summary),
        draft: summary.draft,
        head_sha: summary.head_sha.clone(),
        base_ref: summary.base_ref.clone(),
        base_sha,
        updated_at: summary.updated_at.clone(),
        ci: ci_facts(summary, details, policy),
        review: review_facts(
            summary,
            details,
            trusted_details,
            config,
            policy,
            review_baseline_json,
        ),
        mergeability: mergeability_facts(summary, merge_conflict),
        queue_state: crate::remote::QueueState::from_native(summary.queue_state.clone()),
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
                    .unwrap_or(crate::remote::PrCheckState::Unknown),
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
    trusted_details: Option<&PrDetails>,
    config: &Config,
    policy: Option<&RepoPolicyCache>,
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

    let required_approvals = policy
        .filter(|policy| {
            policy
                .error
                .as_ref()
                .is_none_or(|error| error.trim().is_empty())
        })
        .map(|policy| policy.required_approvals)
        .unwrap_or(0);
    let approved_reviewers = trusted_details
        .into_iter()
        .flat_map(|details| &details.reviews)
        .filter(|review| review.state.eq_ignore_ascii_case("APPROVED"))
        .filter_map(|review| {
            let author = review.author.trim();
            (!author.is_empty()).then(|| author.to_ascii_lowercase())
        })
        .collect::<BTreeSet<_>>();

    ReviewFacts {
        decision: summary.review_decision.clone(),
        approval_required: config.auto.require_review_approval || required_approvals > 0,
        approval_count: approved_reviewers.len() as u64,
        required_approvals,
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

fn observe_worktree_dirty(path: &std::path::Path, config: &Config) -> (bool, Option<String>) {
    match git::selected_dirty(path, config) {
        Ok(dirty) => (dirty, None),
        Err(error) => (true, Some(format!("git status inspection failed: {error}"))),
    }
}

fn record_worktree_observation_error(
    pull_request: &mut Option<PullRequestFacts>,
    worktree_observation_error: Option<String>,
) {
    let (Some(pull_request), Some(error)) = (pull_request, worktree_observation_error) else {
        return;
    };
    pull_request.observation_error = Some(match pull_request.observation_error.take() {
        Some(existing) => format!("{existing}; {error}"),
        None => error,
    });
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
    use crate::remote::{
        CachedCiFailure, PrCache, PrCheckContext, PrCheckState, PrComment, PrDetails, PrReview,
        PrReviewComment, PrSummary, ProviderKind, RepoPolicyCache, record_pr_summary,
        save_pr_cache, save_pr_details_cache, save_repo_policy_cache,
    };
    use crate::repo::Repository;
    use crate::session::{Session, SessionClassification};
    use crate::test_support::write_executable;

    use super::*;

    #[cfg(unix)]
    #[test]
    fn push_remote_head_uses_the_configured_remote_branch() {
        let temp = unique_temp_dir("prism-push-remote-head-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("git.log");
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"branch --show-current"*) printf '%s\n' 'topic' ;;
  *"for-each-ref --format=%(push:remotename)%00%(push) refs/heads/topic"*) printf 'publish\000refs/remotes/publish/review/topic\n' ;;
  *"remote get-url --push --all publish"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url publish --push"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"rev-parse HEAD"*) printf '%s\n' 'local-head' ;;
  *"ls-remote --exit-code --heads https://github.com/contributor/widget.git refs/heads/review/topic"*) printf '%s\t%s\n' 'remote-head' 'refs/heads/review/topic' ;;
  *) exit 1 ;;
esac
"#,
                log.display()
            ),
        );
        let mut config = test_config(false);
        config
            .tools
            .insert("git".to_string(), git.display().to_string());

        assert_eq!(
            push_remote_head_sha(&temp, "topic", &config).unwrap(),
            Some("remote-head".to_string())
        );
        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.contains("refs/heads/review/topic"));
        assert!(!commands.contains("refs/remotes/origin/topic"));
        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[test]
    fn pending_push_snapshot_keeps_a_freshly_merged_repair_head() {
        let temp = unique_temp_dir("prism-merged-repair-snapshot-test");
        fs::create_dir_all(&temp).unwrap();
        let git = temp.join("git");
        let gh = temp.join("gh");
        write_executable(
            &git,
            r#"#!/bin/sh
case "$*" in
  *"branch --show-current"*) printf '%s\n' 'feature' ;;
  *"for-each-ref --format=%(push:remotename)%00%(push) refs/heads/feature"*) printf 'origin\000refs/remotes/origin/feature\n' ;;
  *"remote get-url --push --all origin"*|*"remote get-url origin"*) printf '%s\n' 'https://github.com/example/repo.git' ;;
  *"rev-parse HEAD"*) printf '%s\n' 'repair-head' ;;
  *"ls-remote --exit-code --heads https://github.com/example/repo.git refs/heads/feature"*) printf '%s\t%s\n' 'repair-head' 'refs/heads/feature' ;;
  *"fetch --no-tags origin +refs/heads/main:refs/remotes/origin/main"*) exit 0 ;;
  *"rev-parse --verify --quiet refs/remotes/origin/main"*) printf '%s\n' 'base-head' ;;
  *"merge-tree --write-tree HEAD origin/main"*) printf '%s\n' 'tree-head' ;;
  *"status --short"*) exit 0 ;;
  *) exit 1 ;;
esac
"#,
        );
        write_executable(
            &gh,
            r#"#!/bin/sh
case "$*" in
  *"reviewThreads(first: 100"*) printf '%s\n' '[{"data":{"repository":{"pullRequest":{"reviewThreads":{"totalCount":0,"pageInfo":{"hasNextPage":false},"nodes":[]}}}}}]' ;;
  *"/issues/42/comments?per_page=100"*|*"/pulls/42/reviews?per_page=100"*|*"/pulls/42/files?per_page=100"*|*"/commits/repair-head/statuses?per_page=100"*) printf '%s\n' '[[]]' ;;
  *"/commits/repair-head/check-runs?per_page=100"*) printf '%s\n' '[{"total_count":0,"check_runs":[]}]' ;;
  "run list "*) printf '%s\n' '[]' ;;
  api\ graphql*) printf '%s\n' '{"data":{"repository":{"pullRequest":{"id":"PR_test","number":42,"title":"Repair","state":"MERGED","mergedAt":"2026-08-01T00:00:00Z","headRefName":"feature","baseRefName":"main","headRefOid":"repair-head","headRepository":{"nameWithOwner":"example/repo"},"baseRepository":{"nameWithOwner":"example/repo"}}}}}' ;;
  *) exit 1 ;;
esac
"#,
        );
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut config = test_config(false);
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());
        let mut summary = test_summary();
        summary.head_sha = "repair-head".to_string();
        summary.change_request_identity = Some(crate::remote::test_change_request_identity());
        save_pr_cache(
            &repo,
            "feature",
            &PrCache::observed(summary, Some(PrDetails::default())),
        )
        .unwrap();
        let mut run = super::super::AutoLaunch::new(&temp, &temp, "feature", "Repair")
            .unwrap()
            .create_run()
            .run;
        run.pending_push = Some(PendingPushGuard {
            change_request_identity: Some(crate::remote::test_change_request_identity()),
            repair_kind: RepairKind::Review,
            commit_sha: "repair-head".to_string(),
            expected_local_head_sha: "repair-head".to_string(),
            expected_remote_head_sha: Some("old-head".to_string()),
            pr_number: Some(42),
            expected_pr_head_sha: Some("old-head".to_string()),
            expected_base_sha: Some("base-head".to_string()),
            guarded_review_thread_ids: Vec::new(),
        });

        let snapshot = build_auto_run_stabilization_snapshot(&repo, &run, &config);

        assert_eq!(
            snapshot.pull_request.map(|request| request.state),
            Some(PullRequestState::Merged)
        );
        let _ = fs::remove_dir_all(repo.prism_dir());
        let _ = fs::remove_dir_all(temp);
    }

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
            Some(&details),
            &config,
            None,
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

        let facts = review_facts(
            &summary,
            Some(&details),
            Some(&details),
            &test_config(false),
            None,
            None,
        );

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
            Some(&details),
            &config,
            None,
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

        let facts = review_facts(
            &summary,
            Some(&details),
            Some(&details),
            &config,
            None,
            None,
        );

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

        let facts = review_facts(
            &summary,
            Some(&details),
            Some(&details),
            &config,
            None,
            None,
        );

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
                ci_failures: vec![CachedCiFailure {
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
    fn forgejo_zero_one_and_two_approval_policies_use_the_numeric_threshold() {
        let cases = [
            (0, "REVIEW_REQUIRED", Vec::new(), 0, true),
            (1, "APPROVED", vec![approved_review("alice", "1")], 1, true),
            (2, "APPROVED", vec![approved_review("alice", "1")], 1, false),
            (
                2,
                "APPROVED",
                vec![approved_review("alice", "1"), approved_review("bob", "2")],
                2,
                true,
            ),
        ];

        for (required, decision, reviews, expected_count, expected_satisfied) in cases {
            let (review, policy) =
                approval_facts(ProviderKind::Forgejo, required, decision, reviews);

            assert_eq!(review.required_approvals, required);
            assert_eq!(review.approval_count, expected_count);
            assert_eq!(review.approval_requirement_satisfied(), expected_satisfied);
            assert_eq!(matches!(policy, PolicyFacts::Satisfied), expected_satisfied);
        }
    }

    #[test]
    fn duplicate_and_stale_forgejo_reviews_do_not_inflate_approval_count() {
        let reviews = vec![
            approved_review("Alice", "1"),
            approved_review("alice", "2"),
            PrReview {
                id: "3".to_string(),
                author: "bob".to_string(),
                state: "stale".to_string(),
                body: String::new(),
                submitted_at: "2026-07-01T00:02:00Z".to_string(),
            },
        ];

        let (review, policy) = approval_facts(ProviderKind::Forgejo, 2, "APPROVED", reviews);

        assert_eq!(review.approval_count, 1);
        assert!(!review.approval_requirement_satisfied());
        assert!(matches!(policy, PolicyFacts::Blocked { .. }));
    }

    #[test]
    fn github_aggregate_approval_cannot_bypass_a_known_numeric_policy() {
        let (review, policy) = approval_facts(
            ProviderKind::GitHub,
            2,
            "APPROVED",
            vec![approved_review("alice", "1")],
        );

        assert_eq!(review.approval_count, 1);
        assert!(!review.approval_requirement_satisfied());
        assert!(matches!(policy, PolicyFacts::Blocked { .. }));
    }

    #[test]
    fn gitlab_aggregate_decision_remains_authoritative_after_count_is_met() {
        let reviews = vec![approved_review("alice", "1"), approved_review("bob", "2")];
        let (pending, pending_policy) =
            approval_facts(ProviderKind::GitLab, 2, "REVIEW_REQUIRED", reviews.clone());
        let (approved, approved_policy) =
            approval_facts(ProviderKind::GitLab, 2, "APPROVED", reviews);

        assert_eq!(pending.approval_count, 2);
        assert!(!pending.approval_requirement_satisfied());
        assert!(matches!(pending_policy, PolicyFacts::Blocked { .. }));
        assert!(approved.approval_requirement_satisfied());
        assert!(matches!(approved_policy, PolicyFacts::Satisfied));
    }

    #[test]
    fn approval_count_is_cleared_when_the_exact_head_changes() {
        let temp = unique_temp_dir("prism-approval-head-change-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let mut summary = test_summary();
        summary.review_decision = "APPROVED".to_string();
        let mut cache = PrCache::observed(
            summary.clone(),
            Some(PrDetails {
                reviews: vec![approved_review("alice", "1")],
                ..PrDetails::default()
            }),
        );
        let policy = RepoPolicyCache {
            provider: Some(ProviderKind::Forgejo),
            required_approvals: 1,
            ..RepoPolicyCache::default()
        };
        let before =
            pull_request_facts_from_cache(&cache, &test_config(false), None, None, Some(&policy))
                .unwrap();

        summary.head_sha = "new-head".to_string();
        summary.review_decision = "REVIEW_REQUIRED".to_string();
        record_pr_summary(&repo, "feature", &mut cache, summary);
        let after =
            pull_request_facts_from_cache(&cache, &test_config(false), None, None, Some(&policy))
                .unwrap();

        assert_eq!(before.review.approval_count, 1);
        assert_eq!(after.review.approval_count, 0);
        assert!(!after.review.approval_requirement_satisfied());
        assert!(after.observation_error.is_some());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn pull_request_facts_preserve_native_queue_state() {
        let mut summary = test_summary();
        summary.queue_state = "preparing_merged_result".to_string();
        let cache = PrCache::observed(summary, None);

        let facts =
            pull_request_facts_from_cache(&cache, &test_config(false), None, None, None).unwrap();

        assert_eq!(
            facts.queue_state,
            crate::remote::QueueState::Unknown("preparing_merged_result".to_string())
        );
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
  *"branch --show-current"*) printf '%s\n' 'feature'; exit 0 ;;
  *"for-each-ref --format=%(push:remotename)%00%(push) refs/heads/feature"*) printf 'origin\000refs/remotes/origin/feature\n'; exit 0 ;;
  *"remote get-url --push --all origin"*) printf '%s\n' 'git@github.com:owner/repo.git'; exit 0 ;;
  *"remote get-url origin"*) printf '%s\n' 'git@github.com:owner/repo.git'; exit 0 ;;
  *"ls-remote --exit-code --heads git@github.com:owner/repo.git refs/heads/feature"*) printf '%s\t%s\n' 'remote123' 'refs/heads/feature'; exit 0 ;;
  *"rev-parse HEAD"*) printf '%s\n' 'local123'; exit 0 ;;
  *"rev-parse --verify --quiet refs/remotes/origin/feature"*) printf '%s\n' 'remote123'; exit 0 ;;
  *"rev-parse --verify --quiet refs/remotes/origin/main"*) printf '%s\n' 'base123'; exit 0 ;;
  *"status --short"*) exit 0 ;;
  *"fetch --no-tags origin +refs/heads/main:refs/remotes/origin/main"*) exit 0 ;;
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
            worktree_session_id: "test-feature".to_string(),
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
            snapshot.repository.remote_project.as_deref(),
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
    fn failed_git_status_blocks_manual_merge_readiness_for_session_snapshot() {
        let temp = unique_temp_dir("prism-session-status-failure-test");
        fs::create_dir_all(&temp).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            r#"#!/bin/sh
case "$*" in
  *"remote get-url origin"*) printf '%s\n' 'git@github.com:owner/repo.git'; exit 0 ;;
  *"rev-parse HEAD"*) printf '%s\n' 'head123'; exit 0 ;;
  *"rev-parse --verify --quiet refs/remotes/origin/feature"*) printf '%s\n' 'head123'; exit 0 ;;
  *"rev-parse --verify --quiet refs/remotes/origin/main"*) printf '%s\n' 'base123'; exit 0 ;;
  *"status --short"*) printf '%s\n' 'status unavailable' >&2; exit 1 ;;
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
        save_repo_policy_cache(
            &repo,
            &RepoPolicyCache {
                repo_remote: "owner/repo".to_string(),
                provider: Some(ProviderKind::GitHub),
                canonical_host: Some("github.com".to_string()),
                project_path: Some("owner/repo".to_string()),
                target_branch: Some("main".to_string()),
                identity_complete: true,
                default_branch: Some("main".to_string()),
                ..RepoPolicyCache::default()
            },
        )
        .unwrap();
        let mut summary = test_summary();
        summary.check_status = "passed".to_string();
        summary.comment_count = 0;
        let worktree = temp.join("worktree");
        let session = Session {
            repo_index: 0,
            repo_label: "repo".to_string(),
            repo_key: None,
            path: worktree.clone(),
            worktree_session_id: "test-feature".to_string(),
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
            pr: PrCache::observed(summary, Some(PrDetails::default())),
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        };

        let snapshot = build_stabilization_snapshot(&repo, &session, None, &config);
        let blockers = super::super::stabilization_plan::derive_blockers(&snapshot);

        assert!(snapshot.worktree.dirty);
        assert!(
            snapshot
                .pull_request
                .as_ref()
                .and_then(|pull_request| pull_request.observation_error.as_deref())
                .is_some_and(|error| error.contains("git status inspection failed"))
        );
        assert_eq!(
            super::super::stabilization_plan::plan(&snapshot).blocker,
            StabilizationBlocker::DirtyWorktree
        );
        assert!(!blockers.contains(&StabilizationBlocker::ReadyForManualMerge));

        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[test]
    fn failed_git_status_blocks_auto_merge_readiness_for_persisted_run_snapshot() {
        let temp = unique_temp_dir("prism-auto-run-status-failure-test");
        fs::create_dir_all(&temp).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            r#"#!/bin/sh
case "$*" in
  *"remote get-url"*) exit 1 ;;
  *"rev-parse HEAD"*) printf '%s\n' 'head123'; exit 0 ;;
  *"rev-parse --verify --quiet refs/remotes/origin/feature"*) printf '%s\n' 'head123'; exit 0 ;;
  *"rev-parse --verify --quiet refs/remotes/origin/main"*) printf '%s\n' 'base123'; exit 0 ;;
  *"status --short"*) printf '%s\n' 'status unavailable' >&2; exit 1 ;;
  *"fetch origin main"*) exit 0 ;;
  *"merge-tree --write-tree HEAD origin/main"*) printf '%s\n' 'tree123'; exit 0 ;;
esac
exit 1
"#,
        );
        let mut config = test_config(false);
        config.auto.merge = true;
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let mut summary = test_summary();
        summary.change_request_identity = Some(crate::remote::test_change_request_identity());
        summary.check_status = "passed".to_string();
        summary.comment_count = 0;
        let details = PrDetails::default();
        let cache = PrCache::observed(summary, Some(details.clone()));
        save_pr_cache(&repo, "feature", &cache).unwrap();
        save_pr_details_cache(&repo, "feature", &details).unwrap();
        let worktree = temp.join("worktree");
        let run = super::super::AutoLaunch::new(&repo.root, &worktree, "feature", "Implement")
            .unwrap()
            .create_run()
            .run;

        let snapshot = build_auto_run_stabilization_snapshot(&repo, &run, &config);
        let blockers = super::super::stabilization_plan::derive_blockers(&snapshot);

        assert!(snapshot.goal.auto_merge);
        assert!(snapshot.worktree.dirty);
        assert!(
            snapshot
                .pull_request
                .as_ref()
                .and_then(|pull_request| pull_request.observation_error.as_deref())
                .is_some_and(|error| error.contains("git status inspection failed"))
        );
        assert_eq!(
            super::super::stabilization_plan::plan(&snapshot).blocker,
            StabilizationBlocker::DirtyWorktree
        );
        assert!(!blockers.contains(&StabilizationBlocker::ReadyToAutoMerge));

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
                provider: Some(crate::remote::ProviderKind::GitHub),
                canonical_host: Some("github.com".to_string()),
                project_path: Some("owner/repo".to_string()),
                target_branch: Some("main".to_string()),
                identity_complete: true,
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
            worktree_session_id: "test-feature".to_string(),
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
                provider: Some(crate::remote::ProviderKind::GitHub),
                canonical_host: Some("github.com".to_string()),
                project_path: Some("owner/repo".to_string()),
                target_branch: Some("main".to_string()),
                identity_complete: true,
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
            worktree_session_id: "test-feature".to_string(),
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
            change_request_identity: None,
            native_state_evidence: crate::remote::NativeStateEvidence::default(),
            title: "Title".to_string(),
            author: "author".to_string(),
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
            queue_state: "not_queued".to_string(),
            comment_count: 3,
            merged: false,
            draft: false,
        }
    }

    fn approved_review(author: &str, id: &str) -> PrReview {
        PrReview {
            id: id.to_string(),
            author: author.to_string(),
            state: "APPROVED".to_string(),
            body: String::new(),
            submitted_at: format!("2026-07-01T00:00:{id:0>2}Z"),
        }
    }

    fn approval_facts(
        provider: ProviderKind,
        required_approvals: u64,
        decision: &str,
        reviews: Vec<PrReview>,
    ) -> (ReviewFacts, PolicyFacts) {
        let mut summary = test_summary();
        summary.review_decision = decision.to_string();
        let cache = PrCache::observed(
            summary,
            Some(PrDetails {
                reviews,
                ..PrDetails::default()
            }),
        );
        let policy = RepoPolicyCache {
            provider: Some(provider),
            required_approvals,
            ..RepoPolicyCache::default()
        };
        let pull_request =
            pull_request_facts_from_cache(&cache, &test_config(false), None, None, Some(&policy))
                .unwrap();
        let policy_facts = policy_facts_from_cache(Some(&policy), Some(&pull_request));
        (pull_request.review, policy_facts)
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
