use crate::agent::AgentState;
use crate::config::Config;
use crate::repo::Repository;
use crate::session::Session;
use crate::terminal::{terminal_size, write_stdout};
use crate::util::{status_count, truncate_line};

pub fn draw(
    repo: &Repository,
    config: &Config,
    sessions: &[Session],
    selected: usize,
    mode_label: &str,
    status_message: Option<&str>,
) -> Result<(), String> {
    let (cols, rows) = terminal_size();
    write_stdout(&render_frame(
        repo,
        config,
        sessions,
        selected,
        mode_label,
        status_message,
        cols,
        rows,
    ))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_frame(
    repo: &Repository,
    config: &Config,
    sessions: &[Session],
    selected: usize,
    mode_label: &str,
    status_message: Option<&str>,
    cols: u16,
    rows: u16,
) -> String {
    let pr_width = if cols >= 118 { 36 } else { 0 };
    let left_width = if pr_width > 0 {
        cols.saturating_sub(pr_width + 66).clamp(30, 42)
    } else {
        cols.saturating_sub(58).clamp(28, 42)
    };
    let center_width =
        cols.saturating_sub(left_width + pr_width + if pr_width > 0 { 2 } else { 1 });
    let mut frame = String::from("\x1b[?25l\x1b[H");
    if pr_width > 0 {
        push_line(
            &mut frame,
            cols,
            format!(
                "{}| {}| {}",
                styled_cell("Sessions / Worktrees", left_width as usize, "1;36"),
                styled_cell(
                    "Kanban",
                    center_width.saturating_sub(2) as usize,
                    "1;36"
                ),
                styled_cell("PR", pr_width.saturating_sub(2) as usize, "1;36"),
            ),
        );
        push_line(
            &mut frame,
            cols,
            format!(
                "{}+{}+{}",
                "-".repeat(left_width as usize),
                "-".repeat(center_width as usize),
                "-".repeat(pr_width as usize)
            ),
        );
    } else {
        push_line(
            &mut frame,
            cols,
            format!(
                "{}| {}",
                styled_cell("Sessions / Worktrees", left_width as usize, "1;36"),
                styled_cell(
                    "Kanban",
                    center_width.saturating_sub(2) as usize,
                    "1;36"
                ),
            ),
        );
        push_line(
            &mut frame,
            cols,
            format!(
                "{}+{}",
                "-".repeat(left_width as usize),
                "-".repeat(center_width as usize)
            ),
        );
    }

    let visible_rows = rows.saturating_sub(4) as usize;
    let start = if selected >= visible_rows {
        selected + 1 - visible_rows
    } else {
        0
    };
    let selected_session = sessions.get(selected);
    let kanban_lines = format_kanban_panel_lines(
        config,
        sessions,
        selected,
        center_width.saturating_sub(2) as usize,
        visible_rows,
    );
    let pr_lines = format_pr_panel_lines(config, selected_session);

    for row in 0..visible_rows {
        let index = start + row;
        let left = if let Some(session) = sessions.get(index) {
            format_session_row(config, session, index == selected, left_width as usize)
        } else {
            " ".repeat(left_width as usize)
        };
        let center = kanban_lines.get(row).cloned().unwrap_or_default();
        if pr_width > 0 {
            let pr = pr_lines.get(row).cloned().unwrap_or_default();
            push_line(
                &mut frame,
                cols,
                format!(
                    "{left}| {}| {}",
                    ansi_cell(&center, center_width.saturating_sub(2) as usize),
                    ansi_cell(&pr, pr_width.saturating_sub(2) as usize),
                ),
            );
        } else {
            push_line(
                &mut frame,
                cols,
                format!(
                    "{left}| {}",
                    ansi_cell(&center, center_width.saturating_sub(2) as usize),
                ),
            );
        }
    }

    let footer = match status_message {
        Some(message) => format!(" {mode_label}  repo {}  |  {message} ", repo.root.display()),
        None => format!(" {mode_label}  repo {} ", repo.root.display()),
    };
    if pr_width > 0 {
        push_line(
            &mut frame,
            cols,
            format!(
                "{}+{}+{}",
                "-".repeat(left_width as usize),
                "-".repeat(center_width as usize),
                "-".repeat(pr_width as usize)
            ),
        );
    } else {
        push_line(
            &mut frame,
            cols,
            format!(
                "{}+{}",
                "-".repeat(left_width as usize),
                "-".repeat(center_width as usize)
            ),
        );
    }
    frame.push_str(&fit_line(&footer, cols as usize));
    frame
}

fn push_line(frame: &mut String, cols: u16, line: String) {
    frame.push_str(&fit_line(&line, cols as usize));
    frame.push('\n');
}

fn fit_line(line: &str, cols: usize) -> String {
    let mut line = truncate_ansi_line(line, cols);
    let len = visible_len(&line);
    if len < cols {
        line.push_str(&" ".repeat(cols - len));
    }
    line
}

fn visible_len(line: &str) -> usize {
    let mut len = 0;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for seq_ch in chars.by_ref() {
                if seq_ch.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            len += 1;
        }
    }
    len
}

