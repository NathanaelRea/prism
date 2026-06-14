use crate::agent::{AgentState, output_tail};
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
    session_filter: &str,
    leader_hint: Option<&str>,
) -> Result<(), String> {
    let (cols, rows) = terminal_size();
    write_stdout(&render_frame(
        repo,
        config,
        sessions,
        selected,
        mode_label,
        status_message,
        session_filter,
        leader_hint,
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
    session_filter: &str,
    leader_hint: Option<&str>,
    cols: u16,
    rows: u16,
) -> String {
    let pr_width = if cols >= 118 { 36 } else { 0 };
    let left_width = if pr_width > 0 {
        cols.saturating_sub(pr_width + 26).clamp(42, 68)
    } else {
        cols.saturating_sub(25).clamp(36, 64)
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
                    "Agent Session",
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
                    "Agent / PR",
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
    let visible_indices = visible_session_indices(sessions, session_filter);
    let selected_visible = visible_indices
        .iter()
        .position(|index| *index == selected)
        .unwrap_or(0);
    let start = if selected_visible >= visible_rows {
        selected_visible + 1 - visible_rows
    } else {
        0
    };
    let selected_session = sessions.get(selected);
    let agent_lines = format_agent_panel_lines(selected_session, mode_label);
    let pr_lines = format_pr_panel_lines(config, selected_session);

    for row in 0..visible_rows {
        let visible_index = start + row;
        let left = if let Some(index) = visible_indices.get(visible_index).copied() {
            let session = &sessions[index];
            format_session_row(config, session, index == selected, left_width as usize)
        } else {
            " ".repeat(left_width as usize)
        };
        let center = if row < agent_lines.len() {
            agent_lines.get(row).cloned().unwrap_or_default()
        } else if row == 0 {
            format!(
                "default agent: {}",
                truncate_line(
                    &config.default_agent,
                    center_width.saturating_sub(2) as usize
                )
            )
        } else {
            String::new()
        };
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
            let merged = if row < agent_lines.len() {
                center
            } else {
                pr_lines
                    .get(row - agent_lines.len())
                    .cloned()
                    .unwrap_or_default()
            };
            push_line(
                &mut frame,
                cols,
                format!(
                    "{left}| {}",
                    ansi_cell(&merged, center_width.saturating_sub(2) as usize),
                ),
            );
        }
    }

    let mode_label = if session_filter.trim().is_empty() {
        mode_label.to_string()
    } else {
        format!("{mode_label} /{}", session_filter.trim())
    };
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
    if let Some(hint) = leader_hint {
        append_leader_hint(&mut frame, hint, cols, rows);
    }
    frame
}

fn append_leader_hint(frame: &mut String, hint: &str, cols: u16, rows: u16) {
    let lines = hint.split("  ").collect::<Vec<_>>();
    let width = lines
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0)
        .saturating_add(4)
        .max(18)
        .min(cols.saturating_sub(2) as usize);
    let height = lines.len() + 2;
    let left = (cols as usize).saturating_sub(width + 1).max(1);
    let top = (rows as usize).saturating_sub(height + 1).max(1);
    let text_width = width.saturating_sub(4);
    frame.push_str(&format!(
        "\x1b[{top};{left}H+{}+",
        "-".repeat(width.saturating_sub(2))
    ));
    for (index, line) in lines.iter().enumerate() {
        let y = top + index + 1;
        frame.push_str(&format!(
            "\x1b[{y};{left}H| {} |",
            ansi_cell(line, text_width)
        ));
    }
    frame.push_str(&format!(
        "\x1b[{};{}H+{}+",
        top + height - 1,
        left,
        "-".repeat(width.saturating_sub(2))
    ));
}

fn visible_session_indices(sessions: &[Session], filter: &str) -> Vec<usize> {
    let filter = filter.trim().to_ascii_lowercase();
    sessions
        .iter()
        .enumerate()
        .filter_map(|(index, session)| {
            (filter.is_empty()
                || session.branch.to_ascii_lowercase().contains(&filter)
                || session
                    .prompt_summary
                    .to_ascii_lowercase()
                    .contains(&filter)
                || session.path_display.to_ascii_lowercase().contains(&filter)
                || session
                    .wt_columns
                    .values()
                    .any(|value| value.to_ascii_lowercase().contains(&filter)))
            .then_some(index)
        })
        .collect()
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
        } else if ch == '\x1b' && chars.peek() == Some(&']') {
            chars.next();
            let mut previous = '\0';
            for seq_ch in chars.by_ref() {
                if seq_ch == '\x07' || (previous == '\x1b' && seq_ch == '\\') {
                    break;
                }
                previous = seq_ch;
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
        if ch == '\x1b' && chars.peek() == Some(&']') {
            out.push(ch);
            out.push(chars.next().unwrap());
            let mut previous = '\0';
            for seq_ch in chars.by_ref() {
                out.push(seq_ch);
                if seq_ch == '\x07' || (previous == '\x1b' && seq_ch == '\\') {
                    break;
                }
                previous = seq_ch;
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
    } else if session.unseen_comments {
        color("!", "1;36")
    } else {
        " ".to_string()
    };
    let branch_code = if selected { "1;37" } else { "37" };
    let pr = pr_label(config, session);
    let comments = comment_count_label(config, session);
    let extra = configured_column_label(config, session);
    let status_color = if config.is_default_branch(&session.branch)
        && status_count(&session.status_label, "behind").is_some()
    {
        "31"
    } else {
        git_status_color(&session.status_label)
    };
    let text = format!(
        "{} {} {} {} {} {} {} {} {}",
        marker,
        styled_cell(&session.branch, 22, branch_code),
        styled_cell(
            &git_status_indicator(&session.status_label),
            9,
            status_color
        ),
        styled_cell(
            agent_icon(session.agent_state),
            3,
            agent_state_color(session.agent_state)
        ),
        styled_cell(&pr, 7, pr_color(session)),
        styled_cell(ci_icon(config, session), 3, ci_color(config, session)),
        styled_cell(&comments, 4, comment_color(session)),
        extra,
        truncate_line(summary, 50),
    );
    ansi_cell(&text, width)
}

fn configured_column_label(config: &Config, session: &Session) -> String {
    let mut labels = Vec::new();
    for column in &config.worktree_columns {
        if let Some(value) = session.wt_columns.get(column)
            && !value.trim().is_empty()
        {
            labels.push(format_column_value(column, value));
        }
    }
    truncate_line(&labels.join(" "), 24)
}

fn format_column_value(column: &str, value: &str) -> String {
    if value.starts_with("http://") || value.starts_with("https://") {
        return format!(
            "\x1b]8;;{value}\x1b\\{}\x1b]8;;\x1b\\",
            truncate_line(value, 24)
        );
    }
    if column == "url_active" {
        return if value == "true" { "url:on" } else { "url:off" }.to_string();
    }
    truncate_line(value, 24)
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
        Some("failed") => "x",
        Some("running") => "•",
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

fn format_agent_panel_lines(session: Option<&Session>, mode_label: &str) -> Vec<String> {
    let Some(session) = session else {
        return vec!["No worktrees discovered".to_string()];
    };
    let summary = if session.prompt_summary.is_empty() {
        "No stored prompt summary"
    } else {
        &session.prompt_summary
    };
    let output_tail = output_tail(&session.agent_output);
    let mut lines = vec![
        format!("branch: {}", session.branch),
        format!("mode: {mode_label}"),
        format!("agent: {}", session.agent_state.label()),
        format!("git: {}", session.status_label),
        format!("path: {}", session.path.display()),
        format!("prompt: {summary}"),
    ];
    if !output_tail.is_empty() {
        lines.push(format!("last output: {output_tail}"));
    }
    lines
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
    let review = review_decision_for_display(summary, session.pr.details.as_ref());
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
            color(review_label(&review), review_color(&review)),
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
        "COMMENTED" => "commented",
        "" | "UNKNOWN" => "unknown",
        _ => decision,
    }
}

fn review_color(decision: &str) -> &'static str {
    match decision {
        "APPROVED" => "32",
        "CHANGES_REQUESTED" => "31",
        "REVIEW_REQUIRED" => "33",
        "COMMENTED" => "36",
        _ => "90",
    }
}

fn review_decision_for_display(
    summary: &crate::github::PrSummary,
    details: Option<&crate::github::PrDetails>,
) -> String {
    if !matches!(summary.review_decision.as_str(), "" | "UNKNOWN") {
        return summary.review_decision.clone();
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::path::PathBuf;

    use crate::agent::AgentState;
    use crate::config::{Checks, Config, EscapeKey};
    use crate::github::PrCache;
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
            worktree_columns: Vec::new(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            prompt_templates: BTreeMap::new(),
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
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        }];

        let frame = render_frame(
            &repo, &config, &sessions, 0, "normal", None, "", None, 120, 20,
        );

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
            worktree_columns: Vec::new(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            prompt_templates: BTreeMap::new(),
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
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        }];

        let frame = render_frame(
            &repo,
            &config,
            &sessions,
            0,
            "normal",
            Some("current worktree is dirty"),
            "",
            None,
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
            worktree_columns: Vec::new(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            prompt_templates: BTreeMap::new(),
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
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        }];

        let frame = render_frame(
            &repo, &config, &sessions, 0, "normal", None, "", None, 120, 20,
        );

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
}
