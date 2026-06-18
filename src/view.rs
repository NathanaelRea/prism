use std::collections::BTreeMap;

use crate::agent::AgentState;
use crate::config::Config;
use crate::github::PrCache;
use crate::session::Session;
use crate::terminal::{terminal_size, write_stdout};
use crate::tui::PanelFocus;
use crate::util::{status_count, truncate_line};

pub(crate) struct FrameModel<'a> {
    pub config: &'a Config,
    pub sessions: &'a [Session],
    pub status: Vec<StatusRow>,
    pub repos: Vec<RepoRow>,
    pub worktrees: Vec<WorktreeRow>,
    pub current_repo_index: usize,
    pub selected_repo_label: String,
    pub selected_repo_root: String,
    pub selected_session: Option<usize>,
    pub focus: PanelFocus,
    pub mode_label: &'a str,
    pub status_message: Option<&'a str>,
    pub repo_filter: &'a str,
    pub worktree_filter: &'a str,
    pub leader_hint: Option<&'a str>,
}

pub(crate) struct StatusRow {
    pub label: String,
    pub value: String,
    pub attention: bool,
}

pub(crate) struct RepoRow {
    pub label: String,
    pub root: String,
    pub key: Option<char>,
    pub health: String,
    pub selected: bool,
}

pub(crate) struct WorktreeRow {
    pub session_index: usize,
    pub repo_root: String,
    pub worktree_path: String,
    pub branch: String,
    pub kind: WorktreeKind,
    pub adopted: bool,
    pub status_label: String,
    pub agent_state: AgentState,
    pub pr: PrCache,
    pub wt_columns: BTreeMap<String, String>,
    pub unseen_comments: bool,
    pub prompt_summary: String,
    pub selected: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorktreeKind {
    DefaultBranch,
    FeatureWorktree,
    Detached,
}

pub(crate) fn draw_model(model: &FrameModel<'_>) -> Result<(), String> {
    let (cols, rows) = terminal_size();
    write_stdout(&render_model_frame(model, cols, rows))
}

pub(crate) fn render_model_frame(model: &FrameModel<'_>, cols: u16, rows: u16) -> String {
    let status_width = if cols >= 160 { 22 } else { 16 }
        .min(cols.saturating_sub(62))
        .max(14);
    let repo_width = if cols >= 160 { 28 } else { 20 }
        .min(cols.saturating_sub(status_width + 44))
        .max(16);
    let worktree_width = if cols >= 160 {
        50
    } else if cols >= 120 {
        40
    } else {
        30
    }
    .min(cols.saturating_sub(status_width + repo_width + 22))
    .max(24);
    let main_width = cols
        .saturating_sub(status_width + repo_width + worktree_width + 3)
        .max(10);
    let mut frame = String::from("\x1b[?25l\x1b[H");
    push_line(
        &mut frame,
        cols,
        format!(
            "{}|{}|{}|{}",
            panel_title(
                "1 Status",
                model.focus == PanelFocus::Status,
                status_width as usize,
            ),
            panel_title(
                "2 Repos",
                model.focus == PanelFocus::Repos,
                repo_width as usize
            ),
            panel_title(
                "3 Worktrees / Sessions",
                model.focus == PanelFocus::Worktrees,
                worktree_width as usize,
            ),
            styled_cell("Main", main_width as usize, "1;36"),
        ),
    );
    push_line(
        &mut frame,
        cols,
        format!(
            "{}+{}+{}+{}",
            "-".repeat(status_width as usize),
            "-".repeat(repo_width as usize),
            "-".repeat(worktree_width as usize),
            "-".repeat(main_width as usize),
        ),
    );

    let visible_rows = rows.saturating_sub(4) as usize;
    let repo_selected = model.repos.iter().position(|row| row.selected).unwrap_or(0);
    let repo_start = scroll_start(repo_selected, visible_rows);
    let worktree_selected = model
        .worktrees
        .iter()
        .position(|row| row.selected)
        .unwrap_or(0);
    let worktree_start = scroll_start(worktree_selected, visible_rows);
    let main_lines = match model.focus {
        PanelFocus::Status => format_status_dashboard_lines(model, main_width as usize),
        PanelFocus::Repos => format_repo_overview_lines(model, main_width as usize, visible_rows),
        PanelFocus::Worktrees => format_worktree_detail_lines(model, main_width as usize),
    };

    for row in 0..visible_rows {
        let status = model
            .status
            .get(row)
            .map(|status| format_status_row(status, status_width as usize))
            .unwrap_or_else(|| " ".repeat(status_width as usize));
        let repo = model
            .repos
            .get(repo_start + row)
            .map(|repo| format_repo_row(repo, repo_width as usize))
            .unwrap_or_else(|| {
                empty_state_cell(
                    row,
                    model.repos.is_empty(),
                    if model.repo_filter.trim().is_empty() {
                        "No repos"
                    } else {
                        "No matches"
                    },
                    repo_width as usize,
                )
            });
        let worktree = model
            .worktrees
            .get(worktree_start + row)
            .map(|worktree| format_worktree_row(model.config, worktree, worktree_width as usize))
            .unwrap_or_else(|| {
                empty_state_cell(
                    row,
                    model.worktrees.is_empty(),
                    if model.worktree_filter.trim().is_empty() {
                        "No worktrees"
                    } else {
                        "No matches"
                    },
                    worktree_width as usize,
                )
            });
        let main = main_lines.get(row).cloned().unwrap_or_default();
        push_line(
            &mut frame,
            cols,
            format!(
                "{status}|{repo}|{worktree}|{}",
                ansi_cell(&main, main_width as usize),
            ),
        );
    }

    push_line(
        &mut frame,
        cols,
        format!(
            "{}+{}+{}+{}",
            "-".repeat(status_width as usize),
            "-".repeat(repo_width as usize),
            "-".repeat(worktree_width as usize),
            "-".repeat(main_width as usize),
        ),
    );
    let mode_label = scoped_mode_label(model);
    let actions = footer_actions(model);
    let footer = match model.status_message {
        Some(message) => format!(
            " {mode_label}  repo {}  |  {actions}  |  {message} ",
            model.selected_repo_root
        ),
        None => format!(
            " {mode_label}  repo {}  |  {actions} ",
            model.selected_repo_root
        ),
    };
    frame.push_str(&fit_line(&footer, cols as usize));
    if let Some(hint) = model.leader_hint {
        append_leader_hint(&mut frame, hint, cols, rows);
    }
    frame
}

fn panel_title(title: &str, focused: bool, width: usize) -> String {
    if focused {
        styled_cell(&format!("[{title}]"), width, "1;36")
    } else {
        styled_cell(title, width, "36")
    }
}

fn scroll_start(selected: usize, visible_rows: usize) -> usize {
    if selected >= visible_rows {
        selected + 1 - visible_rows
    } else {
        0
    }
}

fn scoped_mode_label(model: &FrameModel<'_>) -> String {
    let filter = match model.focus {
        PanelFocus::Status => "",
        PanelFocus::Repos => model.repo_filter.trim(),
        PanelFocus::Worktrees => model.worktree_filter.trim(),
    };
    if filter.is_empty() {
        model.mode_label.to_string()
    } else {
        format!("{} /{}", model.mode_label, filter)
    }
}

fn footer_actions(model: &FrameModel<'_>) -> String {
    match model.focus {
        PanelFocus::Status => {
            "Enter focus repos  ? help  Tab next  2 repos  3 worktrees".to_string()
        }
        PanelFocus::Repos => {
            let mut actions = Vec::new();
            if !model.worktrees.is_empty() {
                actions.push("Enter focus worktrees");
            }
            actions.extend([
                "c create",
                "p pull",
                "Space Enter terminal",
                "Space g g lazygit",
                "A add",
                "R repos",
                "/ search",
            ]);
            actions.join("  ")
        }
        PanelFocus::Worktrees => {
            let Some(row) = model.worktrees.iter().find(|row| row.selected) else {
                return "/ search".to_string();
            };
            let mut actions = vec!["Space Enter terminal", "Space g g lazygit"];
            if row.kind != WorktreeKind::DefaultBranch {
                actions.insert(0, "Enter open");
            }
            if row.kind == WorktreeKind::FeatureWorktree {
                actions.push("Space g P push");
                if row.pr.summary.is_some() {
                    actions.push("Space g M merge");
                    actions.push("Space g f review");
                }
            }
            if row.kind != WorktreeKind::DefaultBranch {
                actions.push("D delete");
            }
            actions.push("/ search");
            actions.join("  ")
        }
    }
}

fn format_status_row(row: &StatusRow, width: usize) -> String {
    let code = if row.attention { "1;33" } else { "37" };
    let label_width = width.saturating_sub(row.value.chars().count() + 2).max(1);
    let text = format!(
        "{} {}",
        styled_cell(&row.label, label_width, code),
        color(&row.value, if row.attention { "1;33" } else { "90" }),
    );
    ansi_cell(&text, width)
}

fn empty_state_cell(row: usize, empty: bool, label: &str, width: usize) -> String {
    if empty && row == 0 {
        ansi_cell(&color(label, "90"), width)
    } else {
        " ".repeat(width)
    }
}

fn format_repo_row(row: &RepoRow, width: usize) -> String {
    let marker = if row.selected {
        color("▶", "1;36")
    } else {
        " ".to_string()
    };
    let key = row
        .key
        .map(|key| format!("Space {key}"))
        .unwrap_or_else(|| "       ".to_string());
    let label = if row.label.trim().is_empty() {
        row.root.as_str()
    } else {
        row.label.as_str()
    };
    let text = format!(
        "{} {} {} {}",
        marker,
        styled_cell(&key, 7, "90"),
        styled_cell(label, 18, if row.selected { "1;37" } else { "37" }),
        color(&row.health, "90"),
    );
    ansi_cell(&text, width)
}

fn format_worktree_row(config: &Config, row: &WorktreeRow, width: usize) -> String {
    let path_hint = row
        .worktree_path
        .strip_prefix(&row.repo_root)
        .unwrap_or(&row.worktree_path)
        .trim_start_matches('/');
    let summary = if !row.prompt_summary.is_empty() {
        row.prompt_summary.as_str()
    } else if !path_hint.is_empty() {
        path_hint
    } else {
        "-"
    };
    let row_number = row.session_index.saturating_add(1).to_string();
    let marker = if row.selected {
        color("▶", "1;36")
    } else if row.unseen_comments {
        color("!", "1;36")
    } else {
        " ".to_string()
    };
    let branch_code = if row.selected { "1;37" } else { "37" };
    let kind = match row.kind {
        WorktreeKind::DefaultBranch => "base",
        WorktreeKind::FeatureWorktree if row.adopted => "wt",
        WorktreeKind::FeatureWorktree => "unadopted",
        WorktreeKind::Detached => "detached",
    };
    let pr = pr_label_for_row(config, row);
    let review = review_icon_for_row(config, row);
    let comments = comment_count_label_for_row(config, row);
    let extra = configured_column_label_for_values(config, &row.wt_columns);
    let status_color = if row.kind == WorktreeKind::DefaultBranch
        && status_count(&row.status_label, "behind").is_some()
    {
        "31"
    } else {
        git_status_color(&row.status_label)
    };
    let text = format!(
        "{} {} {} {} {} {} {} {} {} {} {} {}",
        marker,
        styled_cell(&row_number, 3, "90"),
        styled_cell(kind, 9, "90"),
        styled_cell(&row.branch, 22, branch_code),
        styled_cell(&git_status_indicator(&row.status_label), 9, status_color),
        styled_cell(
            agent_icon(row.agent_state),
            3,
            agent_state_color(row.agent_state)
        ),
        styled_cell(&pr, 7, pr_color_for_cache(&row.pr)),
        styled_cell(&review, 3, review_icon_color_for_row(config, row)),
        styled_cell(
            ci_icon_for_row(config, row),
            3,
            ci_color_for_row(config, row)
        ),
        styled_cell(&comments, 4, comment_color_for_cache(&row.pr)),
        extra,
        truncate_line(summary, 50),
    );
    ansi_cell(&text, width)
}

fn format_status_dashboard_lines(model: &FrameModel<'_>, width: usize) -> Vec<String> {
    let mut lines = vec![
        color("      ____       _               ", "1;36"),
        color("     / __ \\_____(_)________ ___ ", "1;36"),
        color("    / /_/ / ___/ / ___/ __ `__ \\", "1;36"),
        color("   / ____/ /  / (__  ) / / / / /", "1;36"),
        color("  /_/   /_/  /_/____/_/ /_/ /_/ ", "1;36"),
        String::new(),
        format!("version {}", env!("CARGO_PKG_VERSION")),
        format!(
            "selected repo {}",
            truncate_line(&model.selected_repo_label, width)
        ),
        truncate_line(&model.selected_repo_root, width),
        String::new(),
        color("Navigation", "1;37"),
        "1 status  2 repos  3 worktrees".to_string(),
        "Tab or h/l moves focus".to_string(),
        "Space 1-9 jumps to configured repos".to_string(),
        String::new(),
        color("Documentation", "1;37"),
        dashboard_link("GitHub repository", "https://github.com/NathanaelRea/prism"),
        dashboard_link(
            "Keybindings",
            "https://github.com/NathanaelRea/prism/blob/main/docs/keybindings.md",
        ),
        dashboard_link(
            "Configuration",
            "https://github.com/NathanaelRea/prism/blob/main/docs/config.md",
        ),
        dashboard_link(
            "README",
            "https://github.com/NathanaelRea/prism/blob/main/README.md",
        ),
    ];
    if width < 46 {
        lines.retain(|line| visible_len(line) <= width || !line.contains("github.com"));
    }
    lines
}

fn dashboard_link(label: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\  {url}")
}

fn format_repo_overview_lines(
    model: &FrameModel<'_>,
    width: usize,
    visible_rows: usize,
) -> Vec<String> {
    let mut lines = vec![
        color(&model.selected_repo_label, "1;36"),
        truncate_line(&model.selected_repo_root, width),
    ];
    if let Some(row) = model.repos.iter().find(|row| row.selected) {
        lines.push(format!("health {}", row.health));
    }
    if let Some(index) = model.selected_session {
        if let Some(session) = model.sessions.get(index) {
            lines.push(format!(
                "remembered {} {}",
                truncate_line(&session.branch, 28),
                git_status_indicator(&session.status_label),
            ));
        }
    }
    lines.push(String::new());
    let indices = model
        .sessions
        .iter()
        .enumerate()
        .filter_map(|(index, session)| {
            (session.repo_index == model.current_repo_index).then_some(index)
        })
        .collect::<Vec<_>>();
    lines.extend(format_kanban_panel_lines(
        model.config,
        model.sessions,
        &indices,
        model.selected_session,
        width,
        visible_rows.saturating_sub(lines.len()),
    ));
    lines
}

fn format_worktree_detail_lines(model: &FrameModel<'_>, width: usize) -> Vec<String> {
    let Some(index) = model.selected_session else {
        return vec![color("No selected worktree", "90")];
    };
    let Some(session) = model.sessions.get(index) else {
        return vec![color("No selected worktree", "90")];
    };
    let mut lines = vec![
        color(&session.branch, "1;36"),
        truncate_line(&session.path_display, width),
        format!(
            "status {}  agent {}  adopted {}",
            git_status_indicator(&session.status_label),
            agent_icon(session.agent_state),
            if session.adopted { "yes" } else { "no" },
        ),
    ];
    if !session.prompt_summary.trim().is_empty() {
        lines.push(format!(
            "prompt {}",
            truncate_line(&session.prompt_summary, width)
        ));
    }
    if let Some(line) = session.agent_output.back() {
        lines.push(format!("agent {}", truncate_line(line, width)));
    }
    lines.push(String::new());
    lines.extend(format_pr_panel_lines(model.config, Some(session)));
    lines
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
    session_indices: &[usize],
    selected: Option<usize>,
    width: usize,
    visible_rows: usize,
) -> Vec<String> {
    if width < 32 {
        return vec![color("Kanban needs more width", "90")];
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
            color("No feature worktrees", "90"),
            color("Create one with c", "33"),
        ];
    }

