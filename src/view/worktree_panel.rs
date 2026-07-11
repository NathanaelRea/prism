use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StabilizationPanelModel {
    pub icon_style: IconStyle,
    pub blocker: String,
    pub next: String,
    pub guard: Option<String>,
    pub ci: String,
    pub review: String,
    pub merge: String,
    pub policy: String,
    pub pending_commit: Option<String>,
}

pub(super) fn worktree_detail_lines(model: &crate::view::FrameModel<'_>) -> Vec<Line<'static>> {
    let Some(index) = model.selected_session else {
        return vec![Line::from(Span::styled(
            "No worktree selected",
            muted_style(),
        ))];
    };
    let Some(session) = model.sessions.get(index) else {
        return vec![Line::from(Span::styled(
            "Selected worktree is filtered",
            muted_style(),
        ))];
    };
    let mut lines = vec![
        Line::from(Span::styled(session.branch.clone(), title_style(true))),
        Line::from(Span::styled(session.path_display.clone(), muted_style())),
    ];
    if !session.prompt_summary.trim().is_empty() {
        lines.push(Line::from(""));
        lines.push(labelled_line("prompt", session.prompt_summary.clone()));
    }
    if let Some(stabilization) = stabilization_panel_model(model, session) {
        lines.push(Line::from(""));
        lines.extend(stabilization_panel_lines(&stabilization));
    }
    lines.push(Line::from(""));
    lines.extend(pr_panel_lines(
        model.config,
        Some(session),
        model.selected_comment,
    ));
    lines
}

pub(crate) fn stabilization_panel_model(
    model: &crate::view::FrameModel<'_>,
    session: &Session,
) -> Option<StabilizationPanelModel> {
    if session.is_default_branch(model.config) || session.is_detached() {
        return None;
    }

    let run = model
        .auto_dashboard
        .as_ref()
        .map(|dashboard| &dashboard.run.run);
    let blocker = run
        .and_then(|run| run.stabilization_blocker.as_ref())
        .cloned()
        .or_else(|| {
            run.and_then(|run| run.pending_push.as_ref())
                .map(|_| StabilizationBlocker::PendingPush)
        })
        .or_else(|| cached_pr_blocker(model.config, session));
    let blocker = blocker?;
    let next = run
        .and_then(|run| run.stabilization_next_work.as_ref())
        .cloned()
        .unwrap_or_else(|| cached_next_work(&blocker));
    let pending_push = run.and_then(|run| run.pending_push.as_ref());

    Some(StabilizationPanelModel {
        icon_style: model.config.icon_style,
        blocker: blocker_label(&blocker),
        next: work_label(&next),
        guard: pending_push.map(guard_label),
        ci: ci_gate_label(session, &blocker),
        review: review_gate_label(model.config, session),
        merge: merge_gate_label(session),
        policy: policy_gate_label(&blocker),
        pending_commit: pending_push.map(pending_commit_label),
    })
}

pub(crate) fn stabilization_panel_lines(model: &StabilizationPanelModel) -> Vec<Line<'static>> {
    let mut lines = vec![
        heading_line("PR Stabilization"),
        stabilization_value_line(
            "state",
            &model.blocker,
            stabilization_state_style(&model.blocker),
        ),
        stabilization_value_line("next", &model.next, attention_style()),
    ];
    lines.push(stabilization_gate_line("ci", &model.ci, model.icon_style));
    lines.push(stabilization_gate_line(
        "code review",
        &model.review,
        model.icon_style,
    ));
    lines.push(stabilization_gate_line(
        "merge conflicts",
        &model.merge,
        model.icon_style,
    ));
    lines.push(stabilization_gate_line(
        "policy",
        &model.policy,
        model.icon_style,
    ));
    if let Some(commit) = &model.pending_commit {
        lines.push(stabilization_gate_line(
            "pending push",
            commit,
            model.icon_style,
        ));
    }
    if let Some(guard) = &model.guard {
        lines.push(stabilization_gate_line("guard", guard, model.icon_style));
    }
    lines
}

fn stabilization_value_line(label: &'static str, value: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{:<16}", label), muted_style()),
        Span::styled(format!("{:<30}", truncate(value, 30)), style),
    ])
}

