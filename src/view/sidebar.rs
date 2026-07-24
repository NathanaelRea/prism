use super::*;

pub(super) fn render_sidebar(
    frame: &mut Frame<'_>,
    area: Rect,
    model: &crate::view::FrameModel<'_>,
) {
    if model.tmux_portal.is_none() {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6),
                Constraint::Percentage(40),
                Constraint::Percentage(60),
            ])
            .split(area);
        render_status(frame, chunks[0], model);
        render_repos(frame, chunks[1], model);
        render_worktrees(frame, chunks[2], model);
        return;
    }

    let halves = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let upper = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(1)])
        .split(halves[0]);
    render_status(frame, upper[0], model);
    render_repos(frame, upper[1], model);
    render_worktrees(frame, halves[1], model);
}

pub(super) fn render_status(
    frame: &mut Frame<'_>,
    area: Rect,
    model: &crate::view::FrameModel<'_>,
) {
    let rows = if model.status.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "no status",
            muted_style(),
        )))]
    } else {
        let label_width = model
            .status
            .iter()
            .map(|row| row.label.chars().count())
            .max()
            .unwrap_or(0);
        model
            .status
            .iter()
            .map(|row| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<label_width$} ", row.label), muted_style()),
                    Span::styled(
                        row.value.clone(),
                        if row.attention {
                            attention_style()
                        } else {
                            Style::default()
                        },
                    ),
                ]))
            })
            .collect()
    };
    let focused = model.focus == PanelFocus::Status && !model.main_focused;
    let title = panel_title("1", "Status", focused);
    frame.render_widget(List::new(rows).block(panel_block(title, focused)), area);
}

pub(super) fn render_repos(frame: &mut Frame<'_>, area: Rect, model: &crate::view::FrameModel<'_>) {
    let rows = if model.repos.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            if model.repo_filter.is_empty() {
                "no repositories"
            } else {
                "no repository matches"
            },
            muted_style(),
        )))]
    } else {
        let label_width = model
            .repos
            .iter()
            .map(|repo| repo.label.chars().count())
            .max()
            .unwrap_or(0);
        model
            .repos
            .iter()
            .map(|repo| {
                let line = Line::from(
                    vec![
                        Span::raw(format!("{:<label_width$}", repo.label)),
                        Span::raw("  "),
                    ]
                    .into_iter()
                    .chain(repo_health_spans(&repo.health, model.config.icon_style))
                    .collect::<Vec<_>>(),
                );
                let focused = model.focus == PanelFocus::Repos && !model.main_focused;
                ListItem::new(line).style(if repo.selected {
                    selected_sidebar_row_style(focused)
                } else {
                    Style::default()
                })
            })
            .collect()
    };
    let focused = model.focus == PanelFocus::Repos && !model.main_focused;
    let mut title = panel_title("2", "Repos", focused);
    if !model.repo_filter.is_empty() {
        title.push_span(Span::styled(
            format!(" /{}", model.repo_filter),
            muted_style(),
        ));
    }
    let selected_row = model
        .repos
        .iter()
        .position(|repo| repo.selected)
        .map(|row| row as u16);
    frame.render_widget(List::new(rows).block(panel_block(title, focused)), area);
    if let Some(row) = selected_row {
        render_selected_row_outline(frame, area, row, focused);
    }
}

pub(super) fn repo_health_spans(health: &str, icon_style: IconStyle) -> Vec<Span<'static>> {
    if health == "ok" {
        return vec![Span::styled("ok".to_string(), health_style(health))];
    }

    let tokens = health.split_whitespace().collect::<Vec<_>>();
    if !tokens
        .iter()
        .all(|token| repo_health_token(token, icon_style).is_some())
    {
        return vec![Span::styled(health.to_string(), health_style(health))];
    }

    let mut spans = Vec::new();
    for token in tokens {
        let Some((kind, symbol, count)) = repo_health_token(token, icon_style) else {
            continue;
        };
        push_repo_health_cell(&mut spans, kind, symbol, count);
    }
    spans
}

