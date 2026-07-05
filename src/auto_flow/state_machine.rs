use super::*;

pub fn append_step_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    step_key: AutoStepKey,
    reason: Option<String>,
) -> Result<i64, String> {
    let mut step = AutoStepRun::queued(
        &persisted.run.id,
        persisted.next_sequence(),
        step_key.clone(),
        persisted.next_attempt_for(&step_key),
        reason,
    );
    let id = save_step_with_conn(conn, &mut step)?;
    persisted.run.selected_step_run_id = Some(id);
    persisted.steps.push(step);
    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)?;
    Ok(id)
}

pub(super) fn next_queued_agent_step(persisted: &PersistedAutoRun) -> Option<usize> {
    persisted.steps.iter().position(|step| {
        step.status == AutoStepStatus::Queued
            && matches!(
                step.step_key,
                AutoStepKey::CreatePlan
                    | AutoStepKey::ReviewPlan
                    | AutoStepKey::Implement
                    | AutoStepKey::FixLocalVerify
                    | AutoStepKey::FixReview
                    | AutoStepKey::FixCi
                    | AutoStepKey::Custom(_)
            )
    })
}

pub(super) fn next_queued_non_agent_step(persisted: &PersistedAutoRun) -> Option<usize> {
    persisted.steps.iter().position(|step| {
        step.status == AutoStepStatus::Queued
            && matches!(
                step.step_key,
                AutoStepKey::ApprovePlan
                    | AutoStepKey::RunPlan
                    | AutoStepKey::LocalVerify
                    | AutoStepKey::CommitImpl
                    | AutoStepKey::PushPr
                    | AutoStepKey::WaitReview
                    | AutoStepKey::VerifyReviewFix
                    | AutoStepKey::CommitReviewFix
                    | AutoStepKey::WaitCi
                    | AutoStepKey::VerifyCiFix
                    | AutoStepKey::CommitCiFix
                    | AutoStepKey::Merge
                    | AutoStepKey::Cleanup
            )
    })
}

pub(super) fn has_queued_non_agent_step(persisted: &PersistedAutoRun) -> bool {
    next_queued_non_agent_step(persisted).is_some()
}

pub(super) fn has_queued_auto_step(persisted: &PersistedAutoRun) -> bool {
    next_queued_agent_step(persisted).is_some() || next_queued_non_agent_step(persisted).is_some()
}

pub(super) fn has_pending_auto_work(persisted: &PersistedAutoRun) -> bool {
    has_queued_auto_step(persisted)
        || queued_prepare_needs_initial_agent_step(persisted)
        || next_state_machine_step_needed(persisted)
        || implementation_follow_up_step_needed(persisted)
}

pub(super) fn pause_before_next_auto_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<(), String> {
    if matches!(
        persisted.run.status,
        AutoRunStatus::Failed | AutoRunStatus::Aborted | AutoRunStatus::Done
    ) {
        return Ok(());
    }
    if !has_queued_auto_step(persisted) {
        ensure_next_auto_step(conn, persisted)?;
    }
    if !has_pending_auto_work(persisted) {
        return Ok(());
    }
    persisted.run.pause_requested = true;
    persisted.run.status = AutoRunStatus::Paused;
    persisted.run.updated_unix_ms = unix_ms();
    append_auto_event(
        conn,
        &AutoEvent {
            id: None,
            run_id: persisted.run.id.clone(),
            step_run_id: persisted.run.selected_step_run_id,
            time_unix_ms: persisted.run.updated_unix_ms,
            kind: "step_gate".to_string(),
            data_json: "{}".to_string(),
        },
    )?;
    save_run_with_conn(conn, &persisted.run)
}

