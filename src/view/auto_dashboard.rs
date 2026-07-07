use super::*;

pub(super) fn auto_dashboard_lines(
    dashboard: &crate::view::AutoDashboard,
    _width: usize,
    visible_rows: usize,
) -> Vec<Line<'static>> {
    let run = &dashboard.run.run;
    let selected_step = run
        .selected_step_run_id
        .and_then(|id| dashboard.run.steps.iter().find(|step| step.id == Some(id)))
        .or_else(|| dashboard.run.steps.first());
    let counts = dashboard.run.status_counts();
    let output_tail = dashboard
        .output_lines
        .last()
        .map(|line| short_log_tail(&line.text));
    let mut lines = vec![
        heading_line("Auto Flow"),
        labelled_line("task", run.prompt_summary.clone()),
    ];
    if let Some(step) = selected_step {
        lines.push(Line::from(vec![
            Span::styled("current ", muted_style()),
            Span::raw(format!(
                "{} attempt {} ",
                step.step_key.as_str(),
                step.attempt
            )),
            Span::styled(
                auto_step_status_label(step.status),
                auto_step_status_style(step.status),
            ),
            Span::raw(
                output_tail
                    .as_ref()
                    .map(|tail| format!("  {tail}"))
                    .unwrap_or_default(),
            ),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("mode ", muted_style()),
        Span::raw(auto_mode_label(run.mode)),
        Span::styled("  status ", muted_style()),
        Span::styled(auto_status_label(run.status), auto_style(run.status)),
        Span::styled("  elapsed ", muted_style()),
        Span::raw(elapsed_label(run.created_unix_ms, run.updated_unix_ms)),
    ]));
    lines.extend([
        Line::from(vec![
            Span::styled("source ", muted_style()),
            Span::raw(auto_source_label(run.implementation_source)),
            Span::raw(
                run.plan_path
                    .as_ref()
                    .map(|path| format!("  plan {}", path.display()))
                    .unwrap_or_default(),
            ),
        ]),
        labelled_line("branch", run.branch.clone()),
    ]);
    if let Some(pr_number) = run.pr_number {
        lines.push(labelled_line("pr", format!("#{pr_number}")));
    }
    if let Some(step) = selected_step {
        if let Some(session_id) = step.opencode_session_id.as_deref() {
            lines.push(labelled_line(
                "opencode session",
                short_id(session_id).to_string(),
            ));
        }
        if let Some(summary) = step.summary.as_deref().or(step.reason.as_deref()) {
            lines.push(labelled_line("latest", summary.to_string()));
        }
        if let Some(error) = step.error.as_deref() {
            lines.push(Line::from(vec![
                Span::styled("error ", muted_style()),
                Span::styled(error.to_string(), error_style()),
            ]));
        }
    }
    lines.push(Line::from(vec![
        Span::styled("counts queued ", muted_style()),
        Span::raw((counts.queued + counts.starting).to_string()),
        Span::styled("  running ", muted_style()),
        Span::raw(counts.running.to_string()),
        Span::styled("  waiting ", muted_style()),
        Span::raw(counts.waiting.to_string()),
        Span::styled("  done ", muted_style()),
        Span::raw(counts.done.to_string()),
        Span::styled("  failed ", muted_style()),
        Span::raw(counts.failed.to_string()),
    ]));
    lines.push(Line::from(""));
    lines.push(heading_line("Checklist"));
    let linked_plan_rows_reserved = linked_plan_summary_lines(dashboard).len();
    let step_rows_available = visible_rows
        .saturating_sub(lines.len())
        .saturating_sub(linked_plan_rows_reserved)
        .max(3);
    lines.extend(auto_checklist_lines(
        dashboard,
        step_rows_available,
        output_tail.as_deref(),
    ));
    lines.extend(linked_plan_summary_lines(dashboard));
    lines.truncate(visible_rows);
    lines
}