fn push_repo_health_cell(
    spans: &mut Vec<Span<'static>>,
    kind: RepoHealthKind,
    symbol: &'static str,
    count: &str,
) {
    const SYMBOL_WIDTH: usize = 2;
    const COUNT_WIDTH: usize = 2;
    let cell_width = SYMBOL_WIDTH + COUNT_WIDTH + 1;
    if count == "0" {
        spans.push(Span::raw(" ".repeat(cell_width)));
        return;
    }

    let style = repo_health_style(kind);
    spans.push(Span::styled(symbol, style));
    let symbol_width = Line::from(symbol).width();
    if symbol_width < SYMBOL_WIDTH {
        spans.push(Span::raw(" ".repeat(SYMBOL_WIDTH - symbol_width)));
    }
    spans.push(Span::styled(count.to_string(), style));
    let count_width = Line::from(count).width();
    if count_width < COUNT_WIDTH {
        spans.push(Span::raw(" ".repeat(COUNT_WIDTH - count_width)));
    }
    spans.push(Span::raw(" "));
}

fn repo_health_token(
    token: &str,
    icon_style: IconStyle,
) -> Option<(RepoHealthKind, &'static str, &str)> {
    let split = token
        .char_indices()
        .rev()
        .find(|(_, ch)| !ch.is_ascii_digit())
        .map(|(index, ch)| index + ch.len_utf8())?;
    let (symbol, count) = token.split_at(split);
    if count.is_empty() || !count.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    [
        RepoHealthKind::Dirty,
        RepoHealthKind::Agents,
        RepoHealthKind::Attention,
        RepoHealthKind::PullRequests,
        RepoHealthKind::CiFailed,
        RepoHealthKind::CiRunning,
        RepoHealthKind::Behind,
    ]
    .into_iter()
    .find_map(|kind| {
        let icon = repo_health_icon(kind, icon_style);
        (symbol == icon).then_some((kind, icon, count))
    })
}

const WORKTREE_BRANCH_WIDTH: usize = 12;

pub(super) fn render_worktrees(
    frame: &mut Frame<'_>,
    area: Rect,
    model: &crate::view::FrameModel<'_>,
) {
    let rows = if model.worktrees.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            if model.worktree_filter.is_empty() {
                "no worktrees"
            } else {
                "no worktree matches"
            },
            muted_style(),
        )))]
    } else {
        let repo_mode = model.worktree_list_mode == WorktreeListMode::Repo;
        let configured_column_widths = if repo_mode {
            configured_worktree_column_widths(
                area.width.saturating_sub(2) as usize,
                &model.config.worktree_columns,
            )
        } else {
            Vec::new()
        };
        let repo_width = model
            .worktrees
            .iter()
            .map(|worktree| worktree.repo_label.chars().count())
            .max()
            .unwrap_or(4)
            .clamp(4, 10);
        let mut rows = vec![worktree_header_row(
            repo_mode,
            repo_width,
            &configured_column_widths,
        )];
        rows.extend(model.worktrees.iter().map(|worktree| {
            let mut spans = Vec::new();
            if !repo_mode {
                spans.push(Span::styled(
                    format!(
                        "{:<repo_width$} ",
                        truncate_column(&worktree.repo_label, repo_width)
                    ),
                    muted_style(),
                ));
            }
            spans.extend(worktree_base_spans(worktree, model.config.icon_style));
            if repo_mode {
                spans.extend(configured_column_widths.iter().map(|(key, width)| {
                    let value = worktree
                        .wt_columns
                        .get(*key)
                        .filter(|value| !value.is_empty())
                        .map(String::as_str)
                        .unwrap_or("·");
                    Span::styled(
                        format!("  {:<width$}", truncate_column(value, *width)),
                        muted_style(),
                    )
                }));
                if !worktree.pr.has_summary() && !worktree.prompt_summary.is_empty() {
                    spans.push(Span::styled(
                        format!("  {}", worktree.prompt_summary),
                        muted_style(),
                    ));
                }
                if let Some(status) = worktree.auto_status {
                    spans.push(Span::styled(
                        format!("  auto:{}", auto_status_label(status)),
                        auto_style(status),
                    ));
                }
            }
            let focused = model.focus == PanelFocus::Worktrees && !model.main_focused;
            ListItem::new(Line::from(spans)).style(if worktree.selected {
                selected_sidebar_row_style(focused)
            } else {
                Style::default()
            })
        }));
        rows
    };
    let focused = model.focus == PanelFocus::Worktrees && !model.main_focused;
    let mut title = panel_title("3", "Worktrees", focused);
    title.push_span(Span::raw(" "));
    for (index, (mode, label)) in [
        (WorktreeListMode::Global, "all"),
        (WorktreeListMode::Repo, "repo"),
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            title.push_span(Span::styled(" | ", muted_style()));
        }
        title.push_span(Span::styled(
            label,
            if model.worktree_list_mode == mode {
                title_style(focused)
            } else {
                muted_style()
            },
        ));
    }
    if !model.worktree_filter.is_empty() {
        title.push_span(Span::styled(
            format!(" /{}", model.worktree_filter),
            muted_style(),
        ));
    }
    let selected_row = model
        .worktrees
        .iter()
        .position(|worktree| worktree.selected)
        .map(|row| row as u16 + 1);
    frame.render_widget(List::new(rows).block(panel_block(title, focused)), area);
    if let Some(row) = selected_row {
        render_selected_row_outline(frame, area, row, focused);
    }
}