fn truncate_ansi_line(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if visible_len(text) <= max_chars {
        return text.to_string();
    }
    if max_chars == 1 {
        return "~".to_string();
    }

    let mut out = String::new();
    let mut visible = 0;
    let mut saw_style = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            saw_style = true;
            out.push(ch);
            out.push(chars.next().unwrap());
            for seq_ch in chars.by_ref() {
                out.push(seq_ch);
                if seq_ch.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if visible + 1 >= max_chars {
            out.push('~');
            if saw_style {
                out.push_str("\x1b[0m");
            }
            return out;
        }
        out.push(if ch.is_ascii_control() { ' ' } else { ch });
        visible += 1;
    }
    out
}

fn color(text: &str, code: &str) -> String {
    format!("\x1b[{code}m{text}\x1b[0m")
}

fn plain_cell(text: &str, width: usize) -> String {
    format!("{:<width$}", truncate_line(text, width), width = width)
}

fn styled_cell(text: &str, width: usize, code: &str) -> String {
    color(&plain_cell(text, width), code)
}

fn ansi_cell(text: &str, width: usize) -> String {
    let mut text = truncate_ansi_line(text, width);
    let len = visible_len(&text);
    if len < width {
        text.push_str(&" ".repeat(width - len));
    }
    text
}

fn format_session_row(config: &Config, session: &Session, selected: bool, width: usize) -> String {
    let summary = if session.prompt_summary.is_empty() {
        "-"
    } else {
        &session.prompt_summary
    };
    let marker = if selected {
        color("▶", "1;36")
    } else {
        " ".to_string()
    };
    let branch_code = if selected { "1;37" } else { "37" };
    let pr = pr_label(config, session);
    let comments = comment_count_label(config, session);
    let text = format!(
        "{} {} {} {} {} {} {} {}",
        marker,
        styled_cell(&session.branch, 22, branch_code),
        styled_cell(
            &git_status_indicator(&session.status_label),
            9,
            git_status_color(&session.status_label)
        ),
        styled_cell(
            agent_icon(session.agent_state),
            3,
            agent_state_color(session.agent_state)
        ),
        styled_cell(&pr, 7, pr_color(session)),
        styled_cell(ci_icon(config, session), 3, ci_color(config, session)),
        styled_cell(&comments, 4, comment_color(session)),
        truncate_line(summary, 50),
    );
    ansi_cell(&text, width)
}

#[derive(Clone, Copy)]
enum KanbanLane {
    Plan,
    Impl,
    PrCi,
    Merged,
}

impl KanbanLane {
    fn index(self) -> usize {
        match self {
            Self::Plan => 0,
            Self::Impl => 1,
            Self::PrCi => 2,
            Self::Merged => 3,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Impl => "impl",
            Self::PrCi => "pr/ci",
            Self::Merged => "merged",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Self::Plan => "90",
            Self::Impl => "33",
            Self::PrCi => "32",
            Self::Merged => "35",
        }
    }
}

const KANBAN_LANES: [KanbanLane; 4] = [
    KanbanLane::Plan,
    KanbanLane::Impl,
    KanbanLane::PrCi,
    KanbanLane::Merged,
];

