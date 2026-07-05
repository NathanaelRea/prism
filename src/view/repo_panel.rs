use super::*;

pub(super) fn repo_overview_lines(
    model: &crate::view::FrameModel<'_>,
    width: usize,
    visible_rows: usize,
) -> Vec<Line<'static>> {
    let indices = model
        .sessions
        .iter()
        .enumerate()
        .filter_map(|(index, session)| {
            (session.repo_index == model.current_repo_index).then_some(index)
        })
        .collect::<Vec<_>>();
    let summary = repo_github_summary(model.config, model.sessions, &indices);
    let mut lines = vec![
        Line::from(Span::styled(
            model.selected_repo_label.clone(),
            title_style(true),
        )),
        Line::from(Span::styled(
            model.selected_repo_root.clone(),
            muted_style(),
        )),
    ];
    if let Some(row) = model.repos.iter().find(|row| row.selected) {
        lines.push(labelled_line("health", row.health.clone()));
    }
    lines.push(Line::from(vec![
        Span::styled("view ", muted_style()),
        Span::raw(model.repo_main_view.label().to_string()),
        Span::styled("  prs ", muted_style()),
        Span::raw(summary.open_prs.to_string()),
        Span::styled("  review needed ", muted_style()),
        Span::raw(summary.review_needed.to_string()),
        Span::styled("  ci failed ", muted_style()),
        Span::raw(summary.ci_failed.to_string()),
        Span::styled("  local ", muted_style()),
        Span::raw(summary.local_branches.to_string()),
    ]));
    lines.push(Line::from(""));
    let remaining_rows = visible_rows.saturating_sub(lines.len());
    match model.repo_main_view {
        crate::view::RepoMainView::Github => lines.extend(repo_github_panel_lines(
            model.config,
            model.sessions,
            &indices,
            model.selected_session,
            width,
            remaining_rows,
        )),
        crate::view::RepoMainView::Kanban => lines.extend(kanban_panel_lines(
            model.config,
            model.sessions,
            &indices,
            model.selected_session,
            width,
            remaining_rows,
        )),
    }
    lines
}

#[derive(Default)]
pub(super) struct RepoGithubSummary {
    open_prs: usize,
    review_needed: usize,
    ci_failed: usize,
    local_branches: usize,
}

pub(super) fn repo_github_summary(
    config: &crate::config::Config,
    sessions: &[Session],
    session_indices: &[usize],
) -> RepoGithubSummary {
    let mut summary = RepoGithubSummary::default();
    for index in session_indices {
        let Some(session) = sessions.get(*index) else {
            continue;
        };
        if session.is_default_branch(config) {
            continue;
        }
        match &session.pr.summary {
            Some(pr) => {
                if !pr.merged && pr.state == "OPEN" {
                    summary.open_prs += 1;
                }
                if review_decision_for_display(pr, session.pr.details.as_ref()) == "REVIEW_REQUIRED"
                {
                    summary.review_needed += 1;
                }
                if pr.check_status == "failed" {
                    summary.ci_failed += 1;
                }
            }
            None => summary.local_branches += 1,
        }
    }
    summary
}

pub(super) fn repo_github_panel_lines(
    config: &crate::config::Config,
    sessions: &[Session],
    session_indices: &[usize],
    selected: Option<usize>,
    width: usize,
    visible_rows: usize,
) -> Vec<Line<'static>> {
    let mut lines = repo_work_list_lines(config, sessions, session_indices, selected, visible_rows);
    if lines.len() < visible_rows {
        lines.push(Line::from(""));
    }
    let preview = selected.and_then(|index| sessions.get(index));
    lines.extend(repo_preview_lines(config, preview, width));
    lines.truncate(visible_rows);
    lines
}

pub(super) fn repo_work_list_lines(
    config: &crate::config::Config,
    sessions: &[Session],
    session_indices: &[usize],
    selected: Option<usize>,
    visible_rows: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![heading_line("PRs / Work")];
    if session_indices.is_empty() {
        lines.push(Line::from(Span::styled(
            "No worktrees discovered",
            muted_style(),
        )));
        lines.push(Line::from(Span::styled(
            "Create one with c",
            attention_style(),
        )));
        lines.truncate(visible_rows);
        return lines;
    }
    for index in session_indices {
        if lines.len() >= visible_rows {
            break;
        }
        let Some(session) = sessions.get(*index) else {
            continue;
        };
        lines.push(repo_work_item_line(
            config,
            session,
            Some(*index) == selected,
        ));
    }
    lines
}

