use super::*;

pub(super) fn pr_panel_lines(
    config: &crate::config::Config,
    session: Option<&Session>,
    _selected_comment: usize,
) -> Vec<Line<'static>> {
    let Some(session) = session else {
        return vec![Line::from(Span::styled(
            "No selected worktree",
            muted_style(),
        ))];
    };
    if session.is_default_branch(config) {
        return vec![
            heading_line("Default branch"),
            labelled_line("branch", session.branch.clone()),
            Line::from(Span::styled("PR tracking disabled", muted_style())),
        ];
    }
    if let Some(error) = session.pr.display_error() {
        return vec![
            Line::from(Span::styled("✕ PR refresh error", error_style())),
            Line::from(error.to_string()),
            Line::from(Span::styled("Press r to retry", attention_style())),
        ];
    }
    let Some(summary) = session.pr.summary() else {
        let refreshed = session
            .pr
            .last_refreshed
            .as_deref()
            .unwrap_or("not refreshed");
        return vec![
            Line::from(Span::styled("○ No PR detected", muted_style())),
            labelled_line("branch", session.branch.clone()),
            labelled_line("last", refreshed.to_string()),
            Line::from(Span::styled("P creates one explicitly", attention_style())),
        ];
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                pr_state_icon(summary, config.icon_style),
                pr_state_style(summary),
            ),
            Span::styled(
                format!(" PR #{} {}", summary.number, pr_state_label(summary)),
                pr_state_style(summary),
            ),
        ]),
        Line::from(Span::styled(summary.title.clone(), selected_text_style())),
    ];
    if !summary.requested_reviewers.is_empty() {
        lines.push(labelled_line(
            "awaiting",
            summary.requested_reviewers.join(", "),
        ));
    }
    lines
}

pub(super) fn pr_comment_lines(
    details: &crate::github::PrDetails,
    max_comments: usize,
    selected: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(""), heading_line("Comments")];
    lines.push(Line::from(vec![Span::styled(
        "  kind   res author       text",
        muted_style(),
    )]));
    let rows = pr_comment_rows(details);
    let shown = rows.len().min(max_comments);
    for (index, row) in rows.iter().take(max_comments).enumerate() {
        lines.push(pr_comment_row_line(row, index == selected));
    }
    if shown == 0 {
        lines.push(Line::from(Span::styled("No comments", muted_style())));
    }
    let total = rows.len();
    if total > shown {
        lines.push(Line::from(Span::styled(
            format!("+{} more", total - shown),
            muted_style(),
        )));
    }
    lines
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PrCommentDisplayRow {
    pub kind: String,
    pub author: String,
    pub context: String,
    pub body: String,
    pub resolved: String,
}

pub(crate) fn pr_comment_rows(details: &crate::github::PrDetails) -> Vec<PrCommentDisplayRow> {
    let mut rows = Vec::new();
    for comment in details.comments.iter().rev() {
        rows.push(PrCommentDisplayRow {
            kind: "root".to_string(),
            author: display_author(&comment.author),
            context: String::new(),
            body: one_line_comment(&comment.body),
            resolved: ".".to_string(),
        });
    }
    for review in details
        .reviews
        .iter()
        .rev()
        .filter(|review| !review.body.trim().is_empty())
    {
        rows.push(PrCommentDisplayRow {
            kind: "review".to_string(),
            author: display_author(&review.author),
            context: review_label(&review.state).to_string(),
            body: one_line_comment(&review.body),
            resolved: ".".to_string(),
        });
    }
    for comment in details.review_comments.iter().rev() {
        let context = if comment.line.is_empty() {
            comment.path.clone()
        } else {
            format!("{}:{}", comment.path, comment.line)
        };
        rows.push(PrCommentDisplayRow {
            kind: "inline".to_string(),
            author: display_author(&comment.author),
            context,
            body: one_line_comment(&comment.body),
            resolved: if comment.resolved { "yes" } else { "no" }.to_string(),
        });
    }
    rows
}