fn format_kanban_panel_lines(
    config: &Config,
    sessions: &[Session],
    selected: usize,
    width: usize,
    visible_rows: usize,
) -> Vec<String> {
    if width < 32 {
        return vec![color("Kanban needs more width", "90")];
    }

    let mut lanes: [Vec<(usize, &Session)>; 4] = std::array::from_fn(|_| Vec::new());
    for (index, session) in sessions.iter().enumerate() {
        if let Some(lane) = kanban_lane(config, session) {
            lanes[lane.index()].push((index, session));
        }
    }

    if lanes.iter().all(Vec::is_empty) {
        return vec![
            color("No feature worktrees", "90"),
            color("Create one with c", "33"),
        ];
    }

    let widths = kanban_column_widths(width);
    let mut lines = Vec::new();

    lines.push(join_kanban_columns(
        KANBAN_LANES
            .iter()
            .enumerate()
            .map(|(index, lane)| {
                let header = format!("{} {}", lane.label(), lanes[index].len());
                ansi_cell(&color(&header, lane.color()), widths[index])
            }),
    ));
    lines.push(join_kanban_columns(
        widths.iter().map(|width| "-".repeat(*width)),
    ));

    let max_lane_rows = lanes.iter().map(Vec::len).max().unwrap_or(0);
    let card_rows = visible_rows.saturating_sub(lines.len() + 1);
    let shown_rows = max_lane_rows.min(card_rows);
    for row in 0..shown_rows {
        lines.push(join_kanban_columns(
            lanes.iter().enumerate().map(|(lane_index, lane_sessions)| {
                if let Some((index, session)) = lane_sessions.get(row) {
                    format_kanban_card(config, session, *index == selected, widths[lane_index])
                } else {
                    " ".repeat(widths[lane_index])
                }
            }),
        ));
    }

    if max_lane_rows > shown_rows && lines.len() < visible_rows {
        lines.push(join_kanban_columns(
            lanes.iter().enumerate().map(|(lane_index, lane_sessions)| {
                let remaining = lane_sessions.len().saturating_sub(shown_rows);
                if remaining > 0 {
                    ansi_cell(&color(&format!("+{remaining} more"), "90"), widths[lane_index])
                } else {
                    " ".repeat(widths[lane_index])
                }
            }),
        ));
    }

    lines
}

fn kanban_lane(config: &Config, session: &Session) -> Option<KanbanLane> {
    if config.is_default_branch(&session.branch) {
        return None;
    }

    if session
        .pr
        .summary
        .as_ref()
        .map(|summary| summary.merged)
        .unwrap_or(false)
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

fn kanban_column_widths(width: usize) -> [usize; 4] {
    let gaps = 3;
    let available = width.saturating_sub(gaps);
    let base = available / KANBAN_LANES.len();
    let mut remainder = available % KANBAN_LANES.len();
    std::array::from_fn(|_| {
        let extra = usize::from(remainder > 0);
        remainder = remainder.saturating_sub(1);
        base + extra
    })
}

fn join_kanban_columns(columns: impl IntoIterator<Item = String>) -> String {
    columns.into_iter().collect::<Vec<_>>().join(" ")
}

fn format_kanban_card(
    config: &Config,
    session: &Session,
    selected: bool,
    width: usize,
) -> String {
    let marker = if selected { color("▶", "1;36") } else { " ".to_string() };
    let mut suffix = git_status_indicator(&session.status_label);
    if let Some(summary) = &session.pr.summary {
        suffix = if suffix.is_empty() {
            format!("#{}", summary.number)
        } else {
            format!("{suffix} #{}", summary.number)
        };
        let ci = ci_icon(config, session);
        if !ci.is_empty() {
            suffix.push(' ');
            suffix.push_str(ci);
        }
    }

    let label_width = width.saturating_sub(2);
    let label = if suffix.is_empty() {
        truncate_line(&session.branch, label_width)
    } else {
        let suffix_width = visible_len(&suffix);
        let branch_width = label_width.saturating_sub(suffix_width + 1).max(1);
        format!(
            "{} {}",
            truncate_line(&session.branch, branch_width),
            truncate_line(&suffix, suffix_width),
        )
    };
    let code = if selected { "1;37" } else { "37" };
    ansi_cell(&format!("{marker} {}", color(&label, code)), width)
}

fn pr_label(config: &Config, session: &Session) -> String {
    if config.is_default_branch(&session.branch) {
        return String::new();
    }
    let Some(summary) = &session.pr.summary else {
        return "no-pr".to_string();
    };
    let icon = if summary.merged {
        "◆"
    } else if summary.draft {
        "◌"
    } else if summary.state == "OPEN" {
        "●"
    } else {
        "×"
    };
    format!("{icon}#{}", summary.number)
}

fn comment_count_label(config: &Config, session: &Session) -> String {
    if config.is_default_branch(&session.branch) {
        return String::new();
    }
    let count = session
        .pr
        .details
        .as_ref()
        .map(|details| details.comments.len() + details.review_comments.len())
        .or_else(|| {
            session
                .pr
                .summary
                .as_ref()
                .map(|summary| summary.comment_count as usize)
        });
    let Some(count) = count else {
        return "C?".to_string();
    };
    format!("C{count}")
}

fn ci_icon(config: &Config, session: &Session) -> &'static str {
    if config.is_default_branch(&session.branch) {
        return "";
    }
    match session
        .pr
        .summary
        .as_ref()
        .map(|summary| summary.check_status.as_str())
    {
        Some("passed") => "✓",
        Some("failed") => "✕",
        Some("running") => "…",
        Some("mixed") => "±",
        Some("unknown") | None => "?",
        Some(_) => "!",
    }
}