pub(super) fn worktree_header_row(
    repo_mode: bool,
    repo_width: usize,
    configured_column_widths: &[(&str, usize)],
) -> ListItem<'static> {
    if !repo_mode {
        let mut spans = vec![Span::styled(
            format!("{:<repo_width$} ", "repo"),
            muted_style(),
        )];
        spans.extend(worktree_base_header_spans());
        return ListItem::new(Line::from(spans));
    }
    ListItem::new(Line::from(
        worktree_base_header_spans()
            .into_iter()
            .chain(configured_column_widths.iter().map(|(key, width)| {
                Span::styled(
                    format!("  {:<width$}", truncate_column(key, *width)),
                    muted_style(),
                )
            }))
            .collect::<Vec<_>>(),
    ))
}

fn worktree_base_header_spans() -> Vec<Span<'static>> {
    vec![
        Span::styled("↕ ", muted_style()),
        Span::styled(
            format!("{:<width$} ", "branch", width = WORKTREE_BRANCH_WIDTH),
            muted_style(),
        ),
        Span::styled("K ", muted_style()),
        Span::styled("A ", muted_style()),
        Span::styled("P ", muted_style()),
        Span::styled("G ", muted_style()),
        Span::styled("C ", muted_style()),
        Span::styled(format!("{:<5} ", "@"), muted_style()),
        Span::styled("!", muted_style()),
    ]
}

fn worktree_base_spans(
    worktree: &crate::view::WorktreeRow,
    icon_style: IconStyle,
) -> Vec<Span<'static>> {
    let (pr_label, pr_style) = worktree_pr_column(worktree, icon_style);
    let (git_label, git_style) = worktree_git_column(worktree, icon_style);
    let (ci_label, ci_style) = worktree_ci_column(worktree, icon_style);
    let (comments_label, comments_style) = worktree_comments_column(worktree);
    let (error_label, error_style) = worktree_error_column(worktree);
    let (agent_label, agent_style) =
        if matches!(worktree.kind, crate::view::WorktreeKind::DefaultBranch) {
            (" ", muted_style())
        } else {
            (
                agent_icon(worktree.agent_state),
                agent_style(worktree.agent_state),
            )
        };
    vec![
        Span::styled(
            format!("{} ", visibility_marker(worktree.visibility)),
            visibility_style(worktree.visibility),
        ),
        Span::raw(format!(
            "{:<width$} ",
            truncate_column(&worktree.branch, WORKTREE_BRANCH_WIDTH),
            width = WORKTREE_BRANCH_WIDTH
        )),
        Span::styled(
            format!("{} ", classification_marker(worktree.classification)),
            classification_style(worktree.classification),
        ),
        Span::styled(format!("{agent_label} "), agent_style),
        Span::styled(format!("{pr_label} "), pr_style),
        Span::styled(format!("{git_label} "), git_style),
        Span::styled(format!("{ci_label} "), ci_style),
        Span::styled(format!("{comments_label:<5} "), comments_style),
        Span::styled(error_label, error_style),
    ]
}