pub(super) fn linked_plan_summary_lines(
    dashboard: &crate::view::AutoDashboard,
) -> Vec<Line<'static>> {
    let selected_step = dashboard
        .run
        .run
        .selected_step_run_id
        .and_then(|id| dashboard.run.steps.iter().find(|step| step.id == Some(id)))
        .or_else(|| dashboard.run.steps.first());
    if !matches!(
        selected_step.map(|step| &step.step_key),
        Some(AutoStepKey::RunPlan)
    ) {
        return Vec::new();
    }
    let Some(plan_dashboard) = dashboard.linked_plan_dashboard.as_ref() else {
        if selected_step
            .and_then(|step| step.plan_run_id.as_ref())
            .is_some()
        {
            return vec![Line::from(Span::styled(
                "linked plan unavailable",
                muted_style(),
            ))];
        }
        return Vec::new();
    };
    let plan_run = &plan_dashboard.run.run;
    let selected_phase = plan_dashboard
        .run
        .steps
        .iter()
        .find(|step| step.step == plan_run.selected_step)
        .or_else(|| plan_dashboard.run.steps.first());
    let mut lines = vec![Line::from(vec![
        Span::styled("linked plan ", muted_style()),
        Span::raw(plan_run.plan_display.clone()),
        Span::styled("  status ", muted_style()),
        Span::styled(
            plan_run_status_label(plan_run.status),
            plan_run_status_style(plan_run.status),
        ),
        Span::styled("  mode ", muted_style()),
        Span::raw(plan_mode_label(plan_run.mode)),
    ])];
    if let Some(phase) = selected_phase {
        lines.push(Line::from(vec![
            Span::styled("phase ", muted_style()),
            Span::raw(format!("{}/{} ", phase.step, plan_run.total_steps)),
            Span::styled(
                plan_step_status_label(phase.status),
                plan_step_status_style(phase.status),
            ),
            Span::raw(
                phase
                    .latest_message
                    .as_ref()
                    .map(|message| format!("  {message}"))
                    .unwrap_or_default(),
            ),
        ]));
        if let Some(error) = phase.error.as_deref() {
            lines.push(Line::from(vec![
                Span::styled("plan error ", muted_style()),
                Span::styled(error.to_string(), error_style()),
            ]));
        }
    }
    if let Some(line) = plan_dashboard.output_lines.last() {
        lines.push(labelled_line("plan output", line.text.clone()));
    }
    lines.push(Line::from(""));
    lines
}

pub(super) fn auto_checklist_lines(
    dashboard: &crate::view::AutoDashboard,
    max_rows: usize,
    output_tail: Option<&str>,
) -> Vec<Line<'static>> {
    let run = &dashboard.run.run;
    let steps = &dashboard.run.steps;
    let selected_step_run_id = run
        .selected_step_run_id
        .or_else(|| dashboard.run.steps.first().and_then(|step| step.id));
    let mut lines = Vec::new();
    lines.push(checklist_line_for_key(
        0,
        steps,
        AutoStepKey::Prepare,
        "Prepare worktree".to_string(),
        selected_step_run_id,
        output_tail,
    ));

    if run.implementation_source == AutoImplementationSource::DraftPlan {
        lines.push(checklist_line_for_key(
            0,
            steps,
            AutoStepKey::CreatePlan,
            "Create implementation plan".to_string(),
            selected_step_run_id,
            output_tail,
        ));
        lines.push(checklist_line_for_key(
            0,
            steps,
            AutoStepKey::ReviewPlan,
            "Review implementation plan".to_string(),
            selected_step_run_id,
            output_tail,
        ));
        lines.push(checklist_line_for_key(
            0,
            steps,
            AutoStepKey::ApprovePlan,
            "Approve implementation plan".to_string(),
            selected_step_run_id,
            output_tail,
        ));
    }

    if run.implementation_source == AutoImplementationSource::Prompt {
        let label = format!("Implement \"{}\"", truncate(&run.prompt_summary, 50));
        lines.push(checklist_line_for_key(
            0,
            steps,
            AutoStepKey::Implement,
            label,
            selected_step_run_id,
            output_tail,
        ));
    } else {
        let plan_name = dashboard
            .linked_plan_dashboard
            .as_ref()
            .map(|plan| plan.run.run.plan_display.clone())
            .or_else(|| {
                run.plan_path
                    .as_ref()
                    .map(|path| path.display().to_string())
            })
            .unwrap_or_else(|| run.prompt_summary.clone());
        lines.push(checklist_line(
            0,
            plan_implementation_status(dashboard),
            format!("Run plan {plan_name}"),
        ));
        if let Some(plan) = dashboard.linked_plan_dashboard.as_ref() {
            for phase in &plan.run.steps {
                lines.push(checklist_line(
                    1,
                    plan_step_as_auto_status(phase.status),
                    format!("Run Phase {}", phase.step),
                ));
            }
        }
    }

    push_local_validation_loop(&mut lines, steps, selected_step_run_id, output_tail);
    lines.push(checklist_line_for_key(
        0,
        steps,
        AutoStepKey::CommitImpl,
        "Commit implementation".to_string(),
        selected_step_run_id,
        output_tail,
    ));
    lines.push(checklist_line_for_key(
        0,
        steps,
        AutoStepKey::PushPr,
        "Create or update PR".to_string(),
        selected_step_run_id,
        output_tail,
    ));
    push_review_loop(&mut lines, steps, selected_step_run_id, output_tail);
    push_ci_loop(&mut lines, steps, selected_step_run_id, output_tail);
    lines.push(checklist_line_for_key(
        0,
        steps,
        AutoStepKey::Merge,
        "Run final merge safety gate".to_string(),
        selected_step_run_id,
        output_tail,
    ));
    lines.push(checklist_line_for_key(
        0,
        steps,
        AutoStepKey::Cleanup,
        "Clean up merged worktree/session".to_string(),
        selected_step_run_id,
        output_tail,
    ));

    if lines.len() > max_rows {
        lines.truncate(max_rows.saturating_sub(1));
        lines.push(Line::from(Span::styled("  ...", muted_style())));
    }
    lines
}