fn agent_icon(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "○",
        AgentState::Running => "●",
        AgentState::ExitedOk => "✓",
        AgentState::ExitedError => "✕",
        AgentState::NeedsRestart => "↻",
        AgentState::NeedsInput => "!",
    }
}

fn git_status_indicator(status: &str) -> String {
    let mut out = String::new();
    if let Some(count) = status_count(status, "dirty") {
        out.push('✗');
        out.push_str(&count.to_string());
    }
    if let Some(count) = status_count(status, "ahead") {
        out.push('↑');
        out.push_str(&count.to_string());
    }
    if let Some(count) = status_count(status, "behind") {
        out.push('↓');
        out.push_str(&count.to_string());
    }
    out
}

fn git_status_color(status: &str) -> &'static str {
    if status_count(status, "dirty").is_some() {
        "31"
    } else if status_count(status, "behind").is_some() {
        "33"
    } else if status_count(status, "ahead").is_some() {
        "36"
    } else {
        "32"
    }
}

fn agent_state_color(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "90",
        AgentState::Running => "33",
        AgentState::ExitedOk => "32",
        AgentState::ExitedError => "31",
        AgentState::NeedsRestart => "35",
        AgentState::NeedsInput => "35",
    }
}

fn pr_color(session: &Session) -> &'static str {
    let Some(summary) = &session.pr.summary else {
        return "90";
    };
    if summary.merged {
        "35"
    } else if summary.draft {
        "90"
    } else if summary.state == "OPEN" {
        "32"
    } else {
        "31"
    }
}

fn ci_color(config: &Config, session: &Session) -> &'static str {
    if config.is_default_branch(&session.branch) {
        return "90";
    }
    match session
        .pr
        .summary
        .as_ref()
        .map(|summary| summary.check_status.as_str())
    {
        Some("passed") => "32",
        Some("failed") => "31",
        Some("running") => "33",
        Some("mixed") => "35",
        Some("unknown") | None => "90",
        Some(_) => "33",
    }
}

fn comment_color(session: &Session) -> &'static str {
    let Some(details) = &session.pr.details else {
        return "90";
    };
    if details.comments.is_empty() && details.review_comments.is_empty() {
        "90"
    } else {
        "36"
    }
}