pub(super) fn next_state_machine_step_needed(persisted: &PersistedAutoRun) -> bool {
    if persisted.run.implementation_source == AutoImplementationSource::DraftPlan {
        if !has_step_key(persisted, &AutoStepKey::CreatePlan) {
            return true;
        }
        if latest_step_status(persisted, &AutoStepKey::CreatePlan) == Some(AutoStepStatus::Done)
            && !has_step_key(persisted, &AutoStepKey::ReviewPlan)
        {
            return true;
        }
        if latest_step_status(persisted, &AutoStepKey::ReviewPlan) == Some(AutoStepStatus::Done)
            && !has_step_key(persisted, &AutoStepKey::ApprovePlan)
        {
            return true;
        }
        if latest_step_status(persisted, &AutoStepKey::ApprovePlan) != Some(AutoStepStatus::Done) {
            return false;
        }
    }
    !has_step_key(persisted, &implementation_step_key(persisted))
}

pub(super) fn implementation_follow_up_step_needed(persisted: &PersistedAutoRun) -> bool {
    latest_step_status(persisted, &implementation_step_key(persisted)) == Some(AutoStepStatus::Done)
        && !has_step_key(persisted, &AutoStepKey::LocalVerify)
}

pub(super) fn ensure_next_auto_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<bool, String> {
    if merge_or_manual_merge_complete(persisted) {
        persisted.run.status = AutoRunStatus::Done;
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        return Ok(false);
    }
    if ci_loop_complete(persisted) && !has_step_key(persisted, &AutoStepKey::Merge) {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::Merge,
            Some("run final merge safety gate".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::Merge) == Some(AutoStepStatus::Done)
        && !has_step_key(persisted, &AutoStepKey::Cleanup)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::Cleanup,
            Some("clean up merged local worktree/session data".to_string()),
        )?;
        return Ok(true);
    }
    if persisted.run.implementation_source == AutoImplementationSource::DraftPlan
        && !has_step_key(persisted, &AutoStepKey::CreatePlan)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::CreatePlan,
            Some("create implementation plan.md".to_string()),
        )?;
        return Ok(true);
    }
    if persisted.run.implementation_source == AutoImplementationSource::DraftPlan
        && latest_step_status(persisted, &AutoStepKey::CreatePlan) == Some(AutoStepStatus::Done)
        && !has_step_key(persisted, &AutoStepKey::ReviewPlan)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::ReviewPlan,
            Some("review implementation plan.md before coding".to_string()),
        )?;
        return Ok(true);
    }
    if persisted.run.implementation_source == AutoImplementationSource::DraftPlan
        && latest_step_status(persisted, &AutoStepKey::ReviewPlan) == Some(AutoStepStatus::Done)
        && !has_step_key(persisted, &AutoStepKey::ApprovePlan)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::ApprovePlan,
            Some("pause for user approval of plan.md".to_string()),
        )?;
        return Ok(true);
    }
    if persisted.run.implementation_source == AutoImplementationSource::DraftPlan
        && latest_step_status(persisted, &AutoStepKey::ApprovePlan) != Some(AutoStepStatus::Done)
    {
        return Ok(false);
    }
    let implementation_step_key = implementation_step_key(persisted);
    if !has_step_key(persisted, &implementation_step_key) {
        append_step_run(
            conn,
            persisted,
            implementation_step_key,
            Some(implementation_step_reason(persisted).to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &implementation_step_key) == Some(AutoStepStatus::Done)
        && !has_step_key(persisted, &AutoStepKey::LocalVerify)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::LocalVerify,
            Some("run local verification before committing".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::FixLocalVerify) == Some(AutoStepStatus::Done)
        && latest_unfinished_verify_after_fix(persisted) == Some(AutoStepKey::LocalVerify)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::LocalVerify,
            Some("retry local verification after repair".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::FixLocalVerify) == Some(AutoStepStatus::Done)
        && latest_unfinished_verify_after_fix(persisted) == Some(AutoStepKey::VerifyReviewFix)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::VerifyReviewFix,
            Some("retry review-fix verification after repair".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::LocalVerify) == Some(AutoStepStatus::Done)
        && !has_step_key(persisted, &AutoStepKey::CommitImpl)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::CommitImpl,
            Some("commit verified implementation changes".to_string()),
        )?;
        return Ok(true);
    }
    if matches!(
        latest_step_status(persisted, &AutoStepKey::CommitImpl),
        Some(AutoStepStatus::Done | AutoStepStatus::Skipped)
    ) && !has_step_key(persisted, &AutoStepKey::PushPr)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::PushPr,
            Some("push branch and create or refresh pull request".to_string()),
        )?;
        return Ok(true);
    }
    if has_step_status(persisted, &AutoStepKey::PushPr, AutoStepStatus::Done)
        && !has_step_key(persisted, &AutoStepKey::WaitReview)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::WaitReview,
            Some("wait for automated review feedback".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::FixReview) == Some(AutoStepStatus::Done)
        && latest_step_status(persisted, &AutoStepKey::VerifyReviewFix)
            != Some(AutoStepStatus::Queued)
        && latest_step_status(persisted, &AutoStepKey::VerifyReviewFix)
            != Some(AutoStepStatus::Done)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::VerifyReviewFix,
            Some("run review-fix verification before committing".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::VerifyReviewFix) == Some(AutoStepStatus::Done)
        && latest_step_status(persisted, &AutoStepKey::CommitReviewFix)
            != Some(AutoStepStatus::Queued)
        && latest_step_status(persisted, &AutoStepKey::CommitReviewFix)
            != Some(AutoStepStatus::Done)
        && latest_step_status(persisted, &AutoStepKey::CommitReviewFix)
            != Some(AutoStepStatus::Skipped)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::CommitReviewFix,
            Some("commit and push verified review fixes".to_string()),
        )?;
        return Ok(true);
    }
    if matches!(
        latest_step_status(persisted, &AutoStepKey::CommitReviewFix),
        Some(AutoStepStatus::Done | AutoStepStatus::Skipped)
    ) {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::WaitReview,
            Some("wait for fresh automated review feedback after review-fix push".to_string()),
        )?;
        return Ok(true);
    }
    if review_loop_complete(persisted) && !has_step_key(persisted, &AutoStepKey::WaitCi) {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::WaitCi,
            Some("wait for pull request checks".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::FixCi) == Some(AutoStepStatus::Done)
        && latest_step_status(persisted, &AutoStepKey::VerifyCiFix) != Some(AutoStepStatus::Queued)
        && latest_step_status(persisted, &AutoStepKey::VerifyCiFix) != Some(AutoStepStatus::Done)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::VerifyCiFix,
            Some("run CI-fix verification before committing".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::FixLocalVerify) == Some(AutoStepStatus::Done)
        && latest_unfinished_verify_after_fix(persisted) == Some(AutoStepKey::VerifyCiFix)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::VerifyCiFix,
            Some("retry CI-fix verification after repair".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::VerifyCiFix) == Some(AutoStepStatus::Done)
        && latest_step_status(persisted, &AutoStepKey::CommitCiFix) != Some(AutoStepStatus::Queued)
        && latest_step_status(persisted, &AutoStepKey::CommitCiFix) != Some(AutoStepStatus::Done)
        && latest_step_status(persisted, &AutoStepKey::CommitCiFix) != Some(AutoStepStatus::Skipped)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::CommitCiFix,
            Some("commit and push verified CI fixes".to_string()),
        )?;
        return Ok(true);
    }
    if matches!(
        latest_step_status(persisted, &AutoStepKey::CommitCiFix),
        Some(AutoStepStatus::Done)
    ) {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::WaitCi,
            Some("wait for pull request checks after CI-fix push".to_string()),
        )?;
        return Ok(true);
    }
    Ok(false)
}

