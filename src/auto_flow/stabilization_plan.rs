#![allow(dead_code)]

use crate::github::PrCheckState;

use super::stabilization_model::*;

pub(crate) fn derive_blockers(snapshot: &StabilizationSnapshot) -> Vec<StabilizationBlocker> {
    let mut blockers = Vec::new();

    if snapshot.worktree.is_default_branch || snapshot.worktree.detached {
        blockers.push(StabilizationBlocker::NotEligible);
        return blockers;
    }

    let Some(pull_request) = &snapshot.pull_request else {
        if snapshot.run.as_ref().is_some_and(|run| {
            matches!(
                run.status,
                super::AutoRunStatus::Queued
                    | super::AutoRunStatus::Running
                    | super::AutoRunStatus::Paused
            )
        }) {
            blockers.push(StabilizationBlocker::NeedsImplementation);
        } else {
            blockers.push(StabilizationBlocker::NeedsPullRequest);
        }
        return blockers;
    };

    if pull_request.state == PullRequestState::Merged {
        blockers.push(StabilizationBlocker::Merged);
        return blockers;
    }

    if pull_request.state != PullRequestState::Open {
        blockers.push(StabilizationBlocker::Escalate);
        return blockers;
    }

    if snapshot.pending_push.is_some() {
        blockers.push(StabilizationBlocker::PendingPush);
    }
    if snapshot.worktree.dirty {
        blockers.push(StabilizationBlocker::DirtyWorktree);
    }
    if pull_request.observation_error.is_some() {
        blockers.push(StabilizationBlocker::ObservationFailed);
    }
    if pull_request.draft {
        blockers.push(StabilizationBlocker::DraftPullRequest);
    }
    if snapshot
        .repository
        .default_base
        .as_deref()
        .is_some_and(|base| base != pull_request.base_ref)
    {
        blockers.push(StabilizationBlocker::WrongBase);
    }
    if heads_diverged(snapshot, pull_request) {
        blockers.push(StabilizationBlocker::HeadDiverged);
    }
    if matches!(pull_request.mergeability, MergeabilityFacts::Blocked { .. }) {
        blockers.push(StabilizationBlocker::MergeBlocked);
    } else if matches!(pull_request.mergeability, MergeabilityFacts::Unknown) {
        blockers.push(StabilizationBlocker::Escalate);
    }
    if !pull_request.review.actionable_reviews.is_empty()
        || !pull_request.review.unresolved_threads.is_empty()
    {
        blockers.push(StabilizationBlocker::ReviewFeedbackFound);
    }

    let required_ci_blocker = required_ci_blocker(&pull_request.ci.required);
    if let Some(blocker) = required_ci_blocker {
        blockers.push(blocker);
    } else if pull_request.ci.required.is_empty() {
        match pull_request.ci.aggregate {
            PrCheckState::Failed | PrCheckState::Mixed => {
                blockers.push(StabilizationBlocker::CiFailed)
            }
            PrCheckState::Pending => blockers.push(StabilizationBlocker::CiPending),
            PrCheckState::Success | PrCheckState::Unknown => {}
        }
    }

    if pull_request.review.approval_required
        && !pull_request
            .review
            .decision
            .eq_ignore_ascii_case("APPROVED")
    {
        blockers.push(StabilizationBlocker::ReviewApprovalMissing);
    }

    match &snapshot.policy {
        PolicyFacts::Blocked { .. } => blockers.push(StabilizationBlocker::PolicyBlocked),
        PolicyFacts::Unknown { .. } if snapshot.goal.auto_merge => {
            blockers.push(StabilizationBlocker::PolicyUnknown);
        }
        PolicyFacts::Unknown { .. } | PolicyFacts::Satisfied => {}
    }

    if blockers.is_empty() {
        if snapshot.goal.auto_merge {
            blockers.push(StabilizationBlocker::ReadyToAutoMerge);
        } else {
            blockers.push(StabilizationBlocker::ReadyForManualMerge);
        }
    }

    blockers.sort_by_key(blocker_priority);
    blockers
}

pub(crate) fn plan(snapshot: &StabilizationSnapshot) -> StabilizationWorkItem {
    let blockers = derive_blockers(snapshot);
    let blocker = blockers
        .first()
        .cloned()
        .unwrap_or(StabilizationBlocker::Escalate);
    work_item_for_blocker(snapshot, blocker)
}

pub(crate) fn manual_merge_authorization(
    snapshot: &StabilizationSnapshot,
) -> Result<StabilizationWorkItem, StabilizationState> {
    let work = plan(snapshot);
    if work.kind != StabilizationWorkKind::MarkReadyForManualMerge {
        return Err(work.state());
    }
    if matches!(snapshot.policy, PolicyFacts::Unknown { .. }) {
        return Err(work_item_for_blocker(snapshot, StabilizationBlocker::PolicyUnknown).state());
    }
    Ok(work)
}

