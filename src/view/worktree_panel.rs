use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StabilizationPanelModel {
    pub icon_style: IconStyle,
    pub pr_number: String,
    pub pr_merged: bool,
    pub pr_name: String,
    pub blocker: String,
    pub next: String,
    pub ci: String,
    pub review: String,
    pub merge: String,
    pub policy: String,
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
    lines.push(Line::from(""));
    lines.extend(agent_lines(session));
    lines.push(Line::from(""));
    lines.extend(stabilization_panel_lines(&stabilization_panel_model(
        model, session,
    )));
    if let Some(details) = &session.pr.details {
        lines.extend(pr_comment_lines(details, 5, model.selected_comment));
    }
    lines
}

fn agent_lines(session: &Session) -> Vec<Line<'static>> {
    let (state, icon, label, tool, user_message, messages) = session
        .opencode_status
        .as_ref()
        .map(|status| {
            let tool = status.active_tool.as_deref();
            let has_active_tool = tool.is_some_and(|tool| !tool.trim().is_empty());
            let state = if matches!(status.state, OpencodeState::Starting | OpencodeState::Busy)
                || has_active_tool
            {
                AgentState::Running
            } else {
                status.state.agent_state()
            };
            let icon = if matches!(status.state, OpencodeState::Unknown | OpencodeState::Idle)
                && state == AgentState::Running
            {
                agent_icon(state)
            } else {
                opencode_icon(status.state)
            };
            (
                state,
                icon,
                match status.state {
                    OpencodeState::Starting => "starting",
                    OpencodeState::Busy => "busy",
                    OpencodeState::Retry => "retrying",
                    OpencodeState::Idle if state == AgentState::Running => "running",
                    OpencodeState::Idle => "ready",
                    OpencodeState::Done => "done",
                    OpencodeState::NeedsInput => "needs input",
                    OpencodeState::Error => "failed",
                    OpencodeState::Unknown if state == AgentState::Running => "running",
                    OpencodeState::Unknown | OpencodeState::Offline => "needs restart",
                },
                tool.or(status.detail.as_deref()),
                status.latest_user_message.as_deref(),
                status.recent_messages.as_slice(),
            )
        })
        .unwrap_or((
            session.agent_state,
            agent_icon(session.agent_state),
            session.agent_state.label(),
            None,
            None,
            &[],
        ));
    let status = match tool.filter(|tool| !tool.trim().is_empty()) {
        Some(tool) => format!("{label}  {tool}"),
        None => label.to_string(),
    };
    let mut lines = vec![
        heading_line("Agent"),
        Line::from(vec![
            Span::styled("status ", muted_style()),
            Span::styled(icon, agent_style(state)),
            Span::raw(format!(" {status}")),
        ]),
        Line::from(vec![
            Span::styled("user ", muted_style()),
            Span::styled(
                truncate(user_message.unwrap_or_default(), 74),
                Style::default().fg(Color::White),
            ),
        ]),
    ];
    for index in 0..5 {
        lines.push(Line::from(
            messages
                .get(index)
                .map(|message| truncate(message, 86))
                .unwrap_or_default(),
        ));
    }
    lines
}

pub(crate) fn stabilization_panel_model(
    model: &crate::view::FrameModel<'_>,
    session: &Session,
) -> StabilizationPanelModel {
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
        .or_else(|| {
            run.and_then(|run| run.stabilization_status)
                .is_none()
                .then(|| cached_pr_blocker(model.config, session))
                .flatten()
        });
    let next = blocker.as_ref().map(|blocker| {
        run.and_then(|run| run.stabilization_next_work.as_ref())
            .cloned()
            .unwrap_or_else(|| cached_next_work(blocker))
    });
    let summary = session.pr.summary.as_ref();

    if summary.is_none() {
        return StabilizationPanelModel {
            icon_style: model.config.icon_style,
            pr_number: String::new(),
            pr_merged: false,
            pr_name: String::new(),
            blocker: String::new(),
            next: String::new(),
            ci: String::new(),
            review: String::new(),
            merge: String::new(),
            policy: String::new(),
        };
    }

    StabilizationPanelModel {
        icon_style: model.config.icon_style,
        pr_number: summary
            .map(|summary| summary.number.to_string())
            .unwrap_or_default(),
        pr_merged: summary.is_some_and(|summary| summary.merged),
        pr_name: summary
            .map(|summary| summary.title.clone())
            .unwrap_or_default(),
        blocker: blocker
            .as_ref()
            .map(blocker_label)
            .or_else(|| {
                run.and_then(|run| run.stabilization_status)
                    .map(|status| pascal_label(status.as_str()))
            })
            .unwrap_or_default(),
        next: next.as_ref().map(work_label).unwrap_or_default(),
        ci: blocker
            .as_ref()
            .map(|blocker| ci_gate_label(session, blocker))
            .unwrap_or_default(),
        review: review_gate_label(model.config, session),
        merge: merge_gate_label(session),
        policy: blocker.as_ref().map(policy_gate_label).unwrap_or_default(),
    }
}

pub(crate) fn stabilization_panel_lines(model: &StabilizationPanelModel) -> Vec<Line<'static>> {
    let mut lines = vec![
        heading_line("PR"),
        pr_number_line(model),
        stabilization_value_line("name", &model.pr_name, selected_text_style()),
    ];
    if model.pr_number.is_empty() {
        return lines;
    }
    lines.extend([
        stabilization_value_line(
            "state",
            &model.blocker,
            stabilization_state_style(&model.blocker),
        ),
        stabilization_value_line("next", &model.next, attention_style()),
    ]);
    lines.push(stabilization_gate_line("ci", &model.ci, model.icon_style));
    lines.push(stabilization_gate_line(
        "review",
        &model.review,
        model.icon_style,
    ));
    lines.push(stabilization_gate_line(
        "merge",
        &model.merge,
        model.icon_style,
    ));
    lines.push(stabilization_gate_line(
        "policy",
        &model.policy,
        model.icon_style,
    ));
    lines
}

fn pr_number_line(model: &StabilizationPanelModel) -> Line<'static> {
    let Some(number) = model
        .pr_number
        .parse::<u64>()
        .ok()
        .filter(|_| !model.pr_number.is_empty())
    else {
        return stabilization_value_line("pr #", "", Style::default());
    };
    let style = Style::default()
        .fg(if model.pr_merged {
            Color::Magenta
        } else {
            Color::Green
        })
        .add_modifier(Modifier::BOLD);
    let symbol = if model.pr_merged {
        icon(model.icon_style, "⋈", "")
    } else {
        icon(model.icon_style, "⇄", "")
    };
    Line::from(vec![
        Span::styled(format!("{:<16}", "pr #"), muted_style()),
        Span::styled(format!("{symbol} #{number}"), style),
    ])
}

fn stabilization_value_line(label: &'static str, value: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{:<16}", label), muted_style()),
        Span::styled(truncate(value, 30), style),
    ])
}

fn stabilization_gate_line(
    gate: &'static str,
    status: &str,
    icon_style: IconStyle,
) -> Line<'static> {
    let style = gate_style(status);
    let status_icon = if status.is_empty() {
        ""
    } else {
        stabilization_status_icon(status, icon_style)
    };
    Line::from(vec![
        Span::styled(format!("{:<16}", gate), muted_style()),
        Span::styled(status_icon, style),
        Span::raw(" "),
        Span::styled(truncate(status, 30), style),
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
        return String::new();
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