pub(super) fn pr_comment_row_line(row: &PrCommentDisplayRow, selected: bool) -> Line<'static> {
    let marker = if selected { ">" } else { " " };
    Line::from(vec![
        Span::styled(format!("{marker} "), title_style(selected)),
        Span::styled(format!("{:<6} ", truncate(&row.kind, 6)), muted_style()),
        Span::styled(
            format!("{:<3} ", truncate(&row.resolved, 3)),
            resolved_style(&row.resolved),
        ),
        Span::styled(format!("{:<12} ", truncate(&row.author, 12)), muted_style()),
        Span::styled(
            truncate(&row.body, 50),
            if selected {
                selected_text_style()
            } else {
                Style::default()
            },
        ),
    ])
}

fn display_author(author: &str) -> String {
    if author.trim().is_empty() {
        "unknown".to_string()
    } else {
        author.trim().to_string()
    }
}

fn one_line_comment(body: &str) -> String {
    let text = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        "empty comment".to_string()
    } else {
        text
    }
}

fn resolved_style(resolved: &str) -> Style {
    if resolved.eq_ignore_ascii_case("no") {
        attention_style()
    } else {
        muted_style()
    }
}

pub(super) fn append_comment(
    lines: &mut Vec<Line<'static>>,
    author: &str,
    context: &str,
    body: &str,
) {
    let author = if author.trim().is_empty() {
        "unknown"
    } else {
        author.trim()
    };
    let context = context.trim();
    if context.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("@ ", muted_style()),
            Span::raw(author.to_string()),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("@ ", muted_style()),
            Span::raw(author.to_string()),
            Span::styled(format!(" {context}"), muted_style()),
        ]));
    }
    let mut body_lines = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(2)
        .peekable();
    if body_lines.peek().is_none() {
        lines.push(Line::from(Span::styled("  empty comment", muted_style())));
        return;
    }
    for line in body_lines {
        lines.push(Line::from(format!("  {line}")));
    }
}

pub(super) fn description_lines(body: &str, max_lines: usize) -> Vec<Line<'static>> {
    let lines = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(max_lines)
        .map(|line| Line::from(line.to_string()))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        vec![Line::from(Span::styled("No description", muted_style()))]
    } else {
        lines
    }
}

pub(super) fn pr_comment_count_label(cache: &crate::github::PrCache) -> String {
    if let Some(details) = cache.details() {
        let open = details.comments.len()
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
        return format!("#{open}✓{resolved}");
    }
    cache
        .summary
        .as_ref()
        .map(|summary| format!("#{}", summary.comment_count))
        .unwrap_or_else(|| "#?".to_string())
}

pub(super) fn review_decision_for_display(
    summary: &crate::github::PrSummary,
    details: Option<&crate::github::PrDetails>,
) -> String {
    if !matches!(summary.review_decision.as_str(), "" | "UNKNOWN") {
        return summary.review_decision.clone();
    }
    if !summary.requested_reviewers.is_empty() {
        return "REVIEW_REQUIRED".to_string();
    }
    details
        .and_then(|details| {
            details
                .reviews
                .iter()
                .rev()
                .find(|review| !review.state.trim().is_empty())
        })
        .map(|review| review.state.clone())
        .or_else(|| {
            details
                .is_some_and(|details| !details.review_comments.is_empty())
                .then(|| "COMMENTED".to_string())
        })
        .unwrap_or_else(|| summary.review_decision.clone())
}

pub(super) fn pr_state_label(summary: &crate::github::PrSummary) -> &'static str {
    if pr_has_merge_conflict(summary) {
        "conflict"
    } else if summary.merged {
        "merged"
    } else if summary.draft {
        "draft"
    } else if summary.state == "OPEN" {
        "open"
    } else {
        "closed"
    }
}

pub(super) fn review_label(decision: &str) -> &str {
    match decision {
        "APPROVED" => "approved",
        "CHANGES_REQUESTED" => "changes",
        "REVIEW_REQUIRED" => "needed",
        "COMMENTED" => "commented",
        "" | "UNKNOWN" => "unknown",
        _ => decision,
    }
}