pub(super) fn push_local_validation_loop(
    lines: &mut Vec<Line<'static>>,
    steps: &[AutoStepRun],
    selected_step_run_id: Option<i64>,
    output_tail: Option<&str>,
) {
    let group_status = first_active_or_latest_status(
        steps,
        &[AutoStepKey::FixLocalVerify, AutoStepKey::LocalVerify],
    );
    lines.push(checklist_line(
        0,
        group_status,
        "Local validation loop".to_string(),
    ));
    lines.push(checklist_line_for_key(
        1,
        steps,
        AutoStepKey::LocalVerify,
        "Run local validation".to_string(),
        selected_step_run_id,
        output_tail,
    ));
    if step_seen(steps, &AutoStepKey::FixLocalVerify) {
        lines.push(checklist_line(
            1,
            auto_status_for_key(steps, AutoStepKey::FixLocalVerify),
            format!(
                "Fix local validation failure ({})",
                attempt_label(steps, &AutoStepKey::FixLocalVerify, 3)
            ),
        ));
        lines.push(checklist_line(
            1,
            auto_status_for_key(steps, AutoStepKey::LocalVerify),
            "Re-run local validation".to_string(),
        ));
    }
}

pub(super) fn push_review_loop(
    lines: &mut Vec<Line<'static>>,
    steps: &[AutoStepRun],
    selected_step_run_id: Option<i64>,
    output_tail: Option<&str>,
) {
    let group_status = if step_seen(steps, &AutoStepKey::WaitCi) {
        AutoStepStatus::Done
    } else {
        first_active_or_latest_status(
            steps,
            &[
                AutoStepKey::FixReview,
                AutoStepKey::VerifyReviewFix,
                AutoStepKey::CommitReviewFix,
                AutoStepKey::WaitReview,
            ],
        )
    };
    lines.push(checklist_line(
        0,
        group_status,
        "Review feedback loop".to_string(),
    ));
    lines.push(checklist_line_for_key(
        1,
        steps,
        AutoStepKey::WaitReview,
        "Wait for automated review".to_string(),
        selected_step_run_id,
        output_tail,
    ));
    if step_seen(steps, &AutoStepKey::FixReview) {
        lines.push(checklist_line(
            1,
            auto_status_for_key(steps, AutoStepKey::FixReview),
            format!(
                "Fix review feedback ({})",
                attempt_label(steps, &AutoStepKey::FixReview, 3)
            ),
        ));
        lines.push(checklist_line(
            1,
            auto_status_for_key(steps, AutoStepKey::VerifyReviewFix),
            "Verify review fixes".to_string(),
        ));
        lines.push(checklist_line(
            1,
            auto_status_for_key(steps, AutoStepKey::CommitReviewFix),
            "Commit and push review fixes".to_string(),
        ));
        if step_count(steps, &AutoStepKey::WaitReview) > 1 {
            lines.push(checklist_line(
                1,
                auto_status_for_key(steps, AutoStepKey::WaitReview),
                "Wait for automated review again".to_string(),
            ));
        }
    }
}

