use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChangeRequestPanelModel {
    pub icon_style: IconStyle,
    pub provider_noun: &'static str,
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
    if let Some(environment) = model
        .worktrees
        .iter()
        .find(|row| row.session_index == index)
        .and_then(|row| row.development.as_ref())
    {
        let reachability = match environment.listening {
            Some(true) => "listening",
            Some(false) => "not listening",
            None => "unknown",
        };
        let quality = match environment.quality {
            DevelopmentEnvironmentQuality::NeverLoaded => "unknown",
            DevelopmentEnvironmentQuality::Refreshing => "refreshing",
            DevelopmentEnvironmentQuality::Fresh => reachability,
            DevelopmentEnvironmentQuality::Stale => "stale",
        };
        lines.push(Line::from(""));
        lines.push(heading_line("Development"));
        lines.push(labelled_line("url", environment.url.clone()));
        lines.push(labelled_line("status", quality.to_string()));
    }
    if !session.prompt_summary.trim().is_empty() {
        lines.push(Line::from(""));
        lines.push(labelled_line("prompt", session.prompt_summary.clone()));
    }
    lines.push(Line::from(""));
    lines.extend(agent_lines(session));
    lines.push(Line::from(""));
    lines.extend(change_request_panel_lines(&change_request_panel_model(
        model, session,
    )));
    if let Some(details) = session.pr.details() {
        lines.extend(pr_comment_lines(details, 5, model.selected_comment));
    }
    lines
}

fn agent_lines(session: &Session) -> Vec<Line<'static>> {
    let (state, icon, label, tool) = session
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
            )
        })
        .unwrap_or((
            session.agent_state,
            agent_icon(session.agent_state),
            session.agent_state.label(),
            None,
        ));
    let status = match tool.filter(|tool| !tool.trim().is_empty()) {
        Some(tool) => format!("{label}  {tool}"),
        None => label.to_string(),
    };
    vec![
        heading_line("Agent"),
        Line::from(vec![
            Span::styled("status ", muted_style()),
            Span::styled(icon, agent_style(state)),
            Span::raw(format!(" {status}")),
        ]),
    ]
}

pub(crate) fn change_request_panel_model(
    model: &crate::view::FrameModel<'_>,
    session: &Session,
) -> ChangeRequestPanelModel {
    let summary = session.pr.summary();

    if summary.is_none() {
        return ChangeRequestPanelModel {
            icon_style: model.config.icon_style,
            provider_noun: "CR",
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

    ChangeRequestPanelModel {
        icon_style: model.config.icon_style,
        provider_noun: summary
            .map(|summary| summary.provider_noun())
            .unwrap_or("CR"),
        pr_number: summary
            .map(|summary| summary.number.to_string())
            .unwrap_or_default(),
        pr_merged: summary.is_some_and(|summary| summary.merged),
        pr_name: summary
            .map(|summary| summary.title.clone())
            .unwrap_or_default(),
        blocker: summary
            .map(|summary| summary.state.clone())
            .unwrap_or_default(),
        next: String::new(),
        ci: summary
            .map(|summary| summary.check_state().label().to_string())
            .unwrap_or_default(),
        review: review_gate_label(session),
        merge: merge_gate_label(session),
        policy: String::new(),
    }
}

pub(crate) fn change_request_panel_lines(model: &ChangeRequestPanelModel) -> Vec<Line<'static>> {
    if model.pr_number.is_empty() {
        return Vec::new();
    }

    let mut lines = vec![
        heading_line(model.provider_noun),
        pr_number_line(model),
        change_request_value_line("name", &model.pr_name, selected_text_style()),
    ];
    lines.extend([
        change_request_value_line(
            "state",
            &model.blocker,
            change_request_state_style(&model.blocker),
        ),
        change_request_value_line("next", &model.next, attention_style()),
    ]);
    lines.push(change_request_gate_line("ci", &model.ci, model.icon_style));
    lines.push(change_request_gate_line(
        "review",
        &model.review,
        model.icon_style,
    ));
    lines.push(change_request_gate_line(
        "merge",
        &model.merge,
        model.icon_style,
    ));
    lines.push(change_request_gate_line(
        "policy",
        &model.policy,
        model.icon_style,
    ));
    lines
}

fn pr_number_line(model: &ChangeRequestPanelModel) -> Line<'static> {
    let Some(number) = model
        .pr_number
        .parse::<u64>()
        .ok()
        .filter(|_| !model.pr_number.is_empty())
    else {
        return change_request_value_line("pr #", "", Style::default());
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

fn change_request_value_line(label: &'static str, value: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{:<16}", label), muted_style()),
        Span::styled(truncate(value, 30), style),
    ])
}

fn change_request_gate_line(
    gate: &'static str,
    status: &str,
    icon_style: IconStyle,
) -> Line<'static> {
    let style = gate_style(status);
    let status_icon = if status.is_empty() {
        ""
    } else {
        change_request_status_icon(status, icon_style)
    };
    Line::from(vec![
        Span::styled(format!("{:<16}", gate), muted_style()),
        Span::styled(status_icon, style),
        Span::raw(" "),
        Span::styled(truncate(status, 30), style),
    ])
}

fn change_request_status_icon(status: &str, icon_style: IconStyle) -> &'static str {
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

fn review_gate_label(session: &Session) -> String {
    if session.pr.summary().is_none() {
        return "unknown".to_string();
    }
    if has_unresolved_review_comments(session) {
        return "needs review".to_string();
    }
    "passed".to_string()
}

fn merge_gate_label(session: &Session) -> String {
    let Some(summary) = session.pr.summary() else {
        return String::new();
    };
    if !matches!(
        summary.queue_state.trim().to_ascii_lowercase().as_str(),
        "" | "not_queued" | "none"
    ) {
        format!("queue ({})", summary.queue_state)
    } else if merge_blocked(summary) {
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

fn merge_blocked(summary: &crate::remote::PrSummary) -> bool {
    matches!(
        summary
            .merge_state_status
            .trim()
            .to_ascii_uppercase()
            .as_str(),
        "DIRTY" | "BLOCKED" | "BEHIND"
    )
}

fn has_unresolved_review_comments(session: &Session) -> bool {
    session.pr.details().is_some_and(|details| {
        details
            .review_comments
            .iter()
            .any(|comment| !comment.resolved)
    })
}

fn change_request_state_style(label: &str) -> Style {
    match label {
        "Merged" => Style::default().fg(Color::Green),
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
        || normalized.contains("needs review")
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