pub(super) fn initial_agent_step(persisted: &PersistedAutoRun) -> (AutoStepKey, &'static str) {
    match persisted.run.implementation_source {
        AutoImplementationSource::Prompt => {
            (AutoStepKey::Implement, "run initial implementation prompt")
        }
        AutoImplementationSource::ExistingPlan => (AutoStepKey::RunPlan, "run plan phases"),
        AutoImplementationSource::DraftPlan => {
            (AutoStepKey::CreatePlan, "create implementation plan.md")
        }
    }
}

pub(super) fn implementation_step_key(persisted: &PersistedAutoRun) -> AutoStepKey {
    match persisted.run.implementation_source {
        AutoImplementationSource::Prompt => AutoStepKey::Implement,
        AutoImplementationSource::ExistingPlan | AutoImplementationSource::DraftPlan => {
            AutoStepKey::RunPlan
        }
    }
}

pub(super) fn implementation_step_reason(persisted: &PersistedAutoRun) -> &'static str {
    match persisted.run.implementation_source {
        AutoImplementationSource::Prompt => "run initial implementation prompt",
        AutoImplementationSource::ExistingPlan => "run plan phases from selected plan",
        AutoImplementationSource::DraftPlan => "run plan phases from approved plan.md",
    }
}

pub(super) fn has_step_key(persisted: &PersistedAutoRun, key: &AutoStepKey) -> bool {
    persisted
        .steps
        .iter()
        .any(|step| step.step_key.as_str() == key.as_str())
}