fn format_pr_panel_lines(config: &Config, session: Option<&Session>) -> Vec<String> {
    let Some(session) = session else {
        return vec![color("No selected worktree", "90")];
    };
    if config.is_default_branch(&session.branch) {
        return vec![
            color("Default branch", "1;36"),
            format!("branch {}", truncate_line(&session.branch, 80)),
            color("PR tracking disabled", "90"),
        ];
    }
    if let Some(error) = &session.pr.error {
        return vec![
            color("✕ PR refresh error", "1;31"),
            truncate_line(error, 120),
            color("Press r to retry", "33"),
        ];
    }
    let Some(summary) = &session.pr.summary else {
        let refreshed = session
            .pr
            .last_refreshed
            .as_deref()
            .unwrap_or("not refreshed");
        return vec![
            color("○ No PR detected", "90"),
            format!("branch {}", truncate_line(&session.branch, 80)),
            format!("last {refreshed}"),
            color("P creates one explicitly", "33"),
        ];
    };
    let state = if summary.merged {
        "merged"
    } else if summary.draft {
        "draft"
    } else {
        summary.state.as_str()
    };
    let mut lines = vec![
        color(
            &format!(
                "{} PR #{} {}",
                pr_state_icon(summary),
                summary.number,
                state
            ),
            pr_state_color(summary),
        ),
        color(&truncate_line(&summary.title, 80), "1;37"),
        format!(
            "{} {}   {} {}",
            color("base", "90"),
            truncate_line(&summary.base_ref, 22),
            color("head", "90"),
            truncate_line(&summary.head_ref, 22),
        ),
        format!(
            "{} {}   {} {} {}",
            color("review", "90"),
            color(
                review_label(&summary.review_decision),
                review_color(&summary.review_decision)
            ),
            color("ci", "90"),
            color(ci_icon(config, session), ci_color(config, session)),
            summary.check_status,
        ),
        String::new(),
        color("Description", "1;36"),
    ];
    lines.extend(description_lines(&summary.body, 4));
    if let Some(details) = &session.pr.details {
        lines.push(String::new());
        lines.push(color("Activity", "1;36"));
        lines.push(format!(
            "{} {}   {} {}   {} {}",
            color("comments", "90"),
            details.comments.len() + details.review_comments.len(),
            color("reviews", "90"),
            details.reviews.len(),
            color("files", "90"),
            details.files.len(),
        ));
        lines.extend(pr_comment_lines(details, 5));
        if !details.failing_checks.is_empty() {
            lines.push(color("Failing checks", "1;31"));
            for check in details.failing_checks.iter().take(3) {
                lines.push(format!("{} {}", color("✕", "31"), truncate_line(check, 80)));
            }
        }
    } else {
        lines.push(String::new());
        lines.push(color("Activity pending", "90"));
    }
    if let Some(refreshed) = &session.pr.last_refreshed {
        lines.push(String::new());
        lines.push(color(&format!("refreshed {refreshed}"), "90"));
    }
    lines
}

fn pr_comment_lines(details: &crate::github::PrDetails, max_comments: usize) -> Vec<String> {
    let mut lines = vec![String::new(), color("Comments", "1;36")];
    let mut shown = 0;

    for comment in details.comments.iter().rev() {
        if shown >= max_comments {
            break;
        }
        append_comment(&mut lines, &comment.author, "", &comment.body);
        shown += 1;
    }

    for review in details
        .reviews
        .iter()
        .rev()
        .filter(|review| !review.body.trim().is_empty())
    {
        if shown >= max_comments {
            break;
        }
        append_comment(
            &mut lines,
            &review.author,
            review_label(&review.state),
            &review.body,
        );
        shown += 1;
    }

    for comment in details.review_comments.iter().rev() {
        if shown >= max_comments {
            break;
        }
        let context = if comment.line.is_empty() {
            comment.path.clone()
        } else {
            format!("{}:{}", comment.path, comment.line)
        };
        append_comment(&mut lines, &comment.author, &context, &comment.body);
        shown += 1;
    }

    if shown == 0 {
        lines.push(color("No comments", "90"));
    }

    let total = details.comments.len()
        + details.review_comments.len()
        + details
            .reviews
            .iter()
            .filter(|review| !review.body.trim().is_empty())
            .count();
    if total > shown {
        lines.push(color(&format!("+{} more", total - shown), "90"));
    }

    lines
}

fn append_comment(lines: &mut Vec<String>, author: &str, context: &str, body: &str) {
    let author = if author.trim().is_empty() {
        "unknown"
    } else {
        author.trim()
    };
    let context = context.trim();
    if context.is_empty() {
        lines.push(format!(
            "{} {}",
            color("@", "90"),
            truncate_line(author, 24)
        ));
    } else {
        lines.push(format!(
            "{} {} {}",
            color("@", "90"),
            truncate_line(author, 18),
            color(&truncate_line(context, 28), "90"),
        ));
    }
    let mut body_lines = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(2)
        .peekable();
    if body_lines.peek().is_none() {
        lines.push(color("  empty comment", "90"));
        return;
    }
    for line in body_lines {
        lines.push(format!("  {}", truncate_line(line, 90)));
    }
}