pub(super) fn pr_state_icon(
    summary: &crate::github::PrSummary,
    icon_style: IconStyle,
) -> &'static str {
    if pr_has_merge_conflict(summary) {
        return icon(icon_style, "⚔", "");
    }
    if icon_style == IconStyle::NerdFont {
        return if summary.merged {
            ""
        } else if summary.draft {
            ""
        } else if summary.state == "OPEN" {
            ""
        } else {
            ""
        };
    }
    if summary.merged {
        "⋈"
    } else if summary.draft {
        "◐"
    } else if summary.state == "OPEN" {
        "⇄"
    } else {
        "×"
    }
}

pub(super) fn pr_has_merge_conflict(summary: &crate::github::PrSummary) -> bool {
    summary.merge_state_status.eq_ignore_ascii_case("DIRTY")
}

pub(super) fn ci_icon(
    config: &crate::config::Config,
    session: &Session,
    icon_style: IconStyle,
) -> &'static str {
    if session.is_default_branch(config) {
        return "";
    }
    match session
        .pr
        .summary
        .as_ref()
        .map(|summary| summary.check_status.as_str())
    {
        Some("passed") => icon(icon_style, "✓", ""),
        Some("failed") => icon(icon_style, "✕", ""),
        Some("running") => icon(icon_style, "•", ""),
        Some("mixed") => icon(icon_style, "±", ""),
        Some("unknown") | None => "?",
        Some(_) => "!",
    }
}

pub(super) fn ci_icon_for_status(status: &str, icon_style: IconStyle) -> &'static str {
    match status {
        "passed" => icon(icon_style, "✓", ""),
        "failed" => icon(icon_style, "✕", ""),
        "running" => icon(icon_style, "•", ""),
        "mixed" => icon(icon_style, "±", ""),
        "unknown" => "?",
        _ => "!",
    }
}

pub(super) fn auto_status_label(status: AutoRunStatus) -> &'static str {
    match status {
        AutoRunStatus::Queued => "queued",
        AutoRunStatus::Running => "running",
        AutoRunStatus::Paused => "paused",
        AutoRunStatus::Done => "done",
        AutoRunStatus::Failed => "failed",
        AutoRunStatus::Aborted => "aborted",
    }
}

pub(super) fn pr_state_style(summary: &crate::github::PrSummary) -> Style {
    if pr_has_merge_conflict(summary) {
        error_style()
    } else if summary.merged {
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else if summary.draft {
        muted_style().add_modifier(Modifier::BOLD)
    } else if summary.state == "OPEN" {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        error_style()
    }
}

pub(super) fn review_style(decision: &str) -> Style {
    match decision {
        "APPROVED" => Style::default().fg(Color::Green),
        "CHANGES_REQUESTED" => Style::default().fg(Color::Red),
        "REVIEW_REQUIRED" => attention_style(),
        "COMMENTED" => title_style(false),
        _ => muted_style(),
    }
}

pub(super) fn ci_style(config: &crate::config::Config, session: &Session) -> Style {
    if session.is_default_branch(config) {
        return muted_style();
    }
    match session
        .pr
        .summary
        .as_ref()
        .map(|summary| summary.check_status.as_str())
    {
        Some("passed") => Style::default().fg(Color::Green),
        Some("failed") => Style::default().fg(Color::Red),
        Some("running") => attention_style(),
        Some("mixed") => Style::default().fg(Color::Magenta),
        Some("unknown") | None => muted_style(),
        Some(_) => attention_style(),
    }
}

pub(super) fn pr_check_style(status: &str) -> Style {
    match status {
        "passed" => Style::default().fg(Color::Green),
        "failed" => Style::default().fg(Color::Red),
        "running" => attention_style(),
        "mixed" => Style::default().fg(Color::Magenta),
        "unknown" => muted_style(),
        _ => attention_style(),
    }
}

pub(super) fn pr_style(summary: &crate::github::PrSummary) -> Style {
    if pr_has_merge_conflict(summary) {
        Style::default().fg(Color::Red)
    } else if summary.merged {
        Style::default().fg(Color::Magenta)
    } else if summary.draft {
        muted_style()
    } else if summary.state == "OPEN" {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Red)
    }
}