fn stabilization_gate_line(
    gate: &'static str,
    status: &str,
    icon_style: IconStyle,
) -> Line<'static> {
    let pending_detail = matches!(gate, "pending push" | "guard");
    let style = if pending_detail {
        attention_style()
    } else {
        gate_style(status)
    };
    let status_icon = if pending_detail {
        icon(icon_style, "…", "")
    } else {
        stabilization_status_icon(status, icon_style)
    };
    Line::from(vec![
        Span::styled(format!("{:<16}", gate), muted_style()),
        Span::styled(status_icon, style),
        Span::raw(" "),
        Span::styled(format!("{:<30}", truncate(status, 30)), style),
    ])
}

fn stabilization_status_icon(status: &str, icon_style: IconStyle) -> &'static str {
    let normalized = status.to_ascii_lowercase();
    if normalized.contains("fail") || normalized.contains("blocked") {
        icon(icon_style, "✕", "")
    } else if normalized.contains("missing") || normalized.contains("feedback") {
        icon(icon_style, "!", "")
    } else if normalized.contains("pending") || normalized.contains("running") {
        icon(icon_style, "…", "")
    } else if normalized.contains("unknown") {
        icon(icon_style, "?", "")
    } else if normalized.contains("disabled") {
        icon(icon_style, "⊘", "")
    } else if normalized.contains("pass")
        || normalized.contains("approved")
        || normalized.contains("clean")
        || normalized.contains("satisfied")
    {
        icon(icon_style, "✓", "")
    } else {
        icon(icon_style, "·", "")
    }
}

fn cached_pr_blocker(
    config: &crate::config::Config,
    session: &Session,
) -> Option<StabilizationBlocker> {
    let summary = session.pr.summary.as_ref()?;
    if summary.merged || summary.state.eq_ignore_ascii_case("merged") {
        return Some(StabilizationBlocker::Merged);
    }
    if merge_blocked(summary) {
        return Some(StabilizationBlocker::MergeBlocked);
    }
    if has_actionable_review_feedback(session) {
        return Some(StabilizationBlocker::ReviewFeedbackFound);
    }
    match summary.check_state() {
        crate::github::PrCheckState::Failed | crate::github::PrCheckState::Mixed => {
            return Some(StabilizationBlocker::CiFailed);
        }
        crate::github::PrCheckState::Pending => return Some(StabilizationBlocker::CiPending),
        crate::github::PrCheckState::Success | crate::github::PrCheckState::Unknown => {}
    }
    if config.auto.merge {
        Some(StabilizationBlocker::PolicyUnknown)
    } else {
        Some(StabilizationBlocker::ReadyForManualMerge)
    }
}

fn cached_next_work(blocker: &StabilizationBlocker) -> StabilizationWorkKind {
    match blocker {
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
        StabilizationBlocker::NeedsPullRequest => StabilizationWorkKind::PushInitialAndOpenPr,
        StabilizationBlocker::NeedsImplementation => StabilizationWorkKind::RunImplementation,
        _ => StabilizationWorkKind::Escalate,
    }
}

fn blocker_label(blocker: &StabilizationBlocker) -> String {
    pascal_label(blocker.as_str())
}

fn work_label(work: &StabilizationWorkKind) -> String {
    pascal_label(work.as_str())
}

fn pascal_label(value: &str) -> String {
    value
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<String>()
}

fn guard_label(guard: &PendingPushGuard) -> String {
    let mut parts = vec![format!(
        "head {}",
        short_sha(&guard.expected_local_head_sha)
    )];
    if let Some(base) = &guard.expected_base_sha {
        parts.push(format!("base {}", short_sha(base)));
    }
    if let Some(remote) = &guard.expected_remote_head_sha {
        parts.push(format!("remote {}", short_sha(remote)));
    }
    if let Some(pr_head) = &guard.expected_pr_head_sha {
        parts.push(format!("pr {}", short_sha(pr_head)));
    }
    parts.join("  ")
}

fn pending_commit_label(guard: &PendingPushGuard) -> String {
    format!(
        "{} {}",
        short_sha(&guard.commit_sha),
        repair_kind_label(&guard.repair_kind)
    )
}

fn repair_kind_label(kind: &crate::auto_flow::stabilization_model::RepairKind) -> &'static str {
    match kind {
        crate::auto_flow::stabilization_model::RepairKind::Review => "review repair",
        crate::auto_flow::stabilization_model::RepairKind::Ci => "ci repair",
        crate::auto_flow::stabilization_model::RepairKind::Merge => "merge repair",
    }
}

fn short_sha(value: &str) -> String {
    value.chars().take(7).collect()
}