fn work_item_for_blocker(
    snapshot: &StabilizationSnapshot,
    blocker: StabilizationBlocker,
) -> StabilizationWorkItem {
    StabilizationWorkItem {
        kind: work_kind_for_blocker(&blocker),
        reason: reason_for_blocker(snapshot, &blocker),
        guard: work_guard(snapshot),
        blocker,
    }
}

pub(crate) fn state(snapshot: &StabilizationSnapshot) -> StabilizationState {
    plan(snapshot).state()
}

pub(crate) fn conservative_cached_state(
    config: &crate::config::Config,
    session: &crate::session::Session,
) -> Option<StabilizationState> {
    session.pr.summary()?;
    let pull_request = super::stabilization_observe::pull_request_facts_from_cache(
        &session.pr,
        config,
        None,
        None,
        None,
    );
    let snapshot = StabilizationSnapshot {
        run: None,
        repository: RepositoryFacts {
            root: session.path.clone(),
            default_base: config.default_base.clone(),
            github_remote: None,
            policy_refreshed_unix_ms: None,
            policy_error: None,
        },
        worktree: WorktreeFacts {
            path: session.path.clone(),
            branch: session.branch.clone(),
            is_default_branch: session.is_default_branch(config),
            detached: session.is_detached(),
            dirty: cached_status_is_dirty(&session.status_label),
            // Rendering cannot observe immutable Git identities. Unknown heads are
            // intentionally conservative and therefore cannot claim readiness.
            local_head_sha: None,
            remote_head_sha: None,
        },
        pull_request,
        policy: if config.auto.merge {
            PolicyFacts::Unknown {
                reason: Some("repository policy is unavailable in the cached view".to_string()),
            }
        } else {
            PolicyFacts::Satisfied
        },
        goal: StabilizationGoal {
            auto_merge: config.auto.merge,
            cleanup_after_merge: config.auto.cleanup_after_merge,
        },
        pending_push: None,
    };
    Some(state(&snapshot))
}

fn cached_status_is_dirty(status_label: &str) -> bool {
    status_label
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|parts| parts[0] == "dirty" && parts[1].parse::<usize>().unwrap_or(0) > 0)
}

fn required_ci_blocker(required: &[CheckFact]) -> Option<StabilizationBlocker> {
    let required = required.iter().filter(|check| check.required);
    let mut saw_pending = false;
    for check in required {
        match check.state {
            PrCheckState::Unknown => return Some(StabilizationBlocker::CiMissingRequiredChecks),
            PrCheckState::Failed | PrCheckState::Mixed => {
                return Some(StabilizationBlocker::CiFailed);
            }
            PrCheckState::Pending => saw_pending = true,
            PrCheckState::Success => {}
        }
    }
    saw_pending.then_some(StabilizationBlocker::CiPending)
}

fn blocker_priority(blocker: &StabilizationBlocker) -> u8 {
    match blocker {
        StabilizationBlocker::NotEligible => 0,
        StabilizationBlocker::NeedsImplementation => 1,
        StabilizationBlocker::NeedsPullRequest => 2,
        StabilizationBlocker::PendingPush => 3,
        StabilizationBlocker::DirtyWorktree => 4,
        StabilizationBlocker::ObservationFailed => 5,
        StabilizationBlocker::DraftPullRequest => 6,
        StabilizationBlocker::WrongBase => 7,
        StabilizationBlocker::HeadDiverged => 8,
        StabilizationBlocker::MergeBlocked => 9,
        StabilizationBlocker::ReviewFeedbackFound => 10,
        StabilizationBlocker::CiFailed | StabilizationBlocker::CiMissingRequiredChecks => 11,
        StabilizationBlocker::CiPending => 12,
        StabilizationBlocker::ReviewApprovalMissing => 13,
        StabilizationBlocker::PolicyBlocked => 14,
        StabilizationBlocker::PolicyUnknown => 15,
        StabilizationBlocker::ReadyToAutoMerge | StabilizationBlocker::ReadyForManualMerge => 16,
        StabilizationBlocker::Merged => 17,
        StabilizationBlocker::Escalate => 18,
    }
}