pub(super) fn repo_work_item_line(
    config: &crate::config::Config,
    session: &Session,
    selected: bool,
) -> Line<'static> {
    let marker = if selected { "▶" } else { " " };
    let kind = repo_work_kind_label(config, session);
    let label = session
        .pr
        .summary
        .as_ref()
        .map(|summary| format!("{} - {}", session.branch, summary.title))
        .unwrap_or_else(|| session.branch.clone());
    Line::from(vec![
        Span::styled(
            marker,
            if selected {
                title_style(true)
            } else {
                muted_style()
            },
        ),
        Span::raw(" "),
        Span::styled(format!("{kind:<8}"), muted_style()),
        Span::styled(
            label,
            if selected {
                selected_text_style()
            } else {
                Style::default()
            },
        ),
        Span::styled(
            format!("  {}", repo_work_detail_label(config, session)),
            muted_style(),
        ),
    ])
}

pub(super) fn repo_preview_lines(
    config: &crate::config::Config,
    session: Option<&Session>,
    _width: usize,
) -> Vec<Line<'static>> {
    let Some(session) = session else {
        return vec![
            heading_line("Preview"),
            Line::from(Span::styled("No selected worktree", muted_style())),
            Line::from(Span::styled("Enter focuses worktrees", attention_style())),
        ];
    };
    let mut lines = vec![heading_line("Preview")];
    if session.is_default_branch(config) {
        lines.push(Line::from(Span::styled(
            "Default branch",
            selected_text_style(),
        )));
        lines.push(labelled_line("branch", session.branch.clone()));
        lines.push(labelled_line("status", session.status_label.clone()));
        lines.push(Line::from(Span::styled(
            "PR tracking disabled",
            muted_style(),
        )));
        return lines;
    }
    if let Some(error) = &session.pr.error {
        lines.push(Line::from(Span::styled(
            "✕ PR refresh error",
            error_style(),
        )));
        lines.push(Line::from(error.clone()));
        return lines;
    }
    let Some(summary) = &session.pr.summary else {
        lines.push(Line::from(Span::styled("○ No PR detected", muted_style())));
        lines.push(labelled_line("branch", session.branch.clone()));
        lines.push(labelled_line("status", session.status_label.clone()));
        lines.push(Line::from(Span::styled(
            "Space g P creates one",
            attention_style(),
        )));
        return lines;
    };
    let review = review_decision_for_display(summary, session.pr.details.as_ref());
    lines.push(Line::from(vec![
        Span::styled(
            pr_state_icon(summary, config.icon_style),
            pr_state_style(summary),
        ),
        Span::styled(
            format!(" PR #{} {}", summary.number, pr_state_label(summary)),
            pr_state_style(summary),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        summary.title.clone(),
        selected_text_style(),
    )));
    lines.push(Line::from(vec![
        Span::styled("review ", muted_style()),
        Span::styled(review_label(&review).to_string(), review_style(&review)),
        Span::styled("  ci ", muted_style()),
        Span::styled(summary.check_status.clone(), ci_style(config, session)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("base ", muted_style()),
        Span::raw(summary.base_ref.clone()),
        Span::styled("  head ", muted_style()),
        Span::raw(summary.head_ref.clone()),
    ]));
    if !summary.requested_reviewers.is_empty() {
        lines.push(labelled_line(
            "awaiting",
            summary.requested_reviewers.join(", "),
        ));
    }
    if let Some(details) = &session.pr.details {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("comments ", muted_style()),
            Span::raw((details.comments.len() + details.review_comments.len()).to_string()),
            Span::styled("  reviews ", muted_style()),
            Span::raw(details.reviews.len().to_string()),
            Span::styled("  files ", muted_style()),
            Span::raw(details.files.len().to_string()),
        ]));
        lines.extend(pr_comment_lines(details, 3));
        if !details.failing_checks.is_empty() {
            lines.push(Line::from(Span::styled("Failing checks", error_style())));
            for check in details.failing_checks.iter().take(2) {
                lines.push(Line::from(vec![
                    Span::styled("✕ ", error_style()),
                    Span::raw(check.clone()),
                ]));
            }
        }
        if !details.ci_failures.is_empty() {
            lines.push(labelled_line(
                "CI failures cached",
                details.ci_failures.len().to_string(),
            ));
        }
    } else {
        lines.push(Line::from(Span::styled("Activity pending", muted_style())));
    }
    lines
}

#[derive(Clone, Copy)]
pub(super) enum KanbanLane {
    Plan,
    Impl,
    PrCi,
    Merged,
}

impl KanbanLane {
    pub(super) fn index(self) -> usize {
        match self {
            Self::Plan => 0,
            Self::Impl => 1,
            Self::PrCi => 2,
            Self::Merged => 3,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Impl => "impl",
            Self::PrCi => "pr/ci",
            Self::Merged => "merged",
        }
    }

    pub(super) fn style(self) -> Style {
        match self {
            Self::Plan => muted_style(),
            Self::Impl => attention_style(),
            Self::PrCi => Style::default().fg(Color::Green),
            Self::Merged => Style::default().fg(Color::Magenta),
        }
    }
}

pub(super) const KANBAN_LANES: [KanbanLane; 4] = [
    KanbanLane::Plan,
    KanbanLane::Impl,
    KanbanLane::PrCi,
    KanbanLane::Merged,
];