pub(super) fn render_selected_row_outline(
    frame: &mut Frame<'_>,
    area: Rect,
    row: u16,
    focused: bool,
) {
    if area.width < 2 || area.height < 3 {
        return;
    }
    let y = area.y.saturating_add(1).saturating_add(row);
    if y >= area.y.saturating_add(area.height).saturating_sub(1) {
        return;
    }
    let style = selected_sidebar_outline_style(focused);
    frame.render_widget(Paragraph::new("█").style(style), Rect::new(area.x, y, 1, 1));
    frame.render_widget(
        Paragraph::new("█").style(style),
        Rect::new(area.x + area.width - 1, y, 1, 1),
    );
}

pub(super) fn visibility_marker(visibility: i16) -> &'static str {
    match visibility.cmp(&0) {
        std::cmp::Ordering::Greater => "↑",
        std::cmp::Ordering::Less => "↓",
        std::cmp::Ordering::Equal => "·",
    }
}

pub(super) fn visibility_style(visibility: i16) -> Style {
    match visibility.cmp(&0) {
        std::cmp::Ordering::Greater => attention_style(),
        std::cmp::Ordering::Less | std::cmp::Ordering::Equal => muted_style(),
    }
}

pub(super) fn classification_marker(
    classification: crate::session::SessionClassification,
) -> &'static str {
    match classification {
        crate::session::SessionClassification::Work => " ",
        crate::session::SessionClassification::Planning => "p",
        crate::session::SessionClassification::Exploration => "e",
    }
}

pub(super) fn classification_style(classification: crate::session::SessionClassification) -> Style {
    match classification {
        crate::session::SessionClassification::Work => muted_style(),
        crate::session::SessionClassification::Planning => Style::default().fg(Color::Cyan),
        crate::session::SessionClassification::Exploration => Style::default().fg(Color::Blue),
    }
}

pub(super) fn configured_worktree_column_widths(
    inner_width: usize,
    configured_columns: &[String],
) -> Vec<(&str, usize)> {
    if configured_columns.is_empty() {
        return Vec::new();
    }
    let base_width = 32;
    let available = inner_width.saturating_sub(base_width);
    if available < 6 {
        return Vec::new();
    }
    let separator_width = configured_columns.len() * 2;
    let value_width = available.saturating_sub(separator_width) / configured_columns.len();
    if value_width < 4 {
        return Vec::new();
    }
    configured_columns
        .iter()
        .map(|column| (column.as_str(), value_width.clamp(4, 12)))
        .collect()
}

pub(super) fn truncate_column(value: &str, width: usize) -> String {
    let mut chars = value.chars();
    let mut out = chars.by_ref().take(width).collect::<String>();
    if chars.next().is_some() && width > 0 {
        out.pop();
        out.push('~');
    }
    out
}

pub(super) fn branch_wt_label(worktree: &crate::view::WorktreeRow) -> String {
    let worktree_name = std::path::Path::new(&worktree.worktree_path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty());
    match worktree_name {
        Some(name) if name != worktree.branch => format!("{}/{}", worktree.branch, name),
        _ => worktree.branch.clone(),
    }
}

pub(super) fn worktree_pr_column(
    worktree: &crate::view::WorktreeRow,
    icon_style: IconStyle,
) -> (&'static str, Style) {
    if matches!(worktree.kind, crate::view::WorktreeKind::DefaultBranch) {
        return (" ", muted_style());
    }
    if worktree.pr.display_error().is_some() {
        return (icon(icon_style, "!", ""), error_style());
    }
    let Some(summary) = worktree.pr.summary() else {
        return ("○", muted_style());
    };
    (pr_state_icon(summary, icon_style), pr_style(summary))
}