fn description_lines(body: &str, max_lines: usize) -> Vec<String> {
    let lines = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(max_lines)
        .map(|line| truncate_line(line, 90))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        vec![color("No description", "90")]
    } else {
        lines
    }
}

fn pr_state_icon(summary: &crate::github::PrSummary) -> &'static str {
    if summary.merged {
        "◆"
    } else if summary.draft {
        "◌"
    } else if summary.state == "OPEN" {
        "●"
    } else {
        "×"
    }
}

fn pr_state_color(summary: &crate::github::PrSummary) -> &'static str {
    if summary.merged {
        "1;35"
    } else if summary.draft {
        "1;90"
    } else if summary.state == "OPEN" {
        "1;32"
    } else {
        "1;31"
    }
}

fn review_label(decision: &str) -> &str {
    match decision {
        "APPROVED" => "approved",
        "CHANGES_REQUESTED" => "changes",
        "REVIEW_REQUIRED" => "needed",
        "" | "UNKNOWN" => "unknown",
        _ => decision,
    }
}

fn review_color(decision: &str) -> &'static str {
    match decision {
        "APPROVED" => "32",
        "CHANGES_REQUESTED" => "31",
        "REVIEW_REQUIRED" => "33",
        _ => "90",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::path::PathBuf;

    use crate::agent::AgentState;
    use crate::config::{Checks, Config, EscapeKey};
    use crate::github::{PrCache, PrSummary};
    use crate::repo::Repository;
    use crate::session::Session;

    use super::{git_status_indicator, render_frame};

    #[test]
    fn render_frame_does_not_clear_the_whole_screen() {
        let repo = Repository {
            root: PathBuf::from("/repo"),
        };
        let config = Config {
            default_agent: "opencode".to_string(),
            default_base: None,
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            escape_key: EscapeKey::EscEsc,
            checks: Checks::default(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/user.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-repo-config.toml"),
        };
        let sessions = vec![Session {
            path: PathBuf::from("/repo"),
            path_display: "/repo".to_string(),
            branch: "main".to_string(),
            prompt_summary: "summary".to_string(),
            adopted: true,
            hidden: false,
            status_label: "clean".to_string(),
            agent: None,
            agent_output: VecDeque::new(),
            agent_state: AgentState::Idle,
            pr: PrCache::default(),
        }];

        let frame = render_frame(&repo, &config, &sessions, 0, "normal", None, 120, 20);

        assert!(frame.starts_with("\x1b[?25l\x1b[H"));
        assert!(!frame.contains("\x1b[2J"));
        assert!(!frame.contains("\x1b[2K"));
    }

    #[test]
    fn render_frame_keeps_status_message_in_footer() {
        let repo = Repository {
            root: PathBuf::from("/repo"),
        };
        let config = Config {
            default_agent: "opencode".to_string(),
            default_base: None,
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            escape_key: EscapeKey::EscEsc,
            checks: Checks::default(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/user.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-repo-config.toml"),
        };
        let sessions = vec![Session {
            path: PathBuf::from("/repo"),
            path_display: "/repo".to_string(),
            branch: "main".to_string(),
            prompt_summary: "summary".to_string(),
            adopted: true,
            hidden: false,
            status_label: "clean".to_string(),
            agent: None,
            agent_output: VecDeque::new(),
            agent_state: AgentState::Idle,
            pr: PrCache::default(),
        }];

        let frame = render_frame(
            &repo,
            &config,
            &sessions,
            0,
            "normal",
            Some("current worktree is dirty"),
            120,
            20,
        );

        assert!(frame.contains("normal  repo /repo  |  current worktree is dirty"));
        assert!(!frame.contains("status:"));
    }

    #[test]
    fn default_branch_does_not_render_pr_placeholders() {
        let repo = Repository {
            root: PathBuf::from("/repo"),
        };
        let config = Config {
            default_agent: "opencode".to_string(),
            default_base: Some("main".to_string()),
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            escape_key: EscapeKey::EscEsc,
            checks: Checks::default(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/user.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-repo-config.toml"),
        };
        let sessions = vec![Session {
            path: PathBuf::from("/repo"),
            path_display: "/repo".to_string(),
            branch: "main".to_string(),
            prompt_summary: String::new(),
            adopted: false,
            hidden: false,
            status_label: "clean".to_string(),
            agent: None,
            agent_output: VecDeque::new(),
            agent_state: AgentState::Idle,
            pr: PrCache::default(),
        }];

        let frame = render_frame(&repo, &config, &sessions, 0, "normal", None, 120, 20);

        assert!(frame.contains("Default branch"));
        assert!(frame.contains("PR tracking disabled"));
        assert!(!frame.contains("no-pr"));
        assert!(!frame.contains("C?"));
    }

    #[test]
    fn git_status_indicator_uses_arrows() {
        assert_eq!(git_status_indicator("clean"), "");
        assert_eq!(git_status_indicator("dirty 1"), "✗1");
        assert_eq!(git_status_indicator("ahead 3"), "↑3");
        assert_eq!(git_status_indicator("behind 2"), "↓2");
        assert_eq!(git_status_indicator("ahead 3 behind 2"), "↑3↓2");
        assert_eq!(git_status_indicator("dirty 4 ahead 3 behind 2"), "✗4↑3↓2");
    }

    #[test]
    fn kanban_panel_groups_sessions_in_workflow_order() {
        let repo = Repository {
            root: PathBuf::from("/repo"),
        };
        let config = test_config(Some("main"));
        let sessions = vec![
            test_session("main", "clean", AgentState::Idle, PrCache::default()),
            test_session("planned-work", "clean", AgentState::Idle, PrCache::default()),
            test_session("impl-work", "dirty 1", AgentState::Idle, PrCache::default()),
            test_session("pr-work", "clean", AgentState::Idle, test_pr(12, false)),
            test_session("merged-work", "clean", AgentState::Idle, test_pr(13, true)),
        ];

        let frame = render_frame(&repo, &config, &sessions, 2, "normal", None, 160, 20);
        let frame = crate::util::strip_ansi(&frame);

        let plan = frame.find("plan 1").expect("plan lane");
        let implementation = frame.find("impl 1").expect("impl lane");
        let pr_ci = frame.find("pr/ci 1").expect("pr/ci lane");
        let merged = frame.find("merged 1").expect("merged lane");
        assert!(plan < implementation);
        assert!(implementation < pr_ci);
        assert!(pr_ci < merged);
        assert!(frame.contains("planned-work"));
        assert!(frame.contains("impl-work"));
        assert!(frame.contains("pr-work"));
        assert!(frame.contains("merged-work"));
    }

    fn test_config(default_base: Option<&str>) -> Config {
        Config {
            default_agent: "opencode".to_string(),
            default_base: default_base.map(str::to_string),
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            escape_key: EscapeKey::EscEsc,
            checks: Checks::default(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/user.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-repo-config.toml"),
        }
    }

    fn test_session(
        branch: &str,
        status_label: &str,
        agent_state: AgentState,
        pr: PrCache,
    ) -> Session {
        Session {
            path: PathBuf::from(format!("/repo/{branch}")),
            path_display: format!("/repo/{branch}"),
            branch: branch.to_string(),
            prompt_summary: String::new(),
            adopted: false,
            hidden: false,
            status_label: status_label.to_string(),
            agent: None,
            agent_output: VecDeque::new(),
            agent_state,
            pr,
        }
    }

    fn test_pr(number: u64, merged: bool) -> PrCache {
        PrCache {
            summary: Some(PrSummary {
                number,
                title: format!("PR {number}"),
                body: String::new(),
                url: format!("https://example.com/{number}"),
                state: "OPEN".to_string(),
                review_decision: "".to_string(),
                head_ref: format!("branch-{number}"),
                base_ref: "main".to_string(),
                head_sha: format!("sha-{number}"),
                updated_at: "now".to_string(),
                check_status: "passed".to_string(),
                comment_count: 0,
                merged,
                draft: false,
            }),
            ..PrCache::default()
        }
    }
}
