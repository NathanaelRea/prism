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
pub(crate) struct RepairContinuation {
    pub step_key: AutoStepKey,
    pub reason: &'static str,
    pub guard: Option<WorkGuard>,
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
    persisted.run.stabilization_status = Some(work.status());
    persisted.run.stabilization_blocker = Some(work.blocker.clone());
    persisted.run.stabilization_next_work = Some(work.kind.clone());
    persisted.run.status = persisted.authoritative_status();
    persisted.run.updated_unix_ms = unix_ms();
    work
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
        let current_head = crate::git::current_head_sha(&persisted.run.worktree_path, config)?;
        let pre_commit_head = persisted
            .run
            .pending_push
            .as_ref()
            .map(|guard| guard.expected_local_head_sha.as_str())
            .unwrap_or_default();
        if current_head == pre_commit_head {
            let reason = "the repair commit did not complete before restart".to_string();
            persisted.run.pending_push = None;
            observe_plan_and_save(conn, repo, config, persisted)?;
            return Ok(GuardedPushProgress::Invalidated { reason });
        }
        if let Some(guard) = persisted.run.pending_push.as_mut() {
            guard.commit_sha = current_head.clone();
            guard.expected_local_head_sha = current_head;
        }
        save_run_with_conn(conn, &persisted.run)?;
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
    let base_sha = summary.and_then(|summary| {
        crate::git::remote_branch_head_sha(&persisted.run.worktree_path, &summary.base_ref, config)
            .ok()
            .flatten()
    });
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
            )?;
            return Ok(GuardedPushProgress::Invalidated { reason });
        }
        GuardedPushDecision::AlreadySatisfied => GuardedPushProgress::AlreadySatisfied,
        GuardedPushDecision::ValidToPush => {
            before_push()?;
            crate::git::push_current_branch(&persisted.run.worktree_path, config)?;
            GuardedPushProgress::Pushed
        }
    };

    while let Some(thread_id) = persisted
        .run
        .pending_push
        .as_ref()
        .and_then(|guard| guard.guarded_review_thread_ids.first().cloned())
    {
        crate::github::resolve_review_thread(&persisted.run.worktree_path, config, &thread_id)?;
        if let Some(guard) = persisted.run.pending_push.as_mut() {
            guard
                .guarded_review_thread_ids
                .retain(|candidate| candidate != &thread_id);
        }
        save_run_with_conn(conn, &persisted.run)?;
    }

    crate::github::refresh_pr_cache(
        repo,
        &persisted.run.branch,
        cache,
        &persisted.run.worktree_path,
        config,
        true,
    )?;
    let completed_guard = persisted.run.pending_push.take();
    if let Err(error) = observe_plan_and_save(conn, repo, config, persisted) {
        persisted.run.pending_push = completed_guard;
        let _ = save_run_with_conn(conn, &persisted.run);
        return Err(error);
    }
    Ok(progress)
}

fn short_sha(value: &str) -> String {
    value.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_flow::{AutoLaunch, AutoStepRun, stabilization_model::RepairKind};
    use std::path::Path;

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