pub(super) fn push_ci_loop(
    lines: &mut Vec<Line<'static>>,
    steps: &[AutoStepRun],
    selected_step_run_id: Option<i64>,
    output_tail: Option<&str>,
) {
    let group_status = if step_seen(steps, &AutoStepKey::Merge) {
        AutoStepStatus::Done
    } else {
        first_active_or_latest_status(
            steps,
            &[
                AutoStepKey::FixCi,
                AutoStepKey::VerifyCiFix,
                AutoStepKey::CommitCiFix,
                AutoStepKey::WaitCi,
            ],
        )
    };
    lines.push(checklist_line(0, group_status, "CI loop".to_string()));
    lines.push(checklist_line_for_key(
        1,
        steps,
        AutoStepKey::WaitCi,
        "Wait for PR checks".to_string(),
        selected_step_run_id,
        output_tail,
    ));
    if step_seen(steps, &AutoStepKey::FixCi) {
        lines.push(checklist_line(
            1,
            auto_status_for_key(steps, AutoStepKey::FixCi),
            format!(
                "Fix CI failure ({})",
                attempt_label(steps, &AutoStepKey::FixCi, 3)
            ),
        ));
        lines.push(checklist_line(
            1,
            auto_status_for_key(steps, AutoStepKey::VerifyCiFix),
            "Verify CI fixes".to_string(),
        ));
        lines.push(checklist_line(
            1,
            auto_status_for_key(steps, AutoStepKey::CommitCiFix),
            "Commit and push CI fixes".to_string(),
        ));
        if step_count(steps, &AutoStepKey::WaitCi) > 1 {
            lines.push(checklist_line(
                1,
                auto_status_for_key(steps, AutoStepKey::WaitCi),
                "Wait for PR checks again".to_string(),
            ));
        }
    }
}

pub(super) fn checklist_line_for_key(
    indent: usize,
    steps: &[AutoStepRun],
    key: AutoStepKey,
    label: String,
    selected_step_run_id: Option<i64>,
    output_tail: Option<&str>,
) -> Line<'static> {
    let status = auto_status_for_key(steps, key.clone());
    let tail = auto_step_tail(steps, &key, selected_step_run_id, output_tail);
    checklist_line_with_tail(indent, status, label, tail)
}

pub(super) fn checklist_line_with_tail(
    indent: usize,
    status: AutoStepStatus,
    label: String,
    tail: Option<String>,
) -> Line<'static> {
    let mut line = checklist_line(indent, status, label);
    if let Some(tail) = tail.filter(|tail| !tail.is_empty()) {
        line.spans
            .push(Span::styled(format!("  {tail}"), muted_style()));
    }
    line
}

pub(super) fn auto_step_tail(
    steps: &[AutoStepRun],
    key: &AutoStepKey,
    selected_step_run_id: Option<i64>,
    output_tail: Option<&str>,
) -> Option<String> {
    let step = latest_step_for_key(steps, key)?;
    if step.id == selected_step_run_id
        && let Some(output_tail) = output_tail
    {
        return Some(output_tail.to_string());
    }
    step.error
        .as_deref()
        .or(step.summary.as_deref())
        .or(step.reason.as_deref())
        .map(short_log_tail)
}

pub(super) fn checklist_line(
    indent: usize,
    status: AutoStepStatus,
    label: String,
) -> Line<'static> {
    Line::from(vec![
        Span::raw("  ".repeat(indent)),
        Span::styled(checklist_mark(status), auto_step_status_style(status)),
        Span::raw(" "),
        Span::styled(label, auto_step_status_style(status)),
    ])
}