    let widths = kanban_column_widths(width);
    let mut lines = Vec::new();

    lines.push(join_kanban_columns(KANBAN_LANES.iter().enumerate().map(
        |(index, lane)| {
            let header = format!("{} {}", lane.label(), lanes[index].len());
            ansi_cell(&color(&header, lane.color()), widths[index])
        },
    )));
    lines.push(join_kanban_columns(
        widths.iter().map(|width| "-".repeat(*width)),
    ));

    let max_lane_rows = lanes.iter().map(Vec::len).max().unwrap_or(0);
    let card_rows = visible_rows.saturating_sub(lines.len() + 1);
    let shown_rows = max_lane_rows.min(card_rows);
    for row in 0..shown_rows {
        lines.push(join_kanban_columns(lanes.iter().enumerate().map(
            |(lane_index, lane_sessions)| {
                if let Some((index, session)) = lane_sessions.get(row) {
                    format_kanban_card(
                        config,
                        session,
                        Some(*index) == selected,
                        widths[lane_index],
                    )
                } else {
                    " ".repeat(widths[lane_index])
                }
            },
        )));
    }

    if max_lane_rows > shown_rows && lines.len() < visible_rows {
        lines.push(join_kanban_columns(lanes.iter().enumerate().map(
            |(lane_index, lane_sessions)| {
                let remaining = lane_sessions.len().saturating_sub(shown_rows);
                if remaining > 0 {
                    ansi_cell(
                        &color(&format!("+{remaining} more"), "90"),
                        widths[lane_index],
                    )
                } else {
                    " ".repeat(widths[lane_index])
                }
            },
        )));
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

fn format_kanban_card(config: &Config, session: &Session, selected: bool, width: usize) -> String {
    let marker = if selected {
        color("▶", "1;36")
    } else {
        " ".to_string()
    };
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
        let available_suffix_width = label_width.saturating_sub(2);
        if available_suffix_width == 0 {
            truncate_line(&session.branch, label_width)
        } else {
            let suffix = truncate_line(&suffix, available_suffix_width);
            let suffix_width = visible_len(&suffix);
            let branch_width = label_width.saturating_sub(suffix_width + 1).max(1);
            format!(
                "{} {}",
                truncate_line(&session.branch, branch_width),
                suffix,
            )
        }
    };
    let code = if selected { "1;37" } else { "37" };
    ansi_cell(&format!("{marker} {}", color(&label, code)), width)
}

#[cfg(test)]
fn configured_column_label(config: &Config, session: &Session) -> String {
    configured_column_label_for_values(config, &session.wt_columns)
}

fn configured_column_label_for_values(
    config: &Config,
    values: &BTreeMap<String, String>,
) -> String {
    let mut labels = Vec::new();
    for column in &config.worktree_columns {
        if let Some(value) = values.get(column)
            && !value.trim().is_empty()
        {
            labels.push(format_column_value(column, value));
        }
    }
    truncate_ansi_line(&labels.join(" "), 24)
}

fn format_column_value(column: &str, value: &str) -> String {
    if value.starts_with("http://") || value.starts_with("https://") {
        let url = strip_ascii_control_chars(value);
        return format!(
            "\x1b]8;;{url}\x1b\\{}\x1b]8;;\x1b\\",
            truncate_line(value, 24)
        );
    }
    if column == "url_active" {
        return if value == "true" { "url:on" } else { "url:off" }.to_string();
    }
    truncate_line(value, 24)
}

fn strip_ascii_control_chars(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_ascii_control()).collect()
}

