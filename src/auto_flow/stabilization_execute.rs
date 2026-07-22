#![allow(dead_code)]

use crate::config::Config;
use crate::repo::Repository;

use super::stabilization_model::{PendingPushGuard, StabilizationWorkItem, WorkGuard};
use super::{
    AutoEvent, AutoStepKey, AutoStepStatus, PersistedAutoRun, append_auto_event,
    save_run_with_conn, stabilization_observe, stabilization_plan, unix_ms,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GuardedPushDecision {
    AlreadySatisfied,
    ValidToPush,
    Invalidated { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GuardedPushProgress {
    AlreadySatisfied,
    Pushed,
    Invalidated { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WorkGuardDecision {
    Valid,
    Invalidated { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepairContinuation {
    pub step_key: AutoStepKey,
    pub reason: &'static str,
    pub guard: Option<WorkGuard>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StandaloneRepair {
    kind: super::stabilization_model::RepairKind,
    prompt: String,
    guard: WorkGuard,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WaitDecision {
    KeepWaiting,
    QueueRepair,
    Continue,
    Escalate(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WaitProgress {
    KeepWaiting,
    Completed,
    RepairQueued,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MergeAuthorization {
    Authorized(AuthorizedMerge),
    Blocked(super::stabilization_model::StabilizationState),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AuthorizedMerge {
    pr_number: u64,
    guard: WorkGuard,
}

impl AuthorizedMerge {
    pub(crate) fn pr_number(&self) -> u64 {
        self.pr_number
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ManualMergeExecution {
    Merged { pr_number: u64 },
    Blocked(super::stabilization_model::StabilizationState),
}

pub(crate) fn observe_manual_merge_authorization(
    repo: &Repository,
    config: &Config,
    session: &mut crate::session::Session,
) -> MergeAuthorization {
    let pr_refresh = crate::github::refresh_pr_cache(
        repo,
        &session.branch,
        &mut session.pr,
        &session.path,
        config,
        true,
    );
    let policy_refresh = crate::github::refresh_repo_policy_cache(repo, &session.path, config);
    let remote_refresh = crate::git::fetch_origin(&session.path, config);
    let worktree_refresh = crate::git::selected_dirty(&session.path, config);
    let snapshot = stabilization_observe::build_stabilization_snapshot(repo, session, None, config);
    let authorization = stabilization_plan::manual_merge_authorization(&snapshot);
    let actual_base_observed = policy_refresh
        .as_ref()
        .is_ok_and(|policy| policy.default_branch.is_some());

    if let Err(error) = pr_refresh
        .or(policy_refresh.map(|_| ()))
        .or(remote_refresh)
        .or(worktree_refresh.map(|_| ()))
    {
        let mut state = authorization
            .err()
            .unwrap_or_else(|| stabilization_plan::state(&snapshot));
        state.status = super::stabilization_model::StabilizationStatus::Escalated;
        state.blocker = super::stabilization_model::StabilizationBlocker::ObservationFailed;
        state.next_work = super::stabilization_model::StabilizationWorkKind::Escalate;
        state.reason = error;
        return MergeAuthorization::Blocked(state);
    }
    if !actual_base_observed {
        let mut state = authorization
            .err()
            .unwrap_or_else(|| stabilization_plan::state(&snapshot));
        state.status = super::stabilization_model::StabilizationStatus::Escalated;
        state.blocker = super::stabilization_model::StabilizationBlocker::ObservationFailed;
        state.next_work = super::stabilization_model::StabilizationWorkKind::Escalate;
        state.reason = "repository default branch was not observed".to_string();
        return MergeAuthorization::Blocked(state);
    }

    match authorization {
        Ok(work) => MergeAuthorization::Authorized(AuthorizedMerge {
            pr_number: snapshot
                .pull_request
                .as_ref()
                .expect("authorized pull request")
                .number,
            guard: work.guard,
        }),
        Err(state) => MergeAuthorization::Blocked(state),
    }
}

pub(crate) fn execute_merge_authorization(
    config: &Config,
    path: &std::path::Path,
    authorization: MergeAuthorization,
) -> Result<ManualMergeExecution, String> {
    match authorization {
        MergeAuthorization::Authorized(AuthorizedMerge { pr_number, guard }) => {
            let expected_head_sha = guard.pr_head_sha.as_deref().ok_or_else(|| {
                "authorized merge is missing the observed pull request head".to_string()
            })?;
            crate::github::merge_pull_request(config, path, pr_number, expected_head_sha)?;
            Ok(ManualMergeExecution::Merged { pr_number })
        }
        MergeAuthorization::Blocked(state) => Ok(ManualMergeExecution::Blocked(state)),
    }
}

pub(crate) fn reobserve_and_execute_manual_merge(
    repo: &Repository,
    config: &Config,
    session: &mut crate::session::Session,
    initial_authorization: MergeAuthorization,
) -> Result<ManualMergeExecution, String> {
    let initial = match initial_authorization {
        MergeAuthorization::Authorized(initial) => initial,
        MergeAuthorization::Blocked(state) => return Ok(ManualMergeExecution::Blocked(state)),
    };
    let path = session.path.clone();
    let fresh_authorization = observe_manual_merge_authorization(repo, config, session);
    let fresh = match fresh_authorization {
        MergeAuthorization::Authorized(fresh) => fresh,
        MergeAuthorization::Blocked(state) => return Ok(ManualMergeExecution::Blocked(state)),
    };
    execute_merge_authorization(config, &path, reauthorize_observed_merge(initial, fresh))
}

fn reauthorize_observed_merge(
    initial: AuthorizedMerge,
    fresh: AuthorizedMerge,
) -> MergeAuthorization {
    if initial.pr_number != fresh.pr_number {
        return MergeAuthorization::Blocked(changed_merge_state(
            "pull request identity changed during pre-push checks".to_string(),
        ));
    }
    if let WorkGuardDecision::Invalidated { reason } = decide_work_guard(
        &super::stabilization_model::RepairKind::Merge,
        &initial.guard,
        &fresh.guard,
    ) {
        return MergeAuthorization::Blocked(changed_merge_state(reason));
    }
    MergeAuthorization::Authorized(fresh)
}

fn changed_merge_state(reason: String) -> super::stabilization_model::StabilizationState {
    super::stabilization_model::StabilizationState {
        status: super::stabilization_model::StabilizationStatus::Escalated,
        blocker: super::stabilization_model::StabilizationBlocker::ObservationFailed,
        next_work: super::stabilization_model::StabilizationWorkKind::Escalate,
        reason,
    }
}

pub(crate) fn authorize_auto_merge(
    snapshot: &super::stabilization_model::StabilizationSnapshot,
    expected_pr_number: Option<u64>,
    expected_guard: &WorkGuard,
) -> MergeAuthorization {
    let work = stabilization_plan::plan(snapshot);
    let Some(pull_request) = snapshot.pull_request.as_ref() else {
        return MergeAuthorization::Blocked(work.state());
    };
    if work.kind != super::stabilization_model::StabilizationWorkKind::Merge {
        return MergeAuthorization::Blocked(work.state());
    }
    if expected_pr_number != Some(pull_request.number) {
        let mut state = work.state();
        state.status = super::stabilization_model::StabilizationStatus::Escalated;
        state.blocker = super::stabilization_model::StabilizationBlocker::ObservationFailed;
        state.next_work = super::stabilization_model::StabilizationWorkKind::Escalate;
        state.reason = "pull request identity changed before merge".to_string();
        return MergeAuthorization::Blocked(state);
    }
    if let WorkGuardDecision::Invalidated { reason } = decide_work_guard(
        &super::stabilization_model::RepairKind::Merge,
        expected_guard,
        &work.guard,
    ) {
        let mut state = work.state();
        state.status = super::stabilization_model::StabilizationStatus::Escalated;
        state.blocker = super::stabilization_model::StabilizationBlocker::ObservationFailed;
        state.next_work = super::stabilization_model::StabilizationWorkKind::Escalate;
        state.reason = reason;
        return MergeAuthorization::Blocked(state);
    }
    MergeAuthorization::Authorized(AuthorizedMerge {
        pr_number: pull_request.number,
        guard: work.guard,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepairCommitObservation {
    pub guard: WorkGuard,
    pub pr_number: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RepairCommitGate {
    Ready,
    Invalidated { summary: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepairCommitOutcome {
    pub summary: String,
}

pub(crate) fn next_repair_continuation(persisted: &PersistedAutoRun) -> Option<RepairContinuation> {
    let latest = |key: &AutoStepKey| {
        persisted
            .steps
            .iter()
            .rev()
            .find(|step| step.step_key.as_str() == key.as_str())
    };
    let unfinished = |key: &AutoStepKey| {
        !matches!(
            latest(key).map(|step| step.status),
            Some(AutoStepStatus::Queued | AutoStepStatus::Starting | AutoStepStatus::Running)
                | Some(AutoStepStatus::Waiting | AutoStepStatus::Done | AutoStepStatus::Skipped)
        )
    };
    let continuation = |step_key, reason, source: &AutoStepKey| RepairContinuation {
        step_key,
        reason,
        guard: latest(source).and_then(|step| step.work_guard.clone()),
    };

    if latest(&AutoStepKey::FixReview).is_some_and(|step| step.status == AutoStepStatus::Done)
        && unfinished(&AutoStepKey::VerifyReviewFix)
    {
        return Some(continuation(
            AutoStepKey::VerifyReviewFix,
            "run review-fix verification before committing",
            &AutoStepKey::FixReview,
        ));
    }
    if latest(&AutoStepKey::VerifyReviewFix).is_some_and(|step| step.status == AutoStepStatus::Done)
        && unfinished(&AutoStepKey::CommitReviewFix)
    {
        return Some(continuation(
            AutoStepKey::CommitReviewFix,
            "commit verified review fixes behind a guarded push",
            &AutoStepKey::VerifyReviewFix,
        ));
    }
    if latest(&AutoStepKey::FixCi).is_some_and(|step| step.status == AutoStepStatus::Done)
        && unfinished(&AutoStepKey::VerifyCiFix)
    {
        return Some(continuation(
            AutoStepKey::VerifyCiFix,
            "run CI-fix verification before committing",
            &AutoStepKey::FixCi,
        ));
    }
    if latest(&AutoStepKey::VerifyCiFix).is_some_and(|step| step.status == AutoStepStatus::Done)
        && unfinished(&AutoStepKey::CommitCiFix)
    {
        return Some(continuation(
            AutoStepKey::CommitCiFix,
            "commit verified CI fixes behind a guarded push",
            &AutoStepKey::VerifyCiFix,
        ));
    }

    let local_fix =
        latest(&AutoStepKey::FixLocalVerify).filter(|step| step.status == AutoStepStatus::Done)?;
    let failed_verify = persisted.steps.iter().rev().find(|step| {
        step.sequence < local_fix.sequence
            && matches!(
                step.step_key,
                AutoStepKey::VerifyReviewFix | AutoStepKey::VerifyCiFix
            )
            && step.status == AutoStepStatus::Failed
    })?;
    let retry_already_exists = persisted.steps.iter().any(|step| {
        step.sequence > local_fix.sequence
            && step.step_key.as_str() == failed_verify.step_key.as_str()
    });
    (!retry_already_exists).then(|| RepairContinuation {
        step_key: failed_verify.step_key.clone(),
        reason: match failed_verify.step_key {
            AutoStepKey::VerifyReviewFix => "retry review-fix verification after local repair",
            AutoStepKey::VerifyCiFix => "retry CI-fix verification after local repair",
            _ => unreachable!(),
        },
        guard: failed_verify.work_guard.clone(),
    })
}

pub(crate) fn observe_and_plan(
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
) -> StabilizationWorkItem {
    let snapshot =
        stabilization_observe::build_auto_run_stabilization_snapshot(repo, &persisted.run, config);
    let work = stabilization_plan::plan(&snapshot);
    apply_state(persisted, &work.state());
    persisted.run.status = persisted.authoritative_status();
    persisted.run.updated_unix_ms = unix_ms();
    work
}

fn apply_state(
    persisted: &mut PersistedAutoRun,
    state: &super::stabilization_model::StabilizationState,
) {
    persisted.run.stabilization_status = Some(state.status);
    persisted.run.stabilization_blocker = Some(state.blocker.clone());
    persisted.run.stabilization_next_work = Some(state.next_work.clone());
}

pub(crate) fn prepare_standalone_repair(
    session: &crate::session::Session,
    config: &Config,
    kind: super::stabilization_model::RepairKind,
) -> Result<StandaloneRepair, String> {
    let summary = session.pr.trusted_summary()?;
    let local_head_sha = Some(crate::git::current_head_sha(&session.path, config)?);
    let remote_head_sha =
        crate::git::remote_branch_head_sha(&session.path, &session.branch, config)?;
    let pr_head_sha = summary.map(|summary| summary.head_sha.clone());
    let base_sha = match summary {
        Some(summary) => {
            crate::git::remote_branch_head_sha(&session.path, &summary.base_ref, config)?
        }
        None => None,
    };
    let current_guard = WorkGuard {
        local_head_sha,
        remote_head_sha,
        pr_head_sha,
        base_sha,
        review_thread_ids: Vec::new(),
    };
    match kind {
        super::stabilization_model::RepairKind::Review => {
            let tracked = crate::review::build_tracked_review_fix_prompt(session, config)?;
            Ok(StandaloneRepair {
                kind,
                prompt: tracked.prompt,
                guard: WorkGuard {
                    review_thread_ids: tracked.review_thread_ids,
                    ..current_guard
                },
            })
        }
        super::stabilization_model::RepairKind::Ci => Ok(StandaloneRepair {
            kind,
            prompt: crate::ci::build_ci_failure_prompt(session, config)?,
            guard: current_guard,
        }),
        super::stabilization_model::RepairKind::Merge => {
            Err("standalone merge repair is not supported".to_string())
        }
    }
}

pub(crate) fn decide_work_guard(
    repair_kind: &super::stabilization_model::RepairKind,
    guard: &WorkGuard,
    current: &WorkGuard,
) -> WorkGuardDecision {
    for (label, expected, actual) in [
        ("local HEAD", &guard.local_head_sha, &current.local_head_sha),
        (
            "remote branch",
            &guard.remote_head_sha,
            &current.remote_head_sha,
        ),
        (
            "pull request head",
            &guard.pr_head_sha,
            &current.pr_head_sha,
        ),
        ("pull request base", &guard.base_sha, &current.base_sha),
    ] {
        if expected != actual {
            return WorkGuardDecision::Invalidated {
                reason: format!("{label} changed while the repair was in progress"),
            };
        }
    }
    if repair_kind == &super::stabilization_model::RepairKind::Review {
        let expected_threads = guard
            .review_thread_ids
            .iter()
            .collect::<std::collections::BTreeSet<_>>();
        let current_threads = current
            .review_thread_ids
            .iter()
            .collect::<std::collections::BTreeSet<_>>();
        if expected_threads != current_threads {
            return WorkGuardDecision::Invalidated {
                reason: "review thread obligations changed while the repair was in progress"
                    .to_string(),
            };
        }
    }
    WorkGuardDecision::Valid
}

pub(crate) fn queue_standalone_repair(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    repair: StandaloneRepair,
) -> Result<i64, String> {
    let original = persisted.clone();
    let step_key = match &repair.kind {
        super::stabilization_model::RepairKind::Review => AutoStepKey::FixReview,
        super::stabilization_model::RepairKind::Ci => AutoStepKey::FixCi,
        super::stabilization_model::RepairKind::Merge => {
            return Err("standalone merge repair is not supported".to_string());
        }
    };
    let (blocker, next_work) = match &repair.kind {
        super::stabilization_model::RepairKind::Review => (
            super::stabilization_model::StabilizationBlocker::ReviewFeedbackFound,
            super::stabilization_model::StabilizationWorkKind::FixReview,
        ),
        super::stabilization_model::RepairKind::Ci => (
            super::stabilization_model::StabilizationBlocker::CiFailed,
            super::stabilization_model::StabilizationWorkKind::FixCi,
        ),
        super::stabilization_model::RepairKind::Merge => unreachable!(),
    };
    apply_state(
        persisted,
        &super::stabilization_model::StabilizationState {
            status: super::stabilization_model::StabilizationStatus::Blocked,
            blocker,
            next_work,
            reason: "standalone PR repair requested from a trustworthy observation".to_string(),
        },
    );
    let result = super::append_step_run_with_work_guard(
        conn,
        persisted,
        step_key,
        Some(repair.prompt),
        repair.guard,
        None,
    );
    if result.is_err() {
        *persisted = original;
    }
    result
}

pub(crate) fn append_repair_continuation(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<bool, String> {
    let Some(continuation) = next_repair_continuation(persisted) else {
        return Ok(false);
    };
    super::append_step_run_with_work_guard(
        conn,
        persisted,
        continuation.step_key,
        Some(continuation.reason.to_string()),
        continuation.guard.unwrap_or_default(),
        None,
    )?;
    Ok(true)
}

pub(crate) fn append_planned_work(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    work: StabilizationWorkItem,
) -> Result<bool, String> {
    let Some(step_key) = step_for_work(&work.kind) else {
        save_run_with_conn(conn, &persisted.run)?;
        return Ok(false);
    };
    if has_active_or_completed_step_after_latest_pr(persisted, &step_key) {
        save_run_with_conn(conn, &persisted.run)?;
        return Ok(false);
    }
    super::append_step_run_with_work_guard(
        conn,
        persisted,
        step_key,
        Some(work.reason),
        work.guard,
        Some(work.blocker),
    )?;
    Ok(true)
}

pub(crate) fn review_wait_decision(
    work: &StabilizationWorkItem,
    repair_prompt_available: bool,
) -> WaitDecision {
    use super::stabilization_model::StabilizationWorkKind;
    match &work.kind {
        StabilizationWorkKind::FixReview if repair_prompt_available => WaitDecision::QueueRepair,
        StabilizationWorkKind::FixReview => WaitDecision::KeepWaiting,
        StabilizationWorkKind::WaitForReview => WaitDecision::KeepWaiting,
        StabilizationWorkKind::Escalate => WaitDecision::Escalate(work.reason.clone()),
        _ => WaitDecision::Continue,
    }
}

pub(crate) fn ci_wait_decision(work: &StabilizationWorkItem) -> WaitDecision {
    use super::stabilization_model::StabilizationWorkKind;
    match &work.kind {
        StabilizationWorkKind::FixCi => WaitDecision::QueueRepair,
        StabilizationWorkKind::WaitForCi => WaitDecision::KeepWaiting,
        StabilizationWorkKind::Escalate => WaitDecision::Escalate(work.reason.clone()),
        _ => WaitDecision::Continue,
    }
}

pub(crate) fn advance_review_wait(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    work: StabilizationWorkItem,
    summary: String,
    repair_prompt: Option<String>,
) -> Result<WaitProgress, String> {
    match review_wait_decision(&work, repair_prompt.is_some()) {
        WaitDecision::QueueRepair => queue_wait_repair(
            conn,
            persisted,
            step_index,
            AutoStepKey::FixReview,
            super::MAX_REVIEW_FIX_ATTEMPTS,
            format!(
                "review feedback remained after {} repair attempts",
                super::MAX_REVIEW_FIX_ATTEMPTS
            ),
            summary,
            repair_prompt.expect("review repair prompt checked by decision"),
            work.guard,
        ),
        WaitDecision::Continue => {
            super::finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                AutoStepStatus::Skipped,
                Some(summary),
                None,
            )?;
            Ok(WaitProgress::Completed)
        }
        WaitDecision::Escalate(reason) => Err(reason),
        WaitDecision::KeepWaiting => Ok(WaitProgress::KeepWaiting),
    }
}

pub(crate) fn advance_ci_wait(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    work: StabilizationWorkItem,
    summary: String,
    repair_prompt: String,
) -> Result<WaitProgress, String> {
    match ci_wait_decision(&work) {
        WaitDecision::QueueRepair => queue_wait_repair(
            conn,
            persisted,
            step_index,
            AutoStepKey::FixCi,
            super::MAX_CI_FIX_ATTEMPTS,
            format!(
                "CI remained failing after {} repair attempts",
                super::MAX_CI_FIX_ATTEMPTS
            ),
            summary,
            repair_prompt,
            work.guard,
        ),
        WaitDecision::Continue => {
            super::finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                AutoStepStatus::Done,
                Some(summary),
                None,
            )?;
            Ok(WaitProgress::Completed)
        }
        WaitDecision::Escalate(reason) => Err(reason),
        WaitDecision::KeepWaiting => Ok(WaitProgress::KeepWaiting),
    }
}

#[allow(clippy::too_many_arguments)]
fn queue_wait_repair(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    step_key: AutoStepKey,
    max_attempts: usize,
    exhausted_error: String,
    summary: String,
    prompt: String,
    guard: WorkGuard,
) -> Result<WaitProgress, String> {
    if persisted.next_attempt_for(&step_key) > max_attempts {
        return Err(exhausted_error);
    }
    let original = persisted.clone();
    let result = (|| {
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
                .map_err(|error| format!("begin wait repair transaction: {error}"))?;
        super::finish_non_agent_step(
            &tx,
            &mut persisted.steps[step_index],
            AutoStepStatus::Done,
            Some(summary),
            None,
        )?;
        super::append_step_run_with_work_guard_in_transaction(
            &tx,
            persisted,
            step_key,
            Some(prompt),
            guard,
            None,
        )?;
        tx.commit()
            .map_err(|error| format!("commit wait repair transaction: {error}"))?;
        Ok(WaitProgress::RepairQueued)
    })();
    if result.is_err() {
        *persisted = original;
    }
    result
}

fn step_for_work(kind: &super::stabilization_model::StabilizationWorkKind) -> Option<AutoStepKey> {
    use super::stabilization_model::StabilizationWorkKind;
    match kind {
        StabilizationWorkKind::RunImplementation => Some(AutoStepKey::Implement),
        StabilizationWorkKind::RunPlan => Some(AutoStepKey::RunPlan),
        StabilizationWorkKind::RunLocalVerification => Some(AutoStepKey::LocalVerify),
        StabilizationWorkKind::CommitImplementation => Some(AutoStepKey::CommitImpl),
        StabilizationWorkKind::PushInitialAndOpenPr => Some(AutoStepKey::PushPr),
        StabilizationWorkKind::FixReview => Some(AutoStepKey::FixReview),
        StabilizationWorkKind::VerifyReviewFix => Some(AutoStepKey::VerifyReviewFix),
        StabilizationWorkKind::CommitReviewFix => Some(AutoStepKey::CommitReviewFix),
        StabilizationWorkKind::FixCi => Some(AutoStepKey::FixCi),
        StabilizationWorkKind::VerifyCiFix => Some(AutoStepKey::VerifyCiFix),
        StabilizationWorkKind::CommitCiFix => Some(AutoStepKey::CommitCiFix),
        StabilizationWorkKind::WaitForCi => Some(AutoStepKey::WaitCi),
        StabilizationWorkKind::WaitForReview => Some(AutoStepKey::WaitReview),
        StabilizationWorkKind::MarkReadyForManualMerge | StabilizationWorkKind::Merge => {
            Some(AutoStepKey::Merge)
        }
        StabilizationWorkKind::PushPendingRepair
        | StabilizationWorkKind::Done
        | StabilizationWorkKind::Escalate => None,
    }
}

fn has_active_or_completed_step_after_latest_pr(
    persisted: &PersistedAutoRun,
    key: &AutoStepKey,
) -> bool {
    let pr_sequence = persisted
        .steps
        .iter()
        .rev()
        .find(|step| step.step_key == AutoStepKey::PushPr && step.status == AutoStepStatus::Done)
        .map(|step| step.sequence)
        .unwrap_or(0);
    persisted.steps.iter().any(|step| {
        step.sequence > pr_sequence
            && step.step_key.as_str() == key.as_str()
            && matches!(
                step.status,
                AutoStepStatus::Queued
                    | AutoStepStatus::Starting
                    | AutoStepStatus::Running
                    | AutoStepStatus::Waiting
                    | AutoStepStatus::Done
                    | AutoStepStatus::Skipped
            )
    })
}

pub(crate) fn observe_plan_and_save(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
) -> Result<StabilizationWorkItem, String> {
    let work = observe_and_plan(repo, config, persisted);
    save_run_with_conn(conn, &persisted.run)?;
    Ok(work)
}

pub(crate) fn repair_commit_message(
    config: &Config,
    kind: &super::stabilization_model::RepairKind,
) -> String {
    let (template_name, default) = match kind {
        super::stabilization_model::RepairKind::Review => ("repair_commit_review", "fix: cr"),
        super::stabilization_model::RepairKind::Ci => ("repair_commit_ci", "fix: ci"),
        super::stabilization_model::RepairKind::Merge => ("repair_commit_merge", "fix: merge"),
    };
    config
        .prompt_template(template_name)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .unwrap_or(default)
        .to_string()
}

pub(crate) fn validate_and_begin_repair_commit(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    kind: super::stabilization_model::RepairKind,
    observation: RepairCommitObservation,
) -> Result<RepairCommitGate, String> {
    let original_guard = persisted.steps[step_index]
        .work_guard
        .clone()
        .ok_or_else(|| {
            format!(
                "{} repair commit is missing its work guard",
                repair_label(&kind)
            )
        })?;
    if let WorkGuardDecision::Invalidated { reason } =
        decide_work_guard(&kind, &original_guard, &observation.guard)
    {
        let summary = format!("repair guard invalidated before commit: {reason}");
        super::finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Skipped,
            Some(summary.clone()),
            None,
        )?;
        observe_plan_and_save(conn, repo, config, persisted)?;
        return Ok(RepairCommitGate::Invalidated { summary });
    }

    let expected_local_head_sha = observation
        .guard
        .local_head_sha
        .clone()
        .ok_or_else(|| "repair commit observation is missing local HEAD".to_string())?;
    persisted.run.pending_push = Some(PendingPushGuard {
        repair_kind: kind.clone(),
        commit_sha: String::new(),
        expected_local_head_sha,
        expected_remote_head_sha: observation.guard.remote_head_sha,
        pr_number: observation.pr_number.or(persisted.run.pr_number),
        expected_pr_head_sha: observation.guard.pr_head_sha,
        expected_base_sha: observation.guard.base_sha,
        guarded_review_thread_ids: if kind == super::stabilization_model::RepairKind::Review {
            original_guard.review_thread_ids
        } else {
            Vec::new()
        },
    });
    apply_state(
        persisted,
        &super::stabilization_model::StabilizationState {
            status: super::stabilization_model::StabilizationStatus::Blocked,
            blocker: super::stabilization_model::StabilizationBlocker::PendingPush,
            next_work: super::stabilization_model::StabilizationWorkKind::PushPendingRepair,
            reason: "verified repair is awaiting guarded commit and push".to_string(),
        },
    );
    save_run_with_conn(conn, &persisted.run)?;
    Ok(RepairCommitGate::Ready)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn complete_repair_commit(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    kind: super::stabilization_model::RepairKind,
    result: crate::git::GitCommitResult,
    local_head_sha: Option<String>,
    pr_summary: Option<crate::github::PrSummary>,
    cache: &mut crate::github::PrCache,
) -> Result<RepairCommitOutcome, String> {
    persisted.run.current_head_sha = local_head_sha.clone();
    if let Some(summary) = &pr_summary {
        persisted.run.pr_number = Some(summary.number);
        persisted.run.pr_url = Some(summary.url.clone());
        persisted.run.review_baseline_json = Some(super::review_baseline_json(summary));
    }

    if !result.committed {
        persisted.run.pending_push = None;
        let (status, summary) = match kind {
            super::stabilization_model::RepairKind::Review => {
                (AutoStepStatus::Skipped, result.message)
            }
            super::stabilization_model::RepairKind::Ci => (
                AutoStepStatus::Failed,
                "CI fix produced no commitable changes".to_string(),
            ),
            super::stabilization_model::RepairKind::Merge => (
                AutoStepStatus::Failed,
                "merge fix produced no commitable changes".to_string(),
            ),
        };
        super::finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            status,
            Some(summary.clone()),
            (status == AutoStepStatus::Failed).then_some(summary.clone()),
        )?;
        save_run_with_conn(conn, &persisted.run)?;
        if status == AutoStepStatus::Failed {
            return Err(summary);
        }
        return Ok(RepairCommitOutcome { summary });
    }

    let commit_sha = result.commit_sha.clone().ok_or_else(|| {
        format!(
            "{} repair commit did not report its SHA",
            repair_label(&kind)
        )
    })?;
    let guard = persisted.run.pending_push.as_mut().ok_or_else(|| {
        format!(
            "{} repair commit lost its persisted obligation",
            repair_label(&kind)
        )
    })?;
    guard.commit_sha = commit_sha.clone();
    guard.expected_local_head_sha = local_head_sha.clone().unwrap_or_else(|| commit_sha.clone());
    save_run_with_conn(conn, &persisted.run)?;
    if config.auto.push_repairs {
        progress_pending_push(conn, repo, config, persisted, cache, || Ok(()))?;
    }

    let step = &mut persisted.steps[step_index];
    step.commit_sha = Some(commit_sha.clone());
    step.head_sha = local_head_sha;
    let label = match kind {
        super::stabilization_model::RepairKind::Review => "review",
        super::stabilization_model::RepairKind::Ci => "CI",
        super::stabilization_model::RepairKind::Merge => "merge",
    };
    let summary = if config.auto.push_repairs {
        format!("committed {label} fixes as {commit_sha} and pushed")
    } else {
        format!("committed {label} fixes as {commit_sha}; pending guarded push")
    };
    super::finish_non_agent_step(
        conn,
        step,
        AutoStepStatus::Done,
        Some(summary.clone()),
        None,
    )?;
    persisted.run.status = persisted.authoritative_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)?;
    Ok(RepairCommitOutcome { summary })
}

fn repair_label(kind: &super::stabilization_model::RepairKind) -> &'static str {
    match kind {
        super::stabilization_model::RepairKind::Review => "review",
        super::stabilization_model::RepairKind::Ci => "CI",
        super::stabilization_model::RepairKind::Merge => "merge",
    }
}

pub(crate) fn decide_guarded_push(
    guard: &PendingPushGuard,
    local_head_sha: Option<&str>,
    remote_head_sha: Option<&str>,
    pr_number: Option<u64>,
    pr_head_sha: Option<&str>,
    base_sha: Option<&str>,
) -> GuardedPushDecision {
    if guard.pr_number.is_some() && pr_number != guard.pr_number {
        return GuardedPushDecision::Invalidated {
            reason: "the guarded pull request identity changed".to_string(),
        };
    }
    if guard.expected_base_sha.is_some() && base_sha != guard.expected_base_sha.as_deref() {
        return GuardedPushDecision::Invalidated {
            reason: "the guarded pull request base moved".to_string(),
        };
    }
    if guard.pr_number.is_some() && guard.expected_base_sha.is_none() {
        return GuardedPushDecision::Invalidated {
            reason: "the guarded pull request base was unavailable when the guard was created"
                .to_string(),
        };
    }
    if guard.pr_number.is_some() && guard.expected_pr_head_sha.is_none() {
        return GuardedPushDecision::Invalidated {
            reason: "the guarded pull request head was unavailable when the guard was created"
                .to_string(),
        };
    }
    if let Some(expected_pr_head) = guard.expected_pr_head_sha.as_deref()
        && pr_head_sha != Some(expected_pr_head)
        && pr_head_sha != Some(guard.commit_sha.as_str())
    {
        return GuardedPushDecision::Invalidated {
            reason: format!(
                "PR head moved from {} to {}",
                short_sha(expected_pr_head),
                pr_head_sha.map(short_sha).unwrap_or("unknown".to_string())
            ),
        };
    }
    if pr_head_sha == Some(guard.commit_sha.as_str())
        || remote_head_sha == Some(guard.commit_sha.as_str())
    {
        return GuardedPushDecision::AlreadySatisfied;
    }

    if local_head_sha != Some(guard.expected_local_head_sha.as_str()) {
        return GuardedPushDecision::Invalidated {
            reason: format!(
                "local HEAD moved from {} to {}",
                short_sha(&guard.expected_local_head_sha),
                local_head_sha
                    .map(short_sha)
                    .unwrap_or("unknown".to_string())
            ),
        };
    }

    if remote_head_sha != guard.expected_remote_head_sha.as_deref() {
        return GuardedPushDecision::Invalidated {
            reason: format!(
                "remote branch moved from {} to {}",
                guard
                    .expected_remote_head_sha
                    .as_deref()
                    .map(short_sha)
                    .unwrap_or("none".to_string()),
                remote_head_sha.map(short_sha).unwrap_or("none".to_string())
            ),
        };
    }

    GuardedPushDecision::ValidToPush
}

pub(crate) fn progress_pending_push(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    cache: &mut crate::github::PrCache,
    before_push: impl FnOnce() -> Result<(), String>,
) -> Result<GuardedPushProgress, String> {
    if persisted
        .run
        .pending_push
        .as_ref()
        .is_some_and(|guard| guard.commit_sha.is_empty())
    {
        let reason = "the interrupted repair commit cannot be identified after restart".to_string();
        persisted.run.pending_push = None;
        observe_plan_and_save(conn, repo, config, persisted)?;
        return Ok(GuardedPushProgress::Invalidated { reason });
    }
    crate::git::fetch_origin(&persisted.run.worktree_path, config)?;
    crate::github::refresh_pr_cache(
        repo,
        &persisted.run.branch,
        cache,
        &persisted.run.worktree_path,
        config,
        true,
    )?;
    let guard = persisted
        .run
        .pending_push
        .clone()
        .ok_or_else(|| "repair push is missing its persisted guard".to_string())?;
    let summary = cache.trusted_summary()?;
    let local_head = crate::git::current_head_sha(&persisted.run.worktree_path, config).ok();
    let remote_head = crate::git::remote_branch_head_sha(
        &persisted.run.worktree_path,
        &persisted.run.branch,
        config,
    )?;
    let base_sha = match summary {
        Some(summary) => crate::git::remote_branch_head_sha(
            &persisted.run.worktree_path,
            &summary.base_ref,
            config,
        )?,
        None => None,
    };
    let decision = decide_guarded_push(
        &guard,
        local_head.as_deref(),
        remote_head.as_deref(),
        summary.map(|summary| summary.number),
        summary.map(|summary| summary.head_sha.as_str()),
        base_sha.as_deref(),
    );
    let progress = match decision {
        GuardedPushDecision::Invalidated { reason } => {
            invalidate_pending_push(conn, repo, config, persisted, &reason)?;
            return Ok(GuardedPushProgress::Invalidated { reason });
        }
        GuardedPushDecision::AlreadySatisfied => GuardedPushProgress::AlreadySatisfied,
        GuardedPushDecision::ValidToPush => {
            before_push()?;
            crate::git::push_current_branch(&persisted.run.worktree_path, config)?;
            GuardedPushProgress::Pushed
        }
    };

    refresh_after_guarded_effect(repo, config, persisted, cache)?;
    let summary = cache.trusted_summary()?.ok_or_else(|| {
        "guarded repair push completed but the pull request disappeared".to_string()
    })?;
    if summary.number != guard.pr_number.unwrap_or(summary.number)
        || summary.head_sha != guard.commit_sha
    {
        return Err(
            "guarded repair push is not yet authoritatively visible on the pull request"
                .to_string(),
        );
    }

    while let Some(thread_id) = persisted
        .run
        .pending_push
        .as_ref()
        .and_then(|guard| guard.guarded_review_thread_ids.first().cloned())
    {
        crate::git::fetch_origin(&persisted.run.worktree_path, config)?;
        refresh_after_guarded_effect(repo, config, persisted, cache)?;
        let guard = persisted
            .run
            .pending_push
            .as_ref()
            .expect("pending push exists while resolving obligations");
        let summary = cache
            .trusted_summary()?
            .ok_or_else(|| "pull request disappeared while resolving review threads".to_string())?;
        let base_sha = crate::git::remote_branch_head_sha(
            &persisted.run.worktree_path,
            &summary.base_ref,
            config,
        )?;
        let local_head = crate::git::current_head_sha(&persisted.run.worktree_path, config)?;
        let remote_head = crate::git::remote_branch_head_sha(
            &persisted.run.worktree_path,
            &persisted.run.branch,
            config,
        )?;
        if let GuardedPushDecision::Invalidated { reason } = decide_guarded_push(
            guard,
            Some(local_head.as_str()),
            remote_head.as_deref(),
            Some(summary.number),
            Some(summary.head_sha.as_str()),
            base_sha.as_deref(),
        ) {
            invalidate_pending_push(conn, repo, config, persisted, &reason)?;
            return Ok(GuardedPushProgress::Invalidated { reason });
        }
        let unresolved = cache.trusted_details()?.is_some_and(|details| {
            details
                .review_comments
                .iter()
                .any(|comment| comment.thread_id == thread_id && !comment.resolved)
        });
        if unresolved {
            crate::github::resolve_review_thread(&persisted.run.worktree_path, config, &thread_id)?;
        }
        if let Some(guard) = persisted.run.pending_push.as_mut() {
            guard
                .guarded_review_thread_ids
                .retain(|candidate| candidate != &thread_id);
        }
        save_run_with_conn(conn, &persisted.run)?;
    }

    refresh_after_guarded_effect(repo, config, persisted, cache)?;
    let completed_guard = persisted.run.pending_push.take();
    if let Err(error) = observe_plan_and_save(conn, repo, config, persisted) {
        persisted.run.pending_push = completed_guard;
        let _ = save_run_with_conn(conn, &persisted.run);
        return Err(error);
    }
    Ok(progress)
}

fn refresh_after_guarded_effect(
    repo: &Repository,
    config: &Config,
    persisted: &PersistedAutoRun,
    cache: &mut crate::github::PrCache,
) -> Result<(), String> {
    crate::github::refresh_pr_cache(
        repo,
        &persisted.run.branch,
        cache,
        &persisted.run.worktree_path,
        config,
        true,
    )
}

fn invalidate_pending_push(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    reason: &str,
) -> Result<(), String> {
    persisted.run.pending_push = None;
    observe_plan_and_save(conn, repo, config, persisted)?;
    append_auto_event(
        conn,
        &AutoEvent {
            id: None,
            run_id: persisted.run.id.clone(),
            step_run_id: persisted.run.selected_step_run_id,
            time_unix_ms: unix_ms(),
            kind: "guard_invalidated".to_string(),
            data_json: serde_json::json!({ "reason": reason }).to_string(),
        },
    )
    .map(|_| ())
}

fn short_sha(value: &str) -> String {
    value.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_flow::{
        AutoLaunch, AutoStepRun,
        stabilization_model::{
            ActionableReviewItem, CiFacts, MergeabilityFacts, PolicyBlocker, PolicyFacts,
            PullRequestFacts, PullRequestState, RepairKind, RepositoryFacts, ReviewFacts,
            StabilizationGoal, StabilizationSnapshot, WorktreeFacts,
        },
    };
    use crate::config::Config;
    use crate::github::{CiFailure, PrCheckState};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn blocked_manual_merge_cases_never_invoke_gh_merge() {
        let (temp, config, log) = manual_merge_test_config();
        let cases = vec![
            {
                let mut snapshot = ready_manual_merge_snapshot();
                snapshot.worktree.dirty = true;
                snapshot
            },
            {
                let mut snapshot = ready_manual_merge_snapshot();
                snapshot.pull_request.as_mut().unwrap().draft = true;
                snapshot
            },
            {
                let mut snapshot = ready_manual_merge_snapshot();
                snapshot.repository.default_base = Some("release".to_string());
                snapshot
            },
            {
                let mut snapshot = ready_manual_merge_snapshot();
                snapshot.worktree.remote_head_sha = Some("remote".to_string());
                snapshot
            },
            {
                let mut snapshot = ready_manual_merge_snapshot();
                snapshot.pull_request.as_mut().unwrap().observation_error =
                    Some("details refresh failed".to_string());
                snapshot
            },
            {
                let mut snapshot = ready_manual_merge_snapshot();
                snapshot
                    .pull_request
                    .as_mut()
                    .unwrap()
                    .review
                    .actionable_reviews
                    .push(ActionableReviewItem::ReviewBody {
                        review_id: "review".to_string(),
                        author: "reviewer".to_string(),
                        state: "CHANGES_REQUESTED".to_string(),
                        body: "fix this".to_string(),
                        submitted_at: "now".to_string(),
                    });
                snapshot
            },
            {
                let mut snapshot = ready_manual_merge_snapshot();
                snapshot.pull_request.as_mut().unwrap().ci.aggregate = PrCheckState::Failed;
                snapshot
            },
            {
                let mut snapshot = ready_manual_merge_snapshot();
                snapshot.pull_request.as_mut().unwrap().mergeability = MergeabilityFacts::Blocked {
                    reason: "conflict".to_string(),
                };
                snapshot
            },
            {
                let mut snapshot = ready_manual_merge_snapshot();
                snapshot.policy = PolicyFacts::Blocked {
                    blockers: vec![PolicyBlocker::RequiredApprovalMissing],
                };
                snapshot
            },
            {
                let mut snapshot = ready_manual_merge_snapshot();
                snapshot.policy = PolicyFacts::Unknown {
                    reason: Some("refresh failed".to_string()),
                };
                snapshot
            },
        ];

        for snapshot in cases {
            let authorization = match stabilization_plan::manual_merge_authorization(&snapshot) {
                Ok(work) => MergeAuthorization::Authorized(AuthorizedMerge {
                    pr_number: 42,
                    guard: work.guard,
                }),
                Err(state) => MergeAuthorization::Blocked(state),
            };
            let execution = execute_merge_authorization(&config, &temp, authorization)
                .expect("blocked authorization is not a mutation error");
            assert!(matches!(execution, ManualMergeExecution::Blocked(_)));
        }

        assert!(!log.exists(), "blocked cases must not invoke gh at all");
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn ready_manual_merge_authorization_invokes_gh_merge() {
        let (temp, config, log) = manual_merge_test_config();
        let snapshot = ready_manual_merge_snapshot();
        let authorization = stabilization_plan::manual_merge_authorization(&snapshot).unwrap();

        let execution = execute_merge_authorization(
            &config,
            &temp,
            MergeAuthorization::Authorized(AuthorizedMerge {
                pr_number: snapshot.pull_request.unwrap().number,
                guard: authorization.guard,
            }),
        )
        .unwrap();

        assert_eq!(execution, ManualMergeExecution::Merged { pr_number: 42 });
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            "pr merge 42 --squash --match-head-commit head\n"
        );
        assert_eq!(
            authorization.kind,
            super::super::stabilization_model::StabilizationWorkKind::MarkReadyForManualMerge
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn manual_merge_authorization_cannot_switch_pr_or_guard_after_checks() {
        let guard = WorkGuard {
            local_head_sha: Some("head".to_string()),
            remote_head_sha: Some("head".to_string()),
            pr_head_sha: Some("head".to_string()),
            base_sha: Some("base".to_string()),
            review_thread_ids: Vec::new(),
        };
        let switched_pr = reauthorize_observed_merge(
            AuthorizedMerge {
                pr_number: 42,
                guard: guard.clone(),
            },
            AuthorizedMerge {
                pr_number: 43,
                guard: guard.clone(),
            },
        );
        let mut changed_guard = guard.clone();
        changed_guard.local_head_sha = Some("new-head".to_string());
        let moved_head = reauthorize_observed_merge(
            AuthorizedMerge {
                pr_number: 42,
                guard,
            },
            AuthorizedMerge {
                pr_number: 42,
                guard: changed_guard,
            },
        );

        assert!(matches!(switched_pr, MergeAuthorization::Blocked(_)));
        assert!(matches!(moved_head, MergeAuthorization::Blocked(_)));
    }

    #[test]
    fn valid_guarded_push_is_allowed() {
        let guard = guard();

        let decision = decide_guarded_push(
            &guard,
            Some("repair"),
            Some("remote"),
            Some(42),
            Some("remote"),
            Some("base"),
        );

        assert_eq!(decision, GuardedPushDecision::ValidToPush);
    }

    #[test]
    fn already_pushed_commit_is_satisfied() {
        let guard = guard();

        let decision = decide_guarded_push(
            &guard,
            Some("repair"),
            Some("repair"),
            Some(42),
            Some("repair"),
            Some("base"),
        );

        assert_eq!(decision, GuardedPushDecision::AlreadySatisfied);
    }

    #[test]
    fn local_head_movement_invalidates_guard() {
        let guard = guard();

        let decision = decide_guarded_push(
            &guard,
            Some("other"),
            Some("remote"),
            Some(42),
            Some("remote"),
            Some("base"),
        );

        assert!(matches!(decision, GuardedPushDecision::Invalidated { .. }));
    }

    #[test]
    fn remote_head_movement_invalidates_guard() {
        let guard = guard();

        let decision = decide_guarded_push(
            &guard,
            Some("repair"),
            Some("other"),
            Some(42),
            Some("remote"),
            Some("base"),
        );

        assert!(matches!(decision, GuardedPushDecision::Invalidated { .. }));
    }

    #[test]
    fn unavailable_expected_pr_head_invalidates_guard() {
        let guard = guard();

        let decision = decide_guarded_push(
            &guard,
            Some("repair"),
            Some("remote"),
            Some(42),
            None,
            Some("base"),
        );

        assert!(matches!(decision, GuardedPushDecision::Invalidated { .. }));
    }

    #[test]
    fn changed_original_work_guard_is_invalidated_before_commit() {
        let original = WorkGuard {
            local_head_sha: Some("head-a".to_string()),
            remote_head_sha: Some("head-a".to_string()),
            pr_head_sha: Some("head-a".to_string()),
            base_sha: Some("base-a".to_string()),
            review_thread_ids: vec!["thread-1".to_string()],
        };
        let mut current = original.clone();
        current.pr_head_sha = Some("head-b".to_string());

        assert!(matches!(
            decide_work_guard(&RepairKind::Review, &original, &current),
            WorkGuardDecision::Invalidated { .. }
        ));
    }

    #[test]
    fn changed_actionable_review_thread_invalidates_commit_guard() {
        let original = WorkGuard {
            review_thread_ids: vec!["thread-1".to_string()],
            ..WorkGuard::default()
        };
        let current = WorkGuard {
            review_thread_ids: vec!["thread-2".to_string()],
            ..WorkGuard::default()
        };

        assert!(matches!(
            decide_work_guard(&RepairKind::Review, &original, &current),
            WorkGuardDecision::Invalidated { .. }
        ));
    }

    #[test]
    fn ci_work_guard_ignores_review_thread_changes() {
        let original = WorkGuard {
            review_thread_ids: Vec::new(),
            ..WorkGuard::default()
        };
        let current = WorkGuard {
            review_thread_ids: vec!["unresolved-thread".to_string()],
            ..WorkGuard::default()
        };

        assert_eq!(
            decide_work_guard(&RepairKind::Ci, &original, &current),
            WorkGuardDecision::Valid
        );
        assert!(matches!(
            decide_work_guard(&RepairKind::Review, &original, &current),
            WorkGuardDecision::Invalidated { .. }
        ));
    }

    #[test]
    fn review_guard_survives_agent_verify_and_commit_continuations() {
        let mut persisted = AutoLaunch::new(
            Path::new("/repo"),
            Path::new("/repo/feature"),
            "feature",
            "repair",
        )
        .unwrap()
        .create_run();
        persisted.steps.clear();
        let mut fix = AutoStepRun::queued(&persisted.run.id, 1, AutoStepKey::FixReview, 1, None);
        fix.status = AutoStepStatus::Done;
        fix.work_guard = Some(WorkGuard {
            review_thread_ids: vec!["thread-1".to_string()],
            ..WorkGuard::default()
        });
        persisted.steps.push(fix);

        let verify = next_repair_continuation(&persisted).expect("verify continuation");
        assert_eq!(verify.step_key, AutoStepKey::VerifyReviewFix);
        assert_eq!(
            verify.guard.as_ref().unwrap().review_thread_ids,
            vec!["thread-1".to_string()]
        );

        let mut verify_step = AutoStepRun::queued(&persisted.run.id, 2, verify.step_key, 1, None);
        verify_step.status = AutoStepStatus::Done;
        verify_step.work_guard = verify.guard;
        persisted.steps.push(verify_step);

        let commit = next_repair_continuation(&persisted).expect("commit continuation");
        assert_eq!(commit.step_key, AutoStepKey::CommitReviewFix);
        assert_eq!(
            commit.guard.unwrap().review_thread_ids,
            vec!["thread-1".to_string()]
        );
    }

    #[test]
    fn standalone_review_and_ci_repairs_share_one_queueing_path() {
        for (kind, expected, blocker, next_work) in [
            (
                RepairKind::Review,
                AutoStepKey::FixReview,
                super::super::stabilization_model::StabilizationBlocker::ReviewFeedbackFound,
                super::super::stabilization_model::StabilizationWorkKind::FixReview,
            ),
            (
                RepairKind::Ci,
                AutoStepKey::FixCi,
                super::super::stabilization_model::StabilizationBlocker::CiFailed,
                super::super::stabilization_model::StabilizationWorkKind::FixCi,
            ),
        ] {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            super::super::migrate_schema(&conn).unwrap();
            let mut persisted = AutoLaunch::new(
                Path::new("/repo"),
                Path::new("/repo/feature"),
                "feature",
                "repair",
            )
            .unwrap()
            .create_run();
            persisted.run.variant = "repair".to_string();
            persisted.steps.clear();
            super::super::save_auto_run(&conn, &mut persisted).unwrap();

            queue_standalone_repair(
                &conn,
                &mut persisted,
                StandaloneRepair {
                    kind,
                    prompt: "repair this PR".to_string(),
                    guard: WorkGuard::default(),
                },
            )
            .unwrap();

            assert_eq!(persisted.steps.len(), 1);
            assert_eq!(persisted.steps[0].step_key, expected);
            assert_eq!(
                persisted.run.stabilization_status,
                Some(super::super::stabilization_model::StabilizationStatus::Blocked)
            );
            assert_eq!(persisted.run.stabilization_blocker, Some(blocker));
            assert_eq!(persisted.run.stabilization_next_work, Some(next_work));
            assert!(!persisted.steps.iter().any(|step| matches!(
                step.step_key,
                AutoStepKey::Implement | AutoStepKey::RunPlan
            )));
        }
    }

    #[test]
    fn standalone_repair_late_write_failure_rolls_back_run_step_and_guard() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        super::super::migrate_schema(&conn).unwrap();
        let mut persisted = AutoLaunch::new(
            Path::new("/repo"),
            Path::new("/repo/feature"),
            "feature",
            "repair",
        )
        .unwrap()
        .create_run();
        persisted.run.variant = "repair".to_string();
        persisted.steps.clear();
        super::super::save_auto_run(&conn, &mut persisted).unwrap();
        conn.execute_batch(
            "create trigger fail_guarded_queue_late
             before update of selected_step_run_id on auto_run
             when new.selected_step_run_id is not null
             begin
               select raise(fail, 'injected late write failure');
             end;",
        )
        .unwrap();

        let error = queue_standalone_repair(
            &conn,
            &mut persisted,
            StandaloneRepair {
                kind: RepairKind::Review,
                prompt: "repair this PR".to_string(),
                guard: WorkGuard {
                    review_thread_ids: vec!["thread-1".to_string()],
                    ..WorkGuard::default()
                },
            },
        )
        .expect_err("late run write should fail");

        assert!(error.contains("injected late write failure"));
        assert!(persisted.steps.is_empty());
        assert_eq!(persisted.run.selected_step_run_id, None);
        let loaded = super::super::load_auto_run(&conn, &persisted.run.id)
            .unwrap()
            .unwrap();
        assert!(loaded.steps.is_empty());
        assert_eq!(loaded.run.selected_step_run_id, None);
    }

    #[test]
    fn wait_repair_late_failure_restores_wait_step_and_does_not_enqueue() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        super::super::migrate_schema(&conn).unwrap();
        let mut persisted = AutoLaunch::new(
            Path::new("/repo"),
            Path::new("/repo/feature"),
            "feature",
            "repair",
        )
        .unwrap()
        .create_run();
        persisted.steps.clear();
        let mut wait = AutoStepRun::queued(
            &persisted.run.id,
            1,
            AutoStepKey::WaitCi,
            1,
            Some("wait for CI".to_string()),
        );
        wait.status = AutoStepStatus::Waiting;
        persisted.steps.push(wait);
        super::super::save_auto_run(&conn, &mut persisted).unwrap();
        let original = persisted.clone();
        conn.execute_batch(
            "create trigger fail_wait_repair_late
             before update of selected_step_run_id on auto_run
             when new.selected_step_run_id is not old.selected_step_run_id
             begin
               select raise(fail, 'injected wait repair failure');
             end;",
        )
        .unwrap();

        let error = queue_wait_repair(
            &conn,
            &mut persisted,
            0,
            AutoStepKey::FixCi,
            3,
            "attempts exhausted".to_string(),
            "CI failed".to_string(),
            "fix CI".to_string(),
            WorkGuard::default(),
        )
        .expect_err("late enqueue write should roll back the wait completion");

        assert!(error.contains("injected wait repair failure"));
        assert_eq!(persisted, original);
        let loaded = super::super::load_auto_run(&conn, &persisted.run.id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.steps.len(), 1);
        assert_eq!(loaded.steps[0].status, AutoStepStatus::Waiting);
        assert_eq!(
            loaded.run.selected_step_run_id,
            original.run.selected_step_run_id
        );
    }

    #[test]
    fn auto_merge_reauthorization_rejects_every_changed_merge_guard() {
        let mut ready = ready_manual_merge_snapshot();
        ready.goal.auto_merge = true;
        let expected_guard = stabilization_plan::plan(&ready).guard;
        let expected_pr = ready.pull_request.as_ref().map(|pr| pr.number);
        assert!(matches!(
            authorize_auto_merge(&ready, expected_pr, &expected_guard),
            MergeAuthorization::Authorized(_)
        ));

        let mut cases = Vec::new();
        let mut dirty = ready.clone();
        dirty.worktree.dirty = true;
        cases.push(dirty);
        let mut head = ready.clone();
        head.worktree.local_head_sha = Some("changed".to_string());
        cases.push(head);
        let mut base = ready.clone();
        base.pull_request.as_mut().unwrap().base_ref = "release".to_string();
        cases.push(base);
        let mut policy = ready.clone();
        policy.policy = PolicyFacts::Unknown {
            reason: Some("policy refresh failed".to_string()),
        };
        cases.push(policy);
        let mut review = ready.clone();
        review
            .pull_request
            .as_mut()
            .unwrap()
            .review
            .actionable_reviews
            .push(ActionableReviewItem::ReviewBody {
                review_id: "new-review".to_string(),
                author: "reviewer".to_string(),
                state: "CHANGES_REQUESTED".to_string(),
                body: "please revise".to_string(),
                submitted_at: "later".to_string(),
            });
        cases.push(review);
        let mut ci = ready.clone();
        ci.pull_request.as_mut().unwrap().ci.aggregate = PrCheckState::Failed;
        cases.push(ci);

        for changed in cases {
            assert!(matches!(
                authorize_auto_merge(&changed, expected_pr, &expected_guard),
                MergeAuthorization::Blocked(_)
            ));
        }
        assert!(matches!(
            authorize_auto_merge(&ready, Some(99), &expected_guard),
            MergeAuthorization::Blocked(_)
        ));
    }

    fn ready_manual_merge_snapshot() -> StabilizationSnapshot {
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
            pull_request: Some(PullRequestFacts {
                number: 42,
                url: "https://example.test/pr/42".to_string(),
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
            }),
            policy: PolicyFacts::Satisfied,
            goal: StabilizationGoal {
                auto_merge: false,
                cleanup_after_merge: false,
            },
            pending_push: None,
        }
    }

    fn manual_merge_test_config() -> (PathBuf, Config, PathBuf) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp = std::env::temp_dir().join(format!(
            "prism-manual-merge-authorization-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("gh.log");
        let mut config = crate::test_support::test_config();
        config.default_base = Some("main".to_string());
        crate::test_support::install_tool(
            &mut config,
            &temp,
            "gh",
            &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
        );
        (temp, config, log)
    }

    fn guard() -> PendingPushGuard {
        PendingPushGuard {
            repair_kind: RepairKind::Review,
            commit_sha: "repair".to_string(),
            expected_local_head_sha: "repair".to_string(),
            expected_remote_head_sha: Some("remote".to_string()),
            pr_number: Some(42),
            expected_pr_head_sha: Some("remote".to_string()),
            expected_base_sha: Some("base".to_string()),
            guarded_review_thread_ids: vec!["thread-1".to_string()],
        }
    }
}