pub(super) fn checklist_mark(status: AutoStepStatus) -> &'static str {
    match status {
        AutoStepStatus::Done | AutoStepStatus::Skipped => "[x]",
        AutoStepStatus::Failed | AutoStepStatus::Aborted => "[!]",
        AutoStepStatus::Starting | AutoStepStatus::Running | AutoStepStatus::Waiting => "[-]",
        AutoStepStatus::Queued => "[ ]",
    }
}

pub(super) fn auto_status_for_key(steps: &[AutoStepRun], key: AutoStepKey) -> AutoStepStatus {
    latest_step_for_key(steps, &key)
        .map(|step| step.status)
        .unwrap_or(AutoStepStatus::Queued)
}

pub(super) fn latest_step_for_key<'a>(
    steps: &'a [AutoStepRun],
    key: &AutoStepKey,
) -> Option<&'a AutoStepRun> {
    steps.iter().rev().find(|step| &step.step_key == key)
}

pub(super) fn step_seen(steps: &[AutoStepRun], key: &AutoStepKey) -> bool {
    steps.iter().any(|step| &step.step_key == key)
}

pub(super) fn step_count(steps: &[AutoStepRun], key: &AutoStepKey) -> usize {
    steps.iter().filter(|step| &step.step_key == key).count()
}

pub(super) fn attempt_label(
    steps: &[AutoStepRun],
    key: &AutoStepKey,
    max_attempts: usize,
) -> String {
    let attempt = latest_step_for_key(steps, key)
        .map(|step| step.attempt)
        .unwrap_or(1);
    format!("attempt {}/{max_attempts}", attempt.min(max_attempts))
}

pub(super) fn first_active_or_latest_status(
    steps: &[AutoStepRun],
    keys: &[AutoStepKey],
) -> AutoStepStatus {
    for key in keys {
        let status = auto_status_for_key(steps, key.clone());
        if matches!(
            status,
            AutoStepStatus::Starting
                | AutoStepStatus::Running
                | AutoStepStatus::Waiting
                | AutoStepStatus::Failed
                | AutoStepStatus::Aborted
        ) {
            return status;
        }
    }
    keys.iter()
        .rev()
        .map(|key| auto_status_for_key(steps, key.clone()))
        .find(|status| *status != AutoStepStatus::Queued)
        .unwrap_or(AutoStepStatus::Queued)
}

pub(super) fn plan_implementation_status(dashboard: &crate::view::AutoDashboard) -> AutoStepStatus {
    if let Some(plan) = dashboard.linked_plan_dashboard.as_ref() {
        if plan
            .run
            .steps
            .iter()
            .all(|step| step.status == PlanStepStatus::Done)
        {
            return AutoStepStatus::Done;
        }
        if plan
            .run
            .steps
            .iter()
            .any(|step| step.status == PlanStepStatus::Failed)
        {
            return AutoStepStatus::Failed;
        }
        if plan.run.steps.iter().any(|step| {
            matches!(
                step.status,
                PlanStepStatus::Starting | PlanStepStatus::Running
            )
        }) {
            return AutoStepStatus::Running;
        }
    }
    auto_status_for_key(&dashboard.run.steps, AutoStepKey::RunPlan)
}

pub(super) fn plan_step_as_auto_status(status: PlanStepStatus) -> AutoStepStatus {
    match status {
        PlanStepStatus::Queued => AutoStepStatus::Queued,
        PlanStepStatus::Starting => AutoStepStatus::Starting,
        PlanStepStatus::Running => AutoStepStatus::Running,
        PlanStepStatus::Done => AutoStepStatus::Done,
        PlanStepStatus::Failed => AutoStepStatus::Failed,
        PlanStepStatus::Aborted => AutoStepStatus::Aborted,
        PlanStepStatus::Skipped => AutoStepStatus::Skipped,
    }
}

pub(super) fn auto_output_row(
    line: &crate::auto_flow::AutoOutputLine,
    selected: bool,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(if selected { "▶ " } else { "  " }, title_style(selected)),
        Span::styled(
            format!("{:<10}", auto_output_kind_label(line.kind)),
            auto_output_kind_style(line.kind),
        ),
        Span::raw(format!(" {}", line.text)),
    ])
}