fn work_kind_for_blocker(blocker: &StabilizationBlocker) -> StabilizationWorkKind {
    match blocker {
        StabilizationBlocker::NeedsImplementation => StabilizationWorkKind::RunImplementation,
        StabilizationBlocker::NeedsPullRequest => StabilizationWorkKind::PushInitialAndOpenPr,
        StabilizationBlocker::PendingPush => StabilizationWorkKind::PushPendingRepair,
        StabilizationBlocker::ReviewFeedbackFound => StabilizationWorkKind::FixReview,
        StabilizationBlocker::CiFailed | StabilizationBlocker::CiMissingRequiredChecks => {
            StabilizationWorkKind::FixCi
        }
        StabilizationBlocker::CiPending => StabilizationWorkKind::WaitForCi,
        StabilizationBlocker::ReviewApprovalMissing => StabilizationWorkKind::WaitForReview,
        StabilizationBlocker::ReadyForManualMerge => StabilizationWorkKind::MarkReadyForManualMerge,
        StabilizationBlocker::ReadyToAutoMerge => StabilizationWorkKind::Merge,
        StabilizationBlocker::Merged => StabilizationWorkKind::Done,
        StabilizationBlocker::NotEligible
        | StabilizationBlocker::DirtyWorktree
        | StabilizationBlocker::ObservationFailed
        | StabilizationBlocker::DraftPullRequest
        | StabilizationBlocker::WrongBase
        | StabilizationBlocker::HeadDiverged
        | StabilizationBlocker::MergeBlocked
        | StabilizationBlocker::PolicyBlocked
        | StabilizationBlocker::PolicyUnknown
        | StabilizationBlocker::Escalate => StabilizationWorkKind::Escalate,
    }
}

fn reason_for_blocker(snapshot: &StabilizationSnapshot, blocker: &StabilizationBlocker) -> String {
    match blocker {
        StabilizationBlocker::NotEligible => {
            if snapshot.worktree.detached {
                "detached worktrees are not eligible for PR Stabilization".to_string()
            } else {
                "default branch worktrees are not eligible for PR Stabilization".to_string()
            }
        }
        StabilizationBlocker::NeedsImplementation => {
            "implementation work must finish before PR Stabilization can continue".to_string()
        }
        StabilizationBlocker::NeedsPullRequest => {
            "no pull request is cached for this worktree branch".to_string()
        }
        StabilizationBlocker::PendingPush => {
            "a guarded repair commit is waiting for user inspection and push".to_string()
        }
        StabilizationBlocker::DirtyWorktree => {
            "local worktree changes must be resolved before interpreting PR gates".to_string()
        }
        StabilizationBlocker::ObservationFailed => snapshot
            .pull_request
            .as_ref()
            .and_then(|pr| pr.observation_error.clone())
            .unwrap_or_else(|| "pull request observation is not trustworthy".to_string()),
        StabilizationBlocker::DraftPullRequest => {
            "draft pull request must be marked ready before merge readiness".to_string()
        }
        StabilizationBlocker::WrongBase => {
            let pr_base = snapshot
                .pull_request
                .as_ref()
                .map(|pr| pr.base_ref.as_str())
                .unwrap_or("unknown");
            let expected = snapshot
                .repository
                .default_base
                .as_deref()
                .unwrap_or("unknown");
            format!("pull request base is {pr_base}, expected {expected}")
        }
        StabilizationBlocker::HeadDiverged => {
            "local, remote, and pull request heads do not agree".to_string()
        }
        StabilizationBlocker::MergeBlocked => snapshot
            .pull_request
            .as_ref()
            .and_then(|pr| match &pr.mergeability {
                MergeabilityFacts::Blocked { reason } => Some(reason.clone()),
                MergeabilityFacts::Unknown | MergeabilityFacts::Clean => None,
            })
            .unwrap_or_else(|| "pull request mergeability is blocked".to_string()),
        StabilizationBlocker::ReviewFeedbackFound => {
            "actionable review feedback is present".to_string()
        }
        StabilizationBlocker::ReviewApprovalMissing => {
            "review approval is required but not satisfied".to_string()
        }
        StabilizationBlocker::CiFailed => required_check_reason(
            snapshot,
            &[PrCheckState::Failed, PrCheckState::Mixed],
            "pull request checks are failing",
            "required checks are failing",
        ),
        StabilizationBlocker::CiPending => required_check_reason(
            snapshot,
            &[PrCheckState::Pending],
            "pull request checks are still running",
            "required checks are still running",
        ),
        StabilizationBlocker::CiMissingRequiredChecks => required_check_reason(
            snapshot,
            &[PrCheckState::Unknown],
            "one or more required checks are missing",
            "required checks are missing",
        ),
        StabilizationBlocker::PolicyBlocked => "repository policy blocks readiness".to_string(),
        StabilizationBlocker::PolicyUnknown => {
            "repository policy is unknown, so merge authorization is blocked".to_string()
        }
        StabilizationBlocker::ReadyForManualMerge => {
            "all known gates pass; auto-merge is disabled".to_string()
        }
        StabilizationBlocker::ReadyToAutoMerge => {
            "all required gates pass for auto-merge".to_string()
        }
        StabilizationBlocker::Merged => "pull request is already merged".to_string(),
        StabilizationBlocker::Escalate => {
            if snapshot
                .pull_request
                .as_ref()
                .is_some_and(|pr| matches!(pr.mergeability, MergeabilityFacts::Unknown))
            {
                "pull request mergeability is unknown".to_string()
            } else {
                "PR Stabilization cannot choose a safe automated action".to_string()
            }
        }
    }
}