pub(super) fn has_step_status(
    persisted: &PersistedAutoRun,
    key: &AutoStepKey,
    status: AutoStepStatus,
) -> bool {
    persisted
        .steps
        .iter()
        .any(|step| step.step_key.as_str() == key.as_str() && step.status == status)
}

pub(super) fn latest_step_status(
    persisted: &PersistedAutoRun,
    key: &AutoStepKey,
) -> Option<AutoStepStatus> {
    persisted
        .steps
        .iter()
        .rev()
        .find(|step| step.step_key.as_str() == key.as_str())
        .map(|step| step.status)
}

pub(super) fn latest_unfinished_verify_after_fix(
    persisted: &PersistedAutoRun,
) -> Option<AutoStepKey> {
    let fix_sequence = persisted
        .steps
        .iter()
        .rev()
        .find(|step| {
            step.step_key == AutoStepKey::FixLocalVerify && step.status == AutoStepStatus::Done
        })?
        .sequence;
    persisted
        .steps
        .iter()
        .rev()
        .find(|step| {
            step.sequence < fix_sequence
                && matches!(
                    step.step_key,
                    AutoStepKey::LocalVerify
                        | AutoStepKey::VerifyReviewFix
                        | AutoStepKey::VerifyCiFix
                )
                && step.status == AutoStepStatus::Failed
        })
        .map(|step| step.step_key.clone())
}

pub(super) fn review_loop_complete(persisted: &PersistedAutoRun) -> bool {
    let Some(wait) = persisted
        .steps
        .iter()
        .rev()
        .find(|step| step.step_key == AutoStepKey::WaitReview)
    else {
        return false;
    };
    wait.status == AutoStepStatus::Skipped
        && persisted
            .steps
            .iter()
            .filter(|step| step.sequence > wait.sequence)
            .all(|step| {
                matches!(
                    step.status,
                    AutoStepStatus::Done | AutoStepStatus::Skipped | AutoStepStatus::Failed
                )
            })
}

pub(super) fn ci_loop_complete(persisted: &PersistedAutoRun) -> bool {
    let Some(wait) = persisted
        .steps
        .iter()
        .rev()
        .find(|step| step.step_key == AutoStepKey::WaitCi)
    else {
        return false;
    };
    wait.status == AutoStepStatus::Done
        && persisted
            .steps
            .iter()
            .filter(|step| step.sequence > wait.sequence)
            .all(|step| {
                matches!(
                    step.status,
                    AutoStepStatus::Done | AutoStepStatus::Skipped | AutoStepStatus::Failed
                )
            })
}

pub(super) fn merge_or_manual_merge_complete(persisted: &PersistedAutoRun) -> bool {
    match latest_step_status(persisted, &AutoStepKey::Merge) {
        Some(AutoStepStatus::Skipped) => true,
        Some(AutoStepStatus::Done) => matches!(
            latest_step_status(persisted, &AutoStepKey::Cleanup),
            Some(AutoStepStatus::Done | AutoStepStatus::Skipped)
        ),
        _ => false,
    }
}