fn pr_label_for_row(config: &Config, row: &WorktreeRow) -> String {
    if row.kind == WorktreeKind::DefaultBranch || config.is_default_branch(&row.branch) {
        return String::new();
    }
    let Some(summary) = &row.pr.summary else {
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

fn review_icon_for_row(config: &Config, row: &WorktreeRow) -> String {
    if row.kind == WorktreeKind::DefaultBranch || config.is_default_branch(&row.branch) {
        return String::new();
    }
    let Some(summary) = &row.pr.summary else {
        return String::new();
    };
    let review = review_decision_for_display(summary, row.pr.details.as_ref());
    match review.as_str() {
        "APPROVED" => "✓",
        "CHANGES_REQUESTED" => "!",
        "REVIEW_REQUIRED" if !summary.requested_reviewers.is_empty() => "@",
        "REVIEW_REQUIRED" => "?",
        "COMMENTED" => "•",
        _ => "",
    }
    .to_string()
}

fn review_icon_color_for_row(config: &Config, row: &WorktreeRow) -> &'static str {
    if row.kind == WorktreeKind::DefaultBranch || config.is_default_branch(&row.branch) {
        return "90";
    }
    let Some(summary) = &row.pr.summary else {
        return "90";
    };
    let review = review_decision_for_display(summary, row.pr.details.as_ref());
    review_color(&review)
}

fn comment_count_label_for_row(config: &Config, row: &WorktreeRow) -> String {
    if row.kind == WorktreeKind::DefaultBranch || config.is_default_branch(&row.branch) {
        return String::new();
    }
    let count = row
        .pr
        .details
        .as_ref()
        .map(|details| details.comments.len() + details.review_comments.len())
        .or_else(|| {
            row.pr
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

fn ci_icon_for_row(config: &Config, row: &WorktreeRow) -> &'static str {
    if row.kind == WorktreeKind::DefaultBranch || config.is_default_branch(&row.branch) {
        return "";
    }
    match row
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

fn pr_color_for_cache(pr: &PrCache) -> &'static str {
    let Some(summary) = &pr.summary else {
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

fn ci_color_for_row(config: &Config, row: &WorktreeRow) -> &'static str {
    if row.kind == WorktreeKind::DefaultBranch || config.is_default_branch(&row.branch) {
        return "90";
    }
    match row
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

fn comment_color_for_cache(pr: &PrCache) -> &'static str {
    let Some(details) = &pr.details else {
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
    ];
    if !summary.requested_reviewers.is_empty() {
        lines.push(format!(
            "{} {}",
            color("awaiting", "90"),
            truncate_line(&summary.requested_reviewers.join(", "), 80),
        ));
    }
    lines.push(String::new());
    lines.push(color("Description", "1;36"));
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::path::PathBuf;

    use crate::agent::AgentState;
    use crate::config::{Checks, Config, EscapeKey, MergeMethod};
    use crate::github::{PrCache, PrSummary};
    use crate::session::Session;
    use crate::tui::PanelFocus;

    use super::{
        FrameModel, RepoRow, StatusRow, WorktreeKind, WorktreeRow, configured_column_label,
        format_column_value, format_repo_row, git_status_indicator, render_model_frame,
        visible_len,
    };

    #[test]
    fn render_model_frame_does_not_clear_the_whole_screen() {
        let config = test_config(Some("main"));
        let sessions = vec![test_session(
            "main",
            "clean",
            AgentState::Idle,
            PrCache::default(),
        )];
        let model = test_model(&config, &sessions, Some(0), PanelFocus::Repos, None);
        let frame = render_model_frame(&model, 180, 20);

        assert!(frame.starts_with("\x1b[?25l\x1b[H"));
        assert!(!frame.contains("\x1b[2J"));
        assert!(!frame.contains("\x1b[2K"));
    }

    #[test]
    fn render_model_frame_keeps_status_message_in_footer() {
        let config = test_config(Some("main"));
        let sessions = vec![test_session(
            "main",
            "clean",
            AgentState::Idle,
            PrCache::default(),
        )];
        let model = test_model(
            &config,
            &sessions,
            Some(0),
            PanelFocus::Repos,
            Some("current worktree is dirty"),
        );
        let frame = render_model_frame(&model, 180, 20);

        assert!(frame.contains("normal  repo /repo"));
        assert!(frame.contains("current worktree is dirty"));
        assert!(!frame.contains("status:"));
    }

    #[test]
    fn repo_row_displays_leader_shortcut() {
        let row = RepoRow {
            label: "repo".to_string(),
            root: "/repo".to_string(),
            key: Some('1'),
            health: "ok".to_string(),
            selected: false,
        };
        let row = crate::util::strip_ansi(&format_repo_row(&row, 80));

        assert!(row.contains("Space 1"));
        assert!(!row.contains("s1"));
    }

    #[test]
    fn default_branch_does_not_render_pr_placeholders() {
        let config = test_config(Some("main"));
        let sessions = vec![test_session(
            "main",
            "clean",
            AgentState::Idle,
            PrCache::default(),
        )];
        let model = test_model(&config, &sessions, Some(0), PanelFocus::Worktrees, None);
        let frame = render_model_frame(&model, 120, 20);

        assert!(frame.contains("Default branch"));
        assert!(frame.contains("PR tracking disabled"));
        assert!(!frame.contains("Enter open"));
        assert!(!frame.contains("D delete"));
        assert!(!frame.contains("no-pr"));
        assert!(!frame.contains("C?"));
    }

    #[test]
    fn feature_worktree_footer_advertises_worktree_actions() {
        let config = test_config(Some("main"));
        let sessions = vec![test_session(
            "feature",
            "clean",
            AgentState::Idle,
            PrCache::default(),
        )];
        let model = test_model(&config, &sessions, Some(0), PanelFocus::Worktrees, None);
        let frame = crate::util::strip_ansi(&render_model_frame(&model, 140, 20));

        assert!(frame.contains("Enter open"));
        assert!(frame.contains("Space g P push"));
        assert!(frame.contains("D delete"));
    }

    #[test]
    fn configured_url_column_preserves_hyperlink_and_sanitizes_target() {
        let config = Config {
            default_agent: "opencode".to_string(),
            default_base: Some("main".to_string()),
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            escape_key: EscapeKey::EscEsc,
            merge_method: MergeMethod::Squash,
            checks: Checks::default(),
            worktree_columns: vec!["url".to_string()],
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            prompt_templates: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/user.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-repo-config.toml"),
        };
        let mut session = Session {
            repo_index: 0,
            repo_label: "repo".to_string(),
            repo_key: None,
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
        };
        session
            .wt_columns
            .insert("url".to_string(), "https://e.test/a".to_string());

        let label = configured_column_label(&config, &session);

        assert!(label.contains("\x1b]8;;https://e.test/a\x1b\\"));
        assert!(label.contains("\x1b]8;;\x1b\\"));

        let linked = format_column_value("url", "https://e.test/a\x1bb");
        assert!(linked.starts_with("\x1b]8;;https://e.test/ab\x1b\\"));
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
        let config = test_config(Some("main"));
        let sessions = vec![
            test_session("main", "clean", AgentState::Idle, PrCache::default()),
            test_session(
                "planned-work",
                "clean",
                AgentState::Idle,
                PrCache::default(),
            ),
            test_session("impl-work", "dirty 1", AgentState::Idle, PrCache::default()),
            test_session("pr-work", "clean", AgentState::Idle, test_pr(12, false)),
            test_session("merged-work", "clean", AgentState::Idle, test_pr(13, true)),
        ];
        let model = test_model(&config, &sessions, Some(2), PanelFocus::Repos, None);
        let frame = render_model_frame(&model, 160, 20);
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

    #[test]
    fn render_model_frame_fits_common_terminal_viewports() {
        let config = test_config(Some("main"));
        let sessions = vec![
            test_session("main", "clean", AgentState::Idle, PrCache::default()),
            test_session(
                "feature/very-long-branch-name-that-must-fit",
                "dirty 12 ahead 3",
                AgentState::Running,
                PrCache::default(),
            ),
            test_session(
                "review-work",
                "clean",
                AgentState::Idle,
                test_pr(1234, false),
            ),
            test_session(
                "merged-work",
                "clean",
                AgentState::Idle,
                test_pr(5678, true),
            ),
        ];

        for (cols, rows) in [
            (80, 24),
            (100, 30),
            (117, 30),
            (118, 30),
            (120, 30),
            (140, 40),
            (160, 40),
            (200, 60),
        ] {
            let model = test_model(&config, &sessions, Some(1), PanelFocus::Worktrees, None);
            let frame = render_model_frame(&model, cols, rows);
            let lines = frame.lines().collect::<Vec<_>>();

            assert_eq!(
                lines.len(),
                rows as usize,
                "{cols}x{rows} should render exactly one line per terminal row"
            );
            for (index, line) in lines.iter().enumerate() {
                assert_eq!(
                    visible_len(line),
                    cols as usize,
                    "{cols}x{rows} line {index} should fill the terminal width"
                );
            }
        }
    }

    #[test]
    fn render_model_frame_uses_repo_and_worktree_panels() {
        let config = test_config(Some("main"));
        let sessions = vec![
            test_session("main", "clean", AgentState::Idle, PrCache::default()),
            test_session(
                "feature",
                "dirty 1",
                AgentState::Running,
                PrCache::default(),
            ),
        ];
        let model = FrameModel {
            config: &config,
            sessions: &sessions,
            status: vec![StatusRow {
                label: "repos".to_string(),
                value: "1".to_string(),
                attention: false,
            }],
            repos: vec![RepoRow {
                label: "repo".to_string(),
                root: "/repo".to_string(),
                key: Some('1'),
                health: "D1 A1".to_string(),
                selected: true,
            }],
            worktrees: vec![
                test_worktree_row(&config, &sessions[0], 0, false),
                test_worktree_row(&config, &sessions[1], 1, true),
            ],
            current_repo_index: 0,
            selected_repo_label: "repo".to_string(),
            selected_repo_root: "/repo".to_string(),
            selected_session: Some(1),
            focus: PanelFocus::Worktrees,
            mode_label: "normal",
            status_message: None,
            repo_filter: "",
            worktree_filter: "",
            leader_hint: None,
        };

        let frame = render_model_frame(&model, 100, 24);
        let stripped = crate::util::strip_ansi(&frame);

        assert!(stripped.contains("1 Status"));
        assert!(stripped.contains("2 Repos"));
        assert!(stripped.contains("3 Worktrees / Sessions"));
        assert!(stripped.contains("feature"));
        for line in frame.lines() {
            assert_eq!(visible_len(line), 100);
        }
    }

    #[test]
    fn worktree_footer_omits_pr_actions_for_default_branch() {
        let config = test_config(Some("main"));
        let sessions = vec![test_session(
            "main",
            "clean",
            AgentState::Idle,
            PrCache::default(),
        )];
        let model = test_model(&config, &sessions, Some(0), PanelFocus::Worktrees, None);
        let frame = crate::util::strip_ansi(&render_model_frame(&model, 140, 24));

        assert!(!frame.contains("Enter open"));
        assert!(!frame.contains("push"));
        assert!(!frame.contains("merge"));
        assert!(!frame.contains("review"));
    }

    #[test]
    fn render_model_frame_places_panel_boundaries_from_viewport_width() {
        let config = test_config(Some("main"));
        let sessions = vec![
            test_session("main", "clean", AgentState::Idle, PrCache::default()),
            test_session("review-work", "clean", AgentState::Idle, test_pr(12, false)),
        ];
        let model = test_model(&config, &sessions, Some(1), PanelFocus::Worktrees, None);
        let frame = render_model_frame(&model, 160, 30);
        let separator = crate::util::strip_ansi(frame.lines().nth(1).unwrap_or_default());
        let chars = separator.chars().collect::<Vec<_>>();

        assert_eq!(chars[22], '+');
        assert_eq!(chars[51], '+');
        assert_eq!(chars[102], '+');
        assert_eq!(chars.len(), 160);
    }

    fn test_config(default_base: Option<&str>) -> Config {
        Config {
            default_agent: "opencode".to_string(),
            default_base: default_base.map(str::to_string),
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            escape_key: EscapeKey::EscEsc,
            merge_method: MergeMethod::Squash,
            checks: Checks::default(),
            worktree_columns: Vec::new(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            prompt_templates: BTreeMap::new(),
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
            repo_index: 0,
            repo_label: "repo".to_string(),
            repo_key: None,
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
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        }
    }

    fn test_model<'a>(
        config: &'a Config,
        sessions: &'a [Session],
        selected_session: Option<usize>,
        focus: PanelFocus,
        status_message: Option<&'a str>,
    ) -> FrameModel<'a> {
        FrameModel {
            config,
            sessions,
            status: vec![StatusRow {
                label: "repos".to_string(),
                value: "1".to_string(),
                attention: false,
            }],
            repos: vec![RepoRow {
                label: "repo".to_string(),
                root: "/repo".to_string(),
                key: Some('1'),
                health: "ok".to_string(),
                selected: true,
            }],
            worktrees: sessions
                .iter()
                .enumerate()
                .map(|(index, session)| {
                    test_worktree_row(config, session, index, Some(index) == selected_session)
                })
                .collect(),
            current_repo_index: 0,
            selected_repo_label: "repo".to_string(),
            selected_repo_root: "/repo".to_string(),
            selected_session,
            focus,
            mode_label: "normal",
            status_message,
            repo_filter: "",
            worktree_filter: "",
            leader_hint: None,
        }
    }

    fn test_worktree_row(
        config: &Config,
        session: &Session,
        session_index: usize,
        selected: bool,
    ) -> WorktreeRow {
        WorktreeRow {
            session_index,
            repo_root: "/repo".to_string(),
            worktree_path: session.path_display.clone(),
            branch: session.branch.clone(),
            kind: if config.is_default_branch(&session.branch) {
                WorktreeKind::DefaultBranch
            } else if session.branch == "(detached)" {
                WorktreeKind::Detached
            } else {
                WorktreeKind::FeatureWorktree
            },
            adopted: session.adopted,
            status_label: session.status_label.clone(),
            agent_state: session.agent_state,
            pr: session.pr.clone(),
            wt_columns: session.wt_columns.clone(),
            unseen_comments: session.unseen_comments,
            prompt_summary: session.prompt_summary.clone(),
            selected,
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
                requested_reviewers: Vec::new(),
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