fn heads_diverged(snapshot: &StabilizationSnapshot, pull_request: &PullRequestFacts) -> bool {
    let Some(local) = snapshot.worktree.local_head_sha.as_deref() else {
        return true;
    };
    let Some(remote) = snapshot.worktree.remote_head_sha.as_deref() else {
        return true;
    };
    local != remote || remote != pull_request.head_sha
}

fn required_check_reason(
    snapshot: &StabilizationSnapshot,
    states: &[PrCheckState],
    aggregate_reason: &str,
    required_reason: &str,
) -> String {
    let names = snapshot
        .pull_request
        .as_ref()
        .map(|pr| {
            pr.ci
                .required
                .iter()
                .filter(|check| check.required && states.contains(&check.state))
                .map(|check| check.name.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if names.is_empty() {
        aggregate_reason.to_string()
    } else {
        format!("{required_reason}: {}", names.join(", "))
    }
}

fn work_guard(snapshot: &StabilizationSnapshot) -> WorkGuard {
    WorkGuard {
        local_head_sha: snapshot.worktree.local_head_sha.clone(),
        remote_head_sha: snapshot.worktree.remote_head_sha.clone(),
        pr_head_sha: snapshot
            .pull_request
            .as_ref()
            .map(|pull_request| pull_request.head_sha.clone()),
        base_sha: snapshot
            .pull_request
            .as_ref()
            .and_then(|pull_request| pull_request.base_sha.clone()),
        review_thread_ids: snapshot
            .pull_request
            .as_ref()
            .map(|pull_request| {
                crate::review::canonical_review_thread_ids(
                    pull_request
                        .review
                        .unresolved_threads
                        .iter()
                        .map(|thread| thread.thread_id.as_str()),
                )
            })
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use crate::agent::AgentState;
    use crate::github::{CiFailure, PrCache, PrCheckState, PrDetails, PrSummary};
    use crate::session::{Session, SessionClassification};

    use super::*;

    #[test]
    fn no_pr_plans_initial_pr_push() {
        let snapshot = snapshot(None);

        let work = plan(&snapshot);

        assert_eq!(work.blocker, StabilizationBlocker::NeedsPullRequest);
        assert_eq!(work.kind, StabilizationWorkKind::PushInitialAndOpenPr);
    }

    #[test]
    fn active_run_without_pr_needs_implementation() {
        let mut snapshot = snapshot(None);
        snapshot.run = Some(AutoRunRef {
            id: "run".to_string(),
            status: super::super::AutoRunStatus::Running,
            pr_number: None,
            pr_url: None,
            current_head_sha: None,
        });

        let work = plan(&snapshot);

        assert_eq!(work.blocker, StabilizationBlocker::NeedsImplementation);
        assert_eq!(work.kind, StabilizationWorkKind::RunImplementation);
    }

    #[test]
    fn pending_push_wins_over_dirty_and_remote_blockers() {
        let mut pr = clean_pr();
        pr.ci.aggregate = PrCheckState::Failed;
        pr.review
            .actionable_reviews
            .push(ActionableReviewItem::ReviewBody {
                review_id: "r1".to_string(),
                author: "reviewer".to_string(),
                state: "CHANGES_REQUESTED".to_string(),
                body: "fix".to_string(),
                submitted_at: "now".to_string(),
            });
        let mut snapshot = snapshot(Some(pr));
        snapshot.worktree.dirty = true;
        snapshot.pending_push = Some(PendingPushGuard {
            repair_kind: RepairKind::Review,
            commit_sha: "repair".to_string(),
            expected_local_head_sha: "repair".to_string(),
            expected_remote_head_sha: Some("remote".to_string()),
            pr_number: Some(1),
            expected_pr_head_sha: Some("remote".to_string()),
            expected_base_sha: Some("base".to_string()),
            guarded_review_thread_ids: vec!["thread-1".to_string()],
        });

        let blockers = derive_blockers(&snapshot);
        let work = plan(&snapshot);

        assert_eq!(blockers[0], StabilizationBlocker::PendingPush);
        assert_eq!(work.kind, StabilizationWorkKind::PushPendingRepair);
    }

    #[test]
    fn observation_failure_precedes_wrong_base_and_remote_gate_failures() {
        let mut pr = clean_pr();
        pr.observation_error = Some("details refresh failed".to_string());
        pr.base_ref = "release".to_string();
        pr.ci.aggregate = PrCheckState::Failed;
        pr.review.unresolved_threads.push(review_thread("thread-1"));
        let snapshot = snapshot(Some(pr));

        let state = state(&snapshot);

        assert_eq!(state.blocker, StabilizationBlocker::ObservationFailed);
        assert_eq!(state.status, StabilizationStatus::Escalated);
        assert_eq!(state.next_work, StabilizationWorkKind::Escalate);
    }

    #[test]
    fn wrong_base_precedes_head_review_and_ci_failures() {
        let mut pr = clean_pr();
        pr.base_ref = "release".to_string();
        pr.ci.aggregate = PrCheckState::Failed;
        pr.review.unresolved_threads.push(review_thread("thread-1"));
        let mut snapshot = snapshot(Some(pr));
        snapshot.worktree.local_head_sha = Some("local".to_string());

        let state = state(&snapshot);

        assert_eq!(state.blocker, StabilizationBlocker::WrongBase);
        assert_eq!(state.status, StabilizationStatus::Escalated);
        assert_eq!(state.next_work, StabilizationWorkKind::Escalate);
    }

    #[test]
    fn interface_derives_waiting_status_with_blocker_and_next_work() {
        let mut pr = clean_pr();
        pr.ci.aggregate = PrCheckState::Pending;

        let state = state(&snapshot(Some(pr)));

        assert_eq!(state.status, StabilizationStatus::Waiting);
        assert_eq!(state.blocker, StabilizationBlocker::CiPending);
        assert_eq!(state.next_work, StabilizationWorkKind::WaitForCi);
        assert!(!state.reason.is_empty());
    }

    #[test]
    fn dirty_worktree_wins_over_merge_review_and_ci() {
        let mut pr = clean_pr();
        pr.mergeability = MergeabilityFacts::Blocked {
            reason: "DIRTY".to_string(),
        };
        pr.ci.aggregate = PrCheckState::Failed;
        let mut snapshot = snapshot(Some(pr));
        snapshot.worktree.dirty = true;

        let work = plan(&snapshot);

        assert_eq!(work.blocker, StabilizationBlocker::DirtyWorktree);
        assert_eq!(work.kind, StabilizationWorkKind::Escalate);
    }

    #[test]
    fn merge_blocked_wins_over_review_feedback() {
        let mut pr = clean_pr();
        pr.mergeability = MergeabilityFacts::Blocked {
            reason: "conflict".to_string(),
        };
        pr.review.unresolved_threads.push(review_thread("thread-1"));
        let snapshot = snapshot(Some(pr));

        let work = plan(&snapshot);

        assert_eq!(work.blocker, StabilizationBlocker::MergeBlocked);
        assert_eq!(work.kind, StabilizationWorkKind::Escalate);
        assert!(work.reason.contains("conflict"));
    }

    #[test]
    fn review_feedback_plans_review_fix_and_guards_threads() {
        let mut pr = clean_pr();
        pr.review.unresolved_threads.push(review_thread("thread-1"));
        let snapshot = snapshot(Some(pr));

        let work = plan(&snapshot);

        assert_eq!(work.blocker, StabilizationBlocker::ReviewFeedbackFound);
        assert_eq!(work.kind, StabilizationWorkKind::FixReview);
        assert_eq!(work.guard.review_thread_ids, vec!["thread-1".to_string()]);
    }

    #[test]
    fn top_level_comment_only_does_not_block_readiness() {
        let mut pr = clean_pr();
        pr.review.top_level_comments = 1;
        pr.top_level_comment_count = 1;
        let snapshot = snapshot(Some(pr));

        let work = plan(&snapshot);

        assert_eq!(work.blocker, StabilizationBlocker::ReadyForManualMerge);
    }

    #[test]
    fn ci_failed_and_missing_required_checks_plan_ci_fix() {
        let mut failed = clean_pr();
        failed.ci.aggregate = PrCheckState::Failed;
        let failed_work = plan(&snapshot(Some(failed)));

        let mut missing = clean_pr();
        missing.ci.required.push(CheckFact {
            name: "lint".to_string(),
            state: PrCheckState::Unknown,
            required: true,
            head_sha: None,
        });
        let missing_work = plan(&snapshot(Some(missing)));

        assert_eq!(failed_work.blocker, StabilizationBlocker::CiFailed);
        assert_eq!(failed_work.kind, StabilizationWorkKind::FixCi);
        assert_eq!(
            missing_work.blocker,
            StabilizationBlocker::CiMissingRequiredChecks
        );
        assert_eq!(missing_work.kind, StabilizationWorkKind::FixCi);
    }

    #[test]
    fn required_check_names_are_reported_for_failed_pending_and_missing_checks() {
        let mut failed = clean_pr();
        failed.ci.aggregate = PrCheckState::Unknown;
        failed.ci.required.push(CheckFact {
            name: "lint".to_string(),
            state: PrCheckState::Failed,
            required: true,
            head_sha: Some("head".to_string()),
        });
        let failed_work = plan(&snapshot(Some(failed)));

        let mut pending = clean_pr();
        pending.ci.aggregate = PrCheckState::Success;
        pending.ci.required.push(CheckFact {
            name: "test".to_string(),
            state: PrCheckState::Pending,
            required: true,
            head_sha: Some("head".to_string()),
        });
        let pending_work = plan(&snapshot(Some(pending)));

        let mut missing = clean_pr();
        missing.ci.aggregate = PrCheckState::Success;
        missing.ci.required.push(CheckFact {
            name: "build".to_string(),
            state: PrCheckState::Unknown,
            required: true,
            head_sha: None,
        });
        let missing_work = plan(&snapshot(Some(missing)));

        assert_eq!(failed_work.blocker, StabilizationBlocker::CiFailed);
        assert!(failed_work.reason.contains("lint"));
        assert_eq!(pending_work.blocker, StabilizationBlocker::CiPending);
        assert!(pending_work.reason.contains("test"));
        assert_eq!(
            missing_work.blocker,
            StabilizationBlocker::CiMissingRequiredChecks
        );
        assert!(missing_work.reason.contains("build"));
    }

    #[test]
    fn required_check_pending_wins_over_aggregate_success() {
        let mut pr = clean_pr();
        pr.ci.aggregate = PrCheckState::Success;
        pr.ci.required.push(CheckFact {
            name: "test".to_string(),
            state: PrCheckState::Pending,
            required: true,
            head_sha: Some("head".to_string()),
        });

        let work = plan(&snapshot(Some(pr)));

        assert_eq!(work.blocker, StabilizationBlocker::CiPending);
        assert_eq!(work.kind, StabilizationWorkKind::WaitForCi);
    }

    #[test]
    fn required_checks_passing_make_aggregate_failure_optional_warning() {
        let mut pr = clean_pr();
        pr.ci.aggregate = PrCheckState::Failed;
        pr.ci.required.push(CheckFact {
            name: "ci".to_string(),
            state: PrCheckState::Success,
            required: true,
            head_sha: Some("head".to_string()),
        });
        pr.ci.optional_failures.push("docs".to_string());

        let work = plan(&snapshot(Some(pr)));

        assert_eq!(work.blocker, StabilizationBlocker::ReadyForManualMerge);
    }

    #[test]
    fn aggregate_status_is_fallback_when_required_checks_are_unavailable() {
        let mut pr = clean_pr();
        pr.ci.aggregate = PrCheckState::Mixed;

        let work = plan(&snapshot(Some(pr)));

        assert_eq!(work.blocker, StabilizationBlocker::CiFailed);
        assert_eq!(work.kind, StabilizationWorkKind::FixCi);
        assert_eq!(work.reason, "pull request checks are failing");
    }

    #[test]
    fn all_green_requires_clean_mergeability() {
        let mut pr = clean_pr();
        pr.mergeability = MergeabilityFacts::Unknown;

        let work = plan(&snapshot(Some(pr)));

        assert_eq!(work.blocker, StabilizationBlocker::Escalate);
        assert_eq!(work.kind, StabilizationWorkKind::Escalate);
        assert!(work.reason.contains("mergeability is unknown"));
    }

    #[test]
    fn review_approval_missing_plans_wait_after_ci() {
        let mut pr = clean_pr();
        pr.review.approval_required = true;
        pr.review.decision = "REVIEW_REQUIRED".to_string();

        let work = plan(&snapshot(Some(pr)));

        assert_eq!(work.blocker, StabilizationBlocker::ReviewApprovalMissing);
        assert_eq!(work.kind, StabilizationWorkKind::WaitForReview);
    }

    #[test]
    fn policy_unknown_blocks_auto_merge_only() {
        let mut manual = snapshot(Some(clean_pr()));
        manual.policy = PolicyFacts::Unknown {
            reason: Some("not fetched".to_string()),
        };
        manual.goal.auto_merge = false;
        let mut auto = manual.clone();
        auto.goal.auto_merge = true;

        assert_eq!(
            plan(&manual).blocker,
            StabilizationBlocker::ReadyForManualMerge
        );
        assert_eq!(plan(&auto).blocker, StabilizationBlocker::PolicyUnknown);
    }

    #[test]
    fn manual_merge_authorization_requires_observed_policy() {
        let mut snapshot = snapshot(Some(clean_pr()));
        snapshot.policy = PolicyFacts::Unknown {
            reason: Some("not fetched".to_string()),
        };

        let denied = manual_merge_authorization(&snapshot).unwrap_err();

        assert_eq!(denied.blocker, StabilizationBlocker::PolicyUnknown);
        assert!(!snapshot.goal.auto_merge);
    }

    #[test]
    fn conservative_cached_projection_reports_observed_dirty_status() {
        let config = cached_test_config();
        let mut session = cached_test_session();
        session.status_label = "dirty 2 ahead 1".to_string();

        let state = conservative_cached_state(&config, &session).unwrap();

        assert_eq!(state.blocker, StabilizationBlocker::DirtyWorktree);
        assert_ne!(state.blocker, StabilizationBlocker::ReadyForManualMerge);
    }

    #[test]
    fn conservative_cached_projection_treats_divergent_or_unknown_heads_as_blocked() {
        let config = cached_test_config();
        for status in ["ahead 1", "clean"] {
            let mut session = cached_test_session();
            session.status_label = status.to_string();

            let state = conservative_cached_state(&config, &session).unwrap();

            assert_eq!(state.blocker, StabilizationBlocker::HeadDiverged);
            assert_ne!(state.blocker, StabilizationBlocker::ReadyForManualMerge);
        }
    }

    #[test]
    fn conservative_cached_projection_preserves_stale_observation_failure() {
        let config = cached_test_config();
        let mut session = cached_test_session();
        session.pr.mark_preserved_stale();

        let state = conservative_cached_state(&config, &session).unwrap();

        assert_eq!(state.blocker, StabilizationBlocker::ObservationFailed);
        assert_ne!(state.blocker, StabilizationBlocker::ReadyForManualMerge);
    }

    #[test]
    fn policy_blocked_blocks_readiness() {
        let mut snapshot = snapshot(Some(clean_pr()));
        snapshot.policy = PolicyFacts::Blocked {
            blockers: vec![PolicyBlocker::BranchOutOfDate],
        };

        let work = plan(&snapshot);

        assert_eq!(work.blocker, StabilizationBlocker::PolicyBlocked);
        assert_eq!(work.kind, StabilizationWorkKind::Escalate);
    }

    #[test]
    fn all_green_returns_manual_or_auto_merge_goal() {
        let manual = snapshot(Some(clean_pr()));
        let mut auto = manual.clone();
        auto.goal.auto_merge = true;

        assert_eq!(
            plan(&manual).kind,
            StabilizationWorkKind::MarkReadyForManualMerge
        );
        assert_eq!(plan(&auto).kind, StabilizationWorkKind::Merge);
    }

    #[test]
    fn merged_pr_is_done() {
        let mut pr = clean_pr();
        pr.state = PullRequestState::Merged;

        let work = plan(&snapshot(Some(pr)));

        assert_eq!(work.blocker, StabilizationBlocker::Merged);
        assert_eq!(work.kind, StabilizationWorkKind::Done);
    }

    #[test]
    fn planner_always_returns_one_work_item_for_representative_snapshots() {
        let cases = [snapshot(None), snapshot(Some(clean_pr())), {
            let mut item = snapshot(Some(clean_pr()));
            item.worktree.is_default_branch = true;
            item
        }];

        for case in cases {
            let work = plan(&case);
            assert!(!work.reason.trim().is_empty());
        }
    }

    #[test]
    fn merge_is_never_returned_unless_required_gates_pass() {
        let mut dirty = snapshot(Some(clean_pr()));
        dirty.goal.auto_merge = true;
        dirty.worktree.dirty = true;
        let mut failed = snapshot(Some(clean_pr()));
        failed.goal.auto_merge = true;
        failed.pull_request.as_mut().unwrap().ci.aggregate = PrCheckState::Failed;
        let mut approved = snapshot(Some(clean_pr()));
        approved.goal.auto_merge = true;

        assert_ne!(plan(&dirty).kind, StabilizationWorkKind::Merge);
        assert_ne!(plan(&failed).kind, StabilizationWorkKind::Merge);
        assert_eq!(plan(&approved).kind, StabilizationWorkKind::Merge);
    }

    #[test]
    fn phase_1_draft_pr_cannot_become_manual_or_auto_merge_ready() {
        for auto_merge in [false, true] {
            let mut pr = clean_pr();
            pr.draft = true;
            let mut snapshot = snapshot(Some(pr));
            snapshot.goal.auto_merge = auto_merge;
            snapshot.worktree.local_head_sha = Some("head".to_string());
            snapshot.worktree.remote_head_sha = Some("head".to_string());

            let work = plan(&snapshot);

            assert!(!matches!(
                work.blocker,
                StabilizationBlocker::ReadyForManualMerge | StabilizationBlocker::ReadyToAutoMerge
            ));
            assert!(!matches!(
                work.kind,
                StabilizationWorkKind::MarkReadyForManualMerge | StabilizationWorkKind::Merge
            ));
            assert!(work.reason.to_ascii_lowercase().contains("draft"));
        }
    }

    #[test]
    fn phase_1_three_way_local_remote_and_pr_head_divergence_blocks_readiness() {
        for auto_merge in [false, true] {
            let mut pr = clean_pr();
            pr.head_sha = "pr-head".to_string();
            let mut snapshot = snapshot(Some(pr));
            snapshot.goal.auto_merge = auto_merge;
            snapshot.worktree.local_head_sha = Some("local-head".to_string());
            snapshot.worktree.remote_head_sha = Some("remote-head".to_string());

            let work = plan(&snapshot);

            assert!(!matches!(
                work.blocker,
                StabilizationBlocker::ReadyForManualMerge | StabilizationBlocker::ReadyToAutoMerge
            ));
            assert!(!matches!(
                work.kind,
                StabilizationWorkKind::MarkReadyForManualMerge | StabilizationWorkKind::Merge
            ));
            assert!(work.reason.to_ascii_lowercase().contains("head"));
        }
    }

    fn snapshot(pull_request: Option<PullRequestFacts>) -> StabilizationSnapshot {
        StabilizationSnapshot {
            run: None,
            repository: RepositoryFacts {
                root: PathBuf::from("/repo"),
                default_base: Some("main".to_string()),
                github_remote: Some("owner/repo".to_string()),
                policy_refreshed_unix_ms: Some(1),
                policy_error: None,
            },
            worktree: WorktreeFacts {
                path: PathBuf::from("/repo/feature"),
                branch: "feature".to_string(),
                is_default_branch: false,
                detached: false,
                dirty: false,
                local_head_sha: Some("head".to_string()),
                remote_head_sha: Some("head".to_string()),
            },
            pull_request,
            policy: PolicyFacts::Satisfied,
            goal: StabilizationGoal {
                auto_merge: false,
                cleanup_after_merge: false,
            },
            pending_push: None,
        }
    }

    fn clean_pr() -> PullRequestFacts {
        PullRequestFacts {
            number: 1,
            url: "https://example.test/pr/1".to_string(),
            state: PullRequestState::Open,
            draft: false,
            head_sha: "head".to_string(),
            base_ref: "main".to_string(),
            base_sha: Some("base".to_string()),
            updated_at: "now".to_string(),
            ci: CiFacts {
                aggregate: PrCheckState::Success,
                required: Vec::new(),
                optional_failures: Vec::new(),
                failures: Vec::<CiFailure>::new(),
            },
            review: ReviewFacts {
                decision: "APPROVED".to_string(),
                approval_required: false,
                actionable_reviews: Vec::new(),
                unresolved_threads: Vec::new(),
                top_level_comments: 0,
            },
            mergeability: MergeabilityFacts::Clean,
            top_level_comment_count: 0,
            observation_error: None,
        }
    }

    fn review_thread(thread_id: &str) -> ReviewThreadFact {
        ReviewThreadFact {
            thread_id: thread_id.to_string(),
            comment_id: "comment-1".to_string(),
            path: "src/lib.rs".to_string(),
            line: Some(1),
            body: "fix".to_string(),
            author: "reviewer".to_string(),
            resolved: false,
            created_at: "now".to_string(),
        }
    }

    fn cached_test_session() -> Session {
        Session {
            repo_index: 0,
            repo_label: "repo".to_string(),
            repo_key: None,
            path: PathBuf::from("/repo/feature"),
            incarnation: String::new(),
            path_display: "/repo/feature".to_string(),
            branch: "feature".to_string(),
            prompt_summary: String::new(),
            classification: SessionClassification::Work,
            visibility: 0,
            adopted: false,
            hidden: false,
            status_label: "clean".to_string(),
            agent_state: AgentState::Idle,
            opencode_status: None,
            pr: PrCache::observed(
                PrSummary {
                    number: 42,
                    title: "Ready".to_string(),
                    body: String::new(),
                    url: "https://example.test/pr/42".to_string(),
                    state: "OPEN".to_string(),
                    review_decision: "APPROVED".to_string(),
                    requested_reviewers: Vec::new(),
                    head_ref: "feature".to_string(),
                    base_ref: "main".to_string(),
                    head_sha: "head".to_string(),
                    updated_at: "now".to_string(),
                    check_status: "passed".to_string(),
                    merge_state_status: "CLEAN".to_string(),
                    comment_count: 0,
                    merged: false,
                    draft: false,
                },
                Some(PrDetails::default()),
            ),
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        }
    }

    fn cached_test_config() -> crate::config::Config {
        crate::config::Config {
            default_agent: "ask".to_string(),
            default_base: Some("main".to_string()),
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            opencode_port_base: 41_000,
            opencode_port_span: 1_000,
            opencode_shutdown_owned_servers: false,
            opencode_plan_plugin: false,
            escape_key: crate::config::EscapeKey::EscEsc,
            merge_method: crate::config::MergeMethod::Squash,
            icon_style: crate::config::IconStyle::Unicode,
            icon_style_configured: false,
            auto: crate::config::AutoConfig::default(),
            layout: crate::config::LayoutConfig::default(),
            checks: crate::config::Checks::default(),
            worktree_columns: Vec::new(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            prompt_templates: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/user.toml"),
            repo_config_path: PathBuf::from("/tmp/repo.toml"),
        }
    }
}