pub(super) fn kanban_panel_lines(
    config: &crate::config::Config,
    sessions: &[Session],
    session_indices: &[usize],
    selected: Option<usize>,
    width: usize,
    visible_rows: usize,
) -> Vec<Line<'static>> {
    if width < 32 {
        return vec![Line::from(Span::styled(
            "Kanban needs more width",
            muted_style(),
        ))];
    }
    let mut lanes: [Vec<(usize, &Session)>; 4] = std::array::from_fn(|_| Vec::new());
    for index in session_indices {
        let Some(session) = sessions.get(*index) else {
            continue;
        };
        if let Some(lane) = kanban_lane(config, session) {
            lanes[lane.index()].push((*index, session));
        }
    }
    if lanes.iter().all(Vec::is_empty) {
        return vec![
            Line::from(Span::styled("No feature worktrees", muted_style())),
            Line::from(Span::styled("Create one with c", attention_style())),
        ];
    }
    let mut lines = vec![Line::from(
        KANBAN_LANES
            .iter()
            .enumerate()
            .flat_map(|(index, lane)| {
                vec![
                    Span::styled(
                        format!("{} {}", lane.label(), lanes[index].len()),
                        lane.style(),
                    ),
                    Span::raw("   "),
                ]
            })
            .collect::<Vec<_>>(),
    )];
    let max_lane_rows = lanes.iter().map(Vec::len).max().unwrap_or(0);
    let shown_rows = max_lane_rows.min(visible_rows.saturating_sub(lines.len()));
    for row in 0..shown_rows {
        lines.push(Line::from(
            lanes
                .iter()
                .flat_map(|lane_sessions| {
                    if let Some((index, session)) = lane_sessions.get(row) {
                        kanban_card_spans(config, session, Some(*index) == selected)
                    } else {
                        vec![Span::raw("   ")]
                    }
                })
                .collect::<Vec<_>>(),
        ));
    }
    lines
}

pub(super) fn kanban_card_spans(
    config: &crate::config::Config,
    session: &Session,
    selected: bool,
) -> Vec<Span<'static>> {
    let mut suffix = git_status_indicator(&session.status_label, config.icon_style);
    if let Some(summary) = &session.pr.summary {
        if !suffix.is_empty() {
            suffix.push(' ');
        }
        suffix.push_str(&format!(
            "#{} {}",
            summary.number,
            ci_icon(config, session, config.icon_style)
        ));
    }
    vec![
        Span::styled(if selected { "▶ " } else { "  " }, title_style(selected)),
        Span::styled(
            session.branch.clone(),
            if selected {
                selected_text_style()
            } else {
                Style::default()
            },
        ),
        Span::styled(format!(" {suffix}   "), muted_style()),
    ]
}

pub(super) fn kanban_lane(config: &crate::config::Config, session: &Session) -> Option<KanbanLane> {
    if session.is_default_branch(config) {
        return None;
    }
    if session
        .pr
        .summary
        .as_ref()
        .is_some_and(|summary| summary.merged)
    {
        return Some(KanbanLane::Merged);
    }
    if session.pr.summary.is_some() {
        return Some(KanbanLane::PrCi);
    }
    if status_count(&session.status_label, "dirty").is_some()
        || status_count(&session.status_label, "ahead").is_some()
        || matches!(
            session.agent_state,
            AgentState::Running
                | AgentState::ExitedError
                | AgentState::NeedsRestart
                | AgentState::NeedsInput
        )
    {
        return Some(KanbanLane::Impl);
    }
    Some(KanbanLane::Plan)
}

pub(super) fn repo_work_kind_label(config: &crate::config::Config, session: &Session) -> String {
    if session.is_default_branch(config) {
        "default".to_string()
    } else if let Some(summary) = &session.pr.summary {
        format!("#{}", summary.number)
    } else {
        "local".to_string()
    }
}

pub(super) fn repo_work_detail_label(config: &crate::config::Config, session: &Session) -> String {
    let mut parts = Vec::new();
    if session.is_default_branch(config) {
        parts.push("tracking off".to_string());
    } else if let Some(summary) = &session.pr.summary {
        parts.push(pr_state_label(summary).to_string());
        parts.push(
            review_label(&review_decision_for_display(
                summary,
                session.pr.details.as_ref(),
            ))
            .to_string(),
        );
        parts.push(format!(
            "ci {} {}",
            ci_icon(config, session, config.icon_style),
            summary.check_status
        ));
        parts.push(pr_comment_count_label(&session.pr));
    } else {
        parts.push("no PR".to_string());
    }
    let git = git_status_indicator(&session.status_label, config.icon_style);
    if !git.is_empty() {
        parts.push(git);
    }
    if matches!(
        session.agent_state,
        AgentState::Running | AgentState::NeedsInput | AgentState::NeedsRestart
    ) {
        parts.push(format!("agent {}", agent_icon(session.agent_state)));
    }
    parts.join("  ")
}