pub(super) fn worktree_git_column(
    worktree: &crate::view::WorktreeRow,
    icon_style: IconStyle,
) -> (&'static str, Style) {
    if matches!(worktree.kind, crate::view::WorktreeKind::DefaultBranch) {
        return (" ", muted_style());
    }
    if status_count(&worktree.status_label, "dirty").is_some() {
        (
            icon(icon_style, "✗", ""),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if status_count(&worktree.status_label, "ahead").is_some()
        && status_count(&worktree.status_label, "behind").is_some()
    {
        (icon(icon_style, "↕", ""), attention_style())
    } else if status_count(&worktree.status_label, "ahead").is_some() {
        ("↑", attention_style())
    } else if status_count(&worktree.status_label, "behind").is_some() {
        ("↓", attention_style())
    } else {
        (
            icon(icon_style, "✓", ""),
            Style::default().fg(Color::Green),
        )
    }
}

pub(super) fn worktree_ci_column(
    worktree: &crate::view::WorktreeRow,
    icon_style: IconStyle,
) -> (&'static str, Style) {
    if matches!(worktree.kind, crate::view::WorktreeKind::DefaultBranch) {
        return (" ", muted_style());
    }
    let Some(summary) = worktree.pr.summary() else {
        return ("·", muted_style());
    };
    (
        ci_icon_for_status(&summary.check_status, icon_style),
        pr_check_style(&summary.check_status),
    )
}

pub(super) fn worktree_comments_column(worktree: &crate::view::WorktreeRow) -> (String, Style) {
    if matches!(worktree.kind, crate::view::WorktreeKind::DefaultBranch) {
        return (" ".to_string(), muted_style());
    }
    let label = if let Some(details) = worktree.pr.details() {
        let unresolved = details.comments.len()
            + details
                .review_comments
                .iter()
                .filter(|comment| !comment.resolved)
                .count();
        let resolved = details
            .review_comments
            .iter()
            .filter(|comment| comment.resolved)
            .count();
        if unresolved == 0 && resolved == 0 {
            "·".to_string()
        } else {
            format!("{unresolved}/{resolved}")
        }
    } else if let Some(summary) = worktree.pr.summary() {
        if summary.comment_count == 0 {
            "·".to_string()
        } else {
            format!("{}/?", summary.comment_count)
        }
    } else {
        "·".to_string()
    };
    let has_unresolved = worktree.pr.details().is_some_and(|details| {
        !details.comments.is_empty()
            || details
                .review_comments
                .iter()
                .any(|comment| !comment.resolved)
    });
    let style = if worktree.unseen_comments || has_unresolved {
        attention_style()
    } else if label == "·" {
        muted_style()
    } else {
        Style::default()
    };
    (truncate_column(&label, 5), style)
}

pub(super) fn worktree_error_column(worktree: &crate::view::WorktreeRow) -> (&'static str, Style) {
    if matches!(worktree.kind, crate::view::WorktreeKind::DefaultBranch) {
        return (" ", muted_style());
    }
    if worktree.pr.display_error().is_some() || worktree.agent_state == AgentState::ExitedError {
        ("!", error_style())
    } else if matches!(
        worktree.agent_state,
        AgentState::NeedsInput | AgentState::NeedsRestart
    ) {
        ("?", attention_style())
    } else {
        ("·", muted_style())
    }
}

pub(super) fn worktree_activity_column(worktree: &crate::view::WorktreeRow) -> (String, Style) {
    if let Some(status) = worktree.auto_status {
        return (
            format!("a:{}", auto_status_label(status)),
            auto_style(status),
        );
    }
    if let Some(status) = worktree.plan_status {
        return (
            format!("p:{}", plan_run_status_label(status)),
            plan_run_status_style(status),
        );
    }
    ("-".to_string(), muted_style())
}