fn ci_gate_label(session: &Session, blocker: &StabilizationBlocker) -> String {
    let Some(summary) = &session.pr.summary else {
        return "unknown".to_string();
    };
    let optional_failure_count = session
        .pr
        .details
        .as_ref()
        .map(|details| details.failing_checks.len())
        .unwrap_or(0);
    if optional_failure_count > 0 && ci_blockers_ruled_out(blocker) {
        return format!(
            "required passing ({} optional failing)",
            optional_failure_count
        );
    }
    if matches!(blocker, StabilizationBlocker::CiMissingRequiredChecks) {
        return "required missing".to_string();
    }
    let mut label = summary.check_state().label().to_string();
    if let Some(details) = &session.pr.details
        && !details.failing_checks.is_empty()
    {
        label = format!("{label} ({} failing)", details.failing_checks.len());
    }
    label
}

fn ci_blockers_ruled_out(blocker: &StabilizationBlocker) -> bool {
    matches!(
        blocker,
        StabilizationBlocker::ReviewApprovalMissing
            | StabilizationBlocker::PolicyBlocked
            | StabilizationBlocker::PolicyUnknown
            | StabilizationBlocker::ReadyForManualMerge
            | StabilizationBlocker::ReadyToAutoMerge
            | StabilizationBlocker::Merged
    )
}

fn review_gate_label(config: &crate::config::Config, session: &Session) -> String {
    let Some(summary) = &session.pr.summary else {
        return "unknown".to_string();
    };
    if has_actionable_review_feedback(session) {
        return "feedback".to_string();
    }
    if !config.auto.require_review_approval {
        return if summary.requested_reviewers.is_empty() {
            "disabled".to_string()
        } else {
            "pending".to_string()
        };
    }
    if summary.review_decision.eq_ignore_ascii_case("approved") {
        "approved".to_string()
    } else if !summary.requested_reviewers.is_empty() {
        "pending".to_string()
    } else {
        "missing".to_string()
    }
}

fn merge_gate_label(session: &Session) -> String {
    let Some(summary) = &session.pr.summary else {
        return "unknown".to_string();
    };
    if merge_blocked(summary) {
        if summary.merge_state_status.trim().is_empty() {
            "blocked".to_string()
        } else {
            format!("blocked ({})", summary.merge_state_status)
        }
    } else if summary.merge_state_status.eq_ignore_ascii_case("clean") {
        "clean".to_string()
    } else {
        "unknown".to_string()
    }
}

fn policy_gate_label(blocker: &StabilizationBlocker) -> String {
    match blocker {
        StabilizationBlocker::PolicyBlocked => "blocked".to_string(),
        StabilizationBlocker::PolicyUnknown => "unknown".to_string(),
        _ => "satisfied".to_string(),
    }
}

fn merge_blocked(summary: &crate::github::PrSummary) -> bool {
    matches!(
        summary
            .merge_state_status
            .trim()
            .to_ascii_uppercase()
            .as_str(),
        "DIRTY" | "BLOCKED" | "BEHIND"
    )
}

fn has_actionable_review_feedback(session: &Session) -> bool {
    session.pr.details.as_ref().is_some_and(|details| {
        details
            .reviews
            .iter()
            .any(|review| !review.body.trim().is_empty())
            || details
                .review_comments
                .iter()
                .any(|comment| !comment.resolved && !comment.body.trim().is_empty())
    })
}

fn stabilization_state_style(label: &str) -> Style {
    match label {
        "ReadyForManualMerge" | "ReadyToAutoMerge" | "Merged" => Style::default().fg(Color::Green),
        "CiFailed" | "MergeBlocked" | "PolicyBlocked" | "Escalate" => error_style(),
        "PendingPush" | "PolicyUnknown" | "ReviewFeedbackFound" => attention_style(),
        _ => Style::default(),
    }
}

fn gate_style(label: &str) -> Style {
    let normalized = label.to_ascii_lowercase();
    if normalized.contains("fail")
        || normalized.contains("blocked")
        || normalized.contains("missing")
    {
        error_style()
    } else if normalized.contains("pending")
        || normalized.contains("unknown")
        || normalized.contains("feedback")
    {
        attention_style()
    } else if normalized.contains("pass")
        || normalized.contains("approved")
        || normalized.contains("clean")
        || normalized.contains("satisfied")
    {
        Style::default().fg(Color::Green)
    } else {
        muted_style()
    }
}
