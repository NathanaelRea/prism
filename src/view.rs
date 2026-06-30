use std::collections::{BTreeMap, BTreeSet};

use crate::agent::AgentState;
use crate::auto_flow::{
    AutoImplementationSource, AutoOutputKind, AutoOutputLine, AutoRunMode, AutoRunStatus,
    AutoStepKey, AutoStepRun, AutoStepStatus, PersistedAutoRun,
};
use crate::config::Config;
use crate::github::{PrCache, pr_cache_has_comments};
use crate::opencode::OpencodeStatus;
use crate::plan_run::{
    PersistedPlanRun, PlanOutputKind, PlanOutputLine, PlanRunMode, PlanRunStatus, PlanStepRun,
    PlanStepStatus, plan_output_block_key,
};
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
    pub repo_main_view: RepoMainView,
    pub mode_label: &'a str,
    pub status_message: Option<&'a str>,
    pub repo_filter: &'a str,
    pub worktree_filter: &'a str,
    pub leader_hint: Option<&'a str>,
    pub auto_dashboard: Option<AutoDashboard>,
    pub plan_dashboard: Option<PlanDashboard>,
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
    pub agent_state: AgentState,
    pub pr: PrCache,
    pub wt_columns: BTreeMap<String, String>,
    pub unseen_comments: bool,
    pub prompt_summary: String,
    pub selected: bool,
}

pub(crate) struct PlanDashboard {
    pub run: PersistedPlanRun,
    pub output_lines: Vec<PlanOutputLine>,
    pub output_state: PlanOutputViewerState,
}

pub(crate) struct AutoDashboard {
    pub run: PersistedAutoRun,
    pub linked_plan_dashboard: Option<PlanDashboard>,
    pub output_lines: Vec<AutoOutputLine>,
    pub output_state: AutoOutputViewerState,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AutoOutputViewerState {
    pub cursor: usize,
    pub follow: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PlanOutputViewerState {
    pub cursor: usize,
    pub follow: bool,
    pub expanded_blocks: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RepoMainView {
    Github,
    Kanban,
}

impl RepoMainView {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Kanban => "kanban",
        }
    }
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
    let sidebar_target = if cols >= 160 {
        50
    } else if cols >= 120 {
        44
    } else {
        38
    };
    let min_sidebar_width = 14;
    let min_main_width = 10;
    let max_sidebar_width = cols.saturating_sub(min_main_width + 1);
    let sidebar_width = sidebar_target
        .min(max_sidebar_width)
        .max(min_sidebar_width.min(max_sidebar_width));
    let main_width = cols.saturating_sub(sidebar_width + 1);
    let panel_rows = rows.saturating_sub(1) as usize;
    let sidebar_content_rows = panel_rows.saturating_sub(9);
    let status_rows = sidebar_content_rows.min(4);
    let remaining_sidebar_rows = sidebar_content_rows.saturating_sub(status_rows);
    let repo_rows = remaining_sidebar_rows.saturating_mul(2) / 5;
    let worktree_rows = remaining_sidebar_rows.saturating_sub(repo_rows);
    let main_rows = panel_rows.saturating_sub(3);
    let repo_selected = model.repos.iter().position(|row| row.selected).unwrap_or(0);
    let repo_start = scroll_start(repo_selected, repo_rows);
    let worktree_selected = model
        .worktrees
        .iter()
        .position(|row| row.selected)
        .unwrap_or(0);
    let worktree_start = scroll_start(worktree_selected, worktree_rows);
    let main_lines = if let Some(dashboard) = &model.auto_dashboard {
        format_auto_dashboard_lines(dashboard, main_width as usize, main_rows)
    } else if let Some(dashboard) = &model.plan_dashboard {
        format_plan_dashboard_lines(dashboard, main_width as usize, main_rows)
    } else {
        match model.focus {
            PanelFocus::Status => format_status_dashboard_lines(model, main_width as usize),
            PanelFocus::Repos => format_repo_overview_lines(model, main_width as usize, main_rows),
            PanelFocus::Worktrees => format_worktree_detail_lines(model, main_width as usize),
        }
    };
    let sidebar_lines = format_sidebar_lines(
        model,
        SidebarLayout {
            width: sidebar_width as usize,
            total_rows: panel_rows,
            status_rows,
            repo_rows,
            worktree_rows,
            repo_start,
            worktree_start,
        },
    );
    let mut frame = String::from("\x1b[?25l\x1b[H");
    for row in 0..panel_rows {
        let sidebar = sidebar_lines
            .get(row)
            .cloned()
            .unwrap_or_else(|| " ".repeat(sidebar_width as usize));
        let main = if row == 0 {
            styled_cell("Main", main_width as usize, "1;36")
        } else if row == 1 || row + 1 == panel_rows {
            "-".repeat(main_width as usize)
        } else {
            ansi_cell(
                main_lines
                    .get(row - 2)
                    .map(String::as_str)
                    .unwrap_or_default(),
                main_width as usize,
            )
        };
        push_line(&mut frame, cols, format!("{sidebar}|{main}"));
    }
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

struct SidebarLayout {
    width: usize,
    total_rows: usize,
    status_rows: usize,
    repo_rows: usize,
    worktree_rows: usize,
    repo_start: usize,
    worktree_start: usize,
}

fn format_sidebar_lines(model: &FrameModel<'_>, layout: SidebarLayout) -> Vec<String> {
    let mut lines = Vec::with_capacity(layout.total_rows);

    append_sidebar_section(
        &mut lines,
        "1 Status",
        model.focus == PanelFocus::Status,
        layout.width,
        (0..layout.status_rows).map(|row| {
            model
                .status
                .get(row)
                .map(|status| format_status_row(status, layout.width))
                .unwrap_or_else(|| " ".repeat(layout.width))
        }),
    );
    append_sidebar_section(
        &mut lines,
        "2 Repos",
        model.focus == PanelFocus::Repos,
        layout.width,
        (0..layout.repo_rows).map(|row| {
            model
                .repos
                .get(layout.repo_start + row)
                .map(|repo| format_repo_row(repo, layout.width))
                .unwrap_or_else(|| {
                    empty_state_cell(
                        row,
                        model.repos.is_empty(),
                        if model.repo_filter.trim().is_empty() {
                            "No repos"
                        } else {
                            "No matches"
                        },
                        layout.width,
                    )
                })
        }),
    );
    append_sidebar_section(
        &mut lines,
        "3 Worktrees / Sessions",
        model.focus == PanelFocus::Worktrees,
        layout.width,
        (0..layout.worktree_rows).map(|row| {
            model
                .worktrees
                .get(layout.worktree_start + row)
                .map(|worktree| format_worktree_row(model.config, worktree, layout.width))
                .unwrap_or_else(|| {
                    empty_state_cell(
                        row,
                        model.worktrees.is_empty(),
                        if model.worktree_filter.trim().is_empty() {
                            "No worktrees"
                        } else {
                            "No matches"
                        },
                        layout.width,
                    )
                })
        }),
    );

    lines.truncate(layout.total_rows);
    while lines.len() < layout.total_rows {
        lines.push(" ".repeat(layout.width));
    }
    lines
}

fn append_sidebar_section(
    lines: &mut Vec<String>,
    title: &str,
    focused: bool,
    width: usize,
    rows: impl IntoIterator<Item = String>,
) {
    lines.push(panel_title(title, focused, width));
    lines.push("-".repeat(width));
    lines.extend(rows);
    lines.push("-".repeat(width));
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
            "j/k output  h/l phase  u pause/resume  f retry  B retry from  s skip  D dismiss  x abort  ? help"
                .to_string()
        }
        PanelFocus::Repos => {
            let mut actions = Vec::new();
            if !model.worktrees.is_empty() {
                actions.push("Enter focus worktrees");
            }
            actions.extend([
                "c create",
                "p pull",
                "P plan",
                "h/l view",
                "<Space> for more options",
                "R repos",
                "/ search",
            ]);
            actions.join("  ")
        }
        PanelFocus::Worktrees => {
            let Some(row) = model.worktrees.iter().find(|row| row.selected) else {
                return "c create  / search".to_string();
            };
            let mut actions = vec!["<Space> for more options"];
            if row.kind != WorktreeKind::DefaultBranch {
                actions.insert(0, "Enter open");
            }
            if row.kind != WorktreeKind::DefaultBranch {
                actions.push("D delete");
            }
            actions.push("p pull");
            actions.push("P plan");
            actions.push("A Auto Flow");
            actions.push("c create");
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
    let status_icons = worktree_status_icons(config, row);
    let name = if status_icons.is_empty() {
        styled_cell(&row.branch, 22, branch_code)
    } else {
        format!(
            "{} {}",
            status_icons,
            styled_cell(&row.branch, 22, branch_code)
        )
    };
    let extra = configured_column_label_for_values(config, &row.wt_columns);
    let text = format!(
        "{} {} {} {} {} {}",
        marker,
        styled_cell(&row_number, 3, "90"),
        name,
        styled_cell(
            agent_icon(row.agent_state),
            3,
            agent_state_color(row.agent_state)
        ),
        extra,
        truncate_line(summary, 50),
    );
    ansi_cell(&text, width)
}

fn format_status_dashboard_lines(model: &FrameModel<'_>, width: usize) -> Vec<String> {
    let logo_color = "38;2;0;255;255";
    let mut lines = vec![
        color("░▒▓█▓▒░ P ◤◥◣◢◤◥◣", logo_color),
        color("▒▓█▓▒░▒ R ◥◣◢◤◥◣◢", logo_color),
        color("▓█▓▒░▒▓ I ◣◢◤◥◣◢◤", logo_color),
        color("█▓▒░▒▓█ S ◢◤◥◣◢◤◥", logo_color),
        color("▓▒░▒▓█▓ M ◤◥◣◢◤◥◣", logo_color),
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
        "Tab cycles focus; repos h/l switches views".to_string(),
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
        color(&model.selected_repo_label, "1;36"),
        truncate_line(&model.selected_repo_root, width),
    ];
    if let Some(row) = model.repos.iter().find(|row| row.selected) {
        lines.push(format!("health {}", row.health));
    }
    lines.push(format!(
        "view {}  prs {}  review needed {}  ci failed {}  local {}",
        model.repo_main_view.label(),
        summary.open_prs,
        summary.review_needed,
        summary.ci_failed,
        summary.local_branches,
    ));
    lines.push(String::new());
    let remaining_rows = visible_rows.saturating_sub(lines.len());
    match model.repo_main_view {
        RepoMainView::Github => lines.extend(format_repo_github_panel_lines(
            model.config,
            model.sessions,
            &indices,
            model.selected_session,
            width,
            remaining_rows,
        )),
        RepoMainView::Kanban => lines.extend(format_kanban_panel_lines(
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
struct RepoGithubSummary {
    open_prs: usize,
    review_needed: usize,
    ci_failed: usize,
    local_branches: usize,
}

fn repo_github_summary(
    config: &Config,
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
    if let Some(status) = &session.opencode_status {
        lines.extend(format_opencode_status_lines(status, width));
    }
    lines.push(String::new());
    lines.extend(format_pr_panel_lines(model.config, Some(session)));
    lines
}

fn format_opencode_status_lines(status: &OpencodeStatus, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let session = status.session_id.as_deref().map(short_id).unwrap_or("none");
    let title = status.title.as_deref().filter(|title| !title.is_empty());
    lines.push(match title {
        Some(title) => format!(
            "opencode {}  session {}  {}",
            status.state.label(),
            session,
            truncate_line(title, width)
        ),
        None => format!("opencode {}  session {}", status.state.label(), session),
    });
    if let Some(tool) = &status.active_tool {
        lines.push(format!("tool {}", truncate_line(tool, width)));
    }
    if let Some(message) = &status.latest_message {
        lines.push(format!("latest {}", truncate_line(message, width)));
    }
    let todo = todo_summary(&status.todos);
    if !todo.is_empty() {
        lines.push(format!("todos {todo}"));
    }
    lines
}

fn format_auto_dashboard_lines(
    dashboard: &AutoDashboard,
    width: usize,
    visible_rows: usize,
) -> Vec<String> {
    let run = &dashboard.run.run;
    let selected_step = run
        .selected_step_run_id
        .and_then(|id| dashboard.run.steps.iter().find(|step| step.id == Some(id)))
        .or_else(|| dashboard.run.steps.first());
    let counts = dashboard.run.status_counts();
    let mut lines = vec![
        color("Auto Flow", "1;36"),
        format!("task   {}", truncate_line(&run.prompt_summary, width)),
        format!(
            "work   {}",
            truncate_line(&run.worktree_path.display().to_string(), width)
        ),
        format!(
            "mode   {}  status {}  elapsed {}",
            auto_mode_label(run.mode),
            auto_run_status_label(run.status),
            elapsed_label(run.created_unix_ms, run.updated_unix_ms)
        ),
        format!(
            "source {}{}",
            auto_source_label(run.implementation_source),
            run.plan_path
                .as_ref()
                .map(|path| format!(
                    "  plan {}",
                    truncate_line(&path.display().to_string(), width)
                ))
                .unwrap_or_default()
        ),
        format!("branch {}", truncate_line(&run.branch, width)),
    ];
    if let Some(pr_number) = run.pr_number {
        lines.push(format!("pr     #{pr_number}"));
    }
    if let Some(step) = selected_step {
        lines.push(format!(
            "step   #{} {} attempt {} {}",
            step.sequence,
            step.step_key.as_str(),
            step.attempt,
            auto_step_status_label(step.status)
        ));
        if let Some(session_id) = step.opencode_session_id.as_deref() {
            lines.push(format!("opencode session {}", short_id(session_id)));
        }
        if let Some(summary) = step.summary.as_deref().or(step.reason.as_deref()) {
            lines.push(format!("latest {}", truncate_line(summary, width)));
        }
        if let Some(error) = step.error.as_deref() {
            lines.push(color(
                &format!("error  {}", truncate_line(error, width)),
                "1;31",
            ));
        }
    }
    lines.push(format!(
        "counts queued {}  running {}  waiting {}  done {}  failed {}",
        counts.queued + counts.starting,
        counts.running,
        counts.waiting,
        counts.done,
        counts.failed
    ));
    lines.push(String::new());
    lines.push(color("Steps", "1;37"));
    let linked_plan_rows_reserved = linked_plan_summary_lines(dashboard, width).len();
    let output_rows_reserved = dashboard.output_lines.len().min(8) + linked_plan_rows_reserved + 2;
    let step_rows_available = visible_rows
        .saturating_sub(lines.len())
        .saturating_sub(output_rows_reserved)
        .max(3);
    let selected_index = selected_step
        .and_then(|selected| {
            dashboard
                .run
                .steps
                .iter()
                .position(|step| step.id == selected.id)
        })
        .unwrap_or(0);
    let start = scroll_start(selected_index, step_rows_available);
    for step in dashboard
        .run
        .steps
        .iter()
        .skip(start)
        .take(step_rows_available)
    {
        lines.push(format_auto_step_row(step, run.selected_step_run_id, width));
    }
    lines.push(String::new());
    lines.push(color("Output", "1;37"));
    lines.extend(linked_plan_summary_lines(dashboard, width));
    if dashboard.output_lines.is_empty() {
        lines.push(color("No output yet", "90"));
    } else {
        let output_rows_available = visible_rows.saturating_sub(lines.len()).max(1);
        let cursor = dashboard
            .output_state
            .cursor
            .min(dashboard.output_lines.len().saturating_sub(1));
        let start = if dashboard.output_state.follow {
            dashboard
                .output_lines
                .len()
                .saturating_sub(output_rows_available)
        } else {
            scroll_start(cursor, output_rows_available)
        };
        for (index, line) in dashboard
            .output_lines
            .iter()
            .enumerate()
            .skip(start)
            .take(output_rows_available)
        {
            lines.push(format_auto_output_row(line, index == cursor, width));
        }
    }
    lines.truncate(visible_rows);
    lines
}

fn linked_plan_summary_lines(dashboard: &AutoDashboard, width: usize) -> Vec<String> {
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
            return vec![color("linked plan unavailable", "90")];
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
    let mut lines = vec![format!(
        "linked plan {}  status {}  mode {}",
        truncate_line(&plan_run.plan_display, width),
        plan_run_status_label(plan_run.status),
        plan_mode_label(plan_run.mode)
    )];
    if let Some(phase) = selected_phase {
        lines.push(format!(
            "phase {}/{} {}{}",
            phase.step,
            plan_run.total_steps,
            plan_step_status_label(phase.status),
            phase
                .latest_message
                .as_ref()
                .map(|message| format!("  {}", truncate_line(message, width)))
                .unwrap_or_default()
        ));
        if let Some(error) = phase.error.as_deref() {
            lines.push(color(
                &format!("plan error {}", truncate_line(error, width)),
                "1;31",
            ));
        }
    }
    if let Some(line) = plan_dashboard.output_lines.last() {
        lines.push(format!(
            "plan output {}",
            truncate_line(&line.text, width.saturating_sub(12))
        ));
    }
    lines.push(String::new());
    lines
}

fn format_auto_step_row(
    step: &AutoStepRun,
    selected_step_run_id: Option<i64>,
    width: usize,
) -> String {
    let marker = if step.id == selected_step_run_id {
        color("▶", "1;36")
    } else {
        " ".to_string()
    };
    let label = format!(
        "{} #{} {}",
        marker,
        step.sequence,
        auto_step_status_label(step.status)
    );
    let detail = step
        .summary
        .as_deref()
        .or(step.reason.as_deref())
        .or(step.error.as_deref())
        .unwrap_or("");
    let text = format!(
        "{} {} {}",
        styled_cell(&label, 24, auto_step_status_color(step.status)),
        styled_cell(step.step_key.as_str(), 20, "37"),
        truncate_line(detail, width.saturating_sub(46))
    );
    ansi_cell(&text, width)
}

fn format_auto_output_row(line: &AutoOutputLine, selected: bool, width: usize) -> String {
    let marker = if selected {
        color("▶", "1;36")
    } else {
        " ".to_string()
    };
    let kind = auto_output_kind_label(line.kind);
    let prefix = format!(
        "{} {}",
        marker,
        styled_cell(kind, 10, auto_output_kind_color(line.kind))
    );
    ansi_cell(
        &format!(
            "{prefix} {}",
            truncate_line(&line.text, width.saturating_sub(14))
        ),
        width,
    )
}

fn format_plan_dashboard_lines(
    dashboard: &PlanDashboard,
    width: usize,
    visible_rows: usize,
) -> Vec<String> {
    let run = &dashboard.run.run;
    let selected_step = dashboard
        .run
        .steps
        .iter()
        .find(|step| step.step == run.selected_step)
        .or_else(|| dashboard.run.steps.first());
    let counts = dashboard.run.status_counts();
    let mut lines = vec![
        color("Plan Run", "1;36"),
        format!("plan   {}", truncate_line(&run.plan_display, width)),
        format!(
            "scope  {}",
            truncate_line(&run.scope_path.display().to_string(), width)
        ),
        format!(
            "mode   {}  status {}  elapsed {}",
            plan_mode_label(run.mode),
            plan_run_status_label(run.status),
            elapsed_label(run.created_unix_ms, run.updated_unix_ms)
        ),
    ];
    if let Some(step) = selected_step {
        lines.push(format!(
            "phase  {}/{} {}",
            step.step,
            run.total_steps,
            plan_step_status_label(step.status)
        ));
        if let Some(session_id) = step.opencode_session_id.as_deref() {
            lines.push(format!("opencode session {}", short_id(session_id)));
        }
        if let Some(tool) = step.active_tool.as_deref() {
            lines.push(format!("tool   {}", truncate_line(tool, width)));
        }
        if let Some(message) = step.latest_message.as_deref() {
            lines.push(format!("latest {}", truncate_line(message, width)));
        }
        let todos = plan_todo_summary(step);
        if !todos.is_empty() {
            lines.push(format!("todos  {todos}"));
        }
        if let Some(error) = step.error.as_deref() {
            lines.push(color(
                &format!("error  {}", truncate_line(error, width)),
                "1;31",
            ));
        }
    }
    lines.push(format!(
        "counts queued {}  running {}  done {}  failed {}",
        counts.queued + counts.starting,
        counts.running,
        counts.done,
        counts.failed
    ));
    lines.push(String::new());
    lines.push(color("Phases", "1;37"));
    let rendered_output = render_plan_output_rows(dashboard, width);
    let output_rows_reserved = rendered_output.len().min(8) + 2;
    let phase_rows_available = visible_rows
        .saturating_sub(lines.len())
        .saturating_sub(output_rows_reserved)
        .max(3);
    let selected_index = dashboard
        .run
        .steps
        .iter()
        .position(|step| step.step == run.selected_step)
        .unwrap_or(0);
    let start = scroll_start(selected_index, phase_rows_available);
    for step in dashboard
        .run
        .steps
        .iter()
        .skip(start)
        .take(phase_rows_available)
    {
        lines.push(format_plan_step_row(
            step,
            run.selected_step,
            run.total_steps,
            width,
        ));
    }
    lines.push(String::new());
    lines.push(color("Output", "1;37"));
    if rendered_output.is_empty() {
        lines.push(color("No output yet", "90"));
    } else {
        let output_rows_available = visible_rows.saturating_sub(lines.len()).max(1);
        let cursor = selected_rendered_output_index(dashboard, &rendered_output);
        let start = if dashboard.output_state.follow {
            rendered_output.len().saturating_sub(output_rows_available)
        } else {
            scroll_start(cursor, output_rows_available)
        };
        for (index, row) in rendered_output
            .iter()
            .enumerate()
            .skip(start)
            .take(output_rows_available)
        {
            lines.push(format_plan_rendered_output_row(row, index == cursor, width));
        }
    }
    lines.truncate(visible_rows);
    lines
}

fn format_plan_step_row(
    step: &PlanStepRun,
    selected_step: usize,
    total_steps: usize,
    width: usize,
) -> String {
    let marker = if step.step == selected_step {
        color("▶", "1;36")
    } else {
        " ".to_string()
    };
    let label = format!(
        "{} {}/{} {}",
        marker,
        step.step,
        total_steps,
        plan_step_status_label(step.status)
    );
    let detail = step
        .active_tool
        .as_deref()
        .or(step.latest_message.as_deref())
        .or(step.error.as_deref())
        .unwrap_or("");
    let text = format!(
        "{} {} {}",
        styled_cell(&label, 24, plan_step_status_color(step.status)),
        styled_cell(&elapsed_step_label(step), 8, "90"),
        truncate_line(detail, width.saturating_sub(34))
    );
    ansi_cell(&text, width)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RenderedPlanOutputRow {
    line_number: u64,
    kind: PlanOutputKind,
    text: String,
    collapsed: bool,
    block_key: Option<String>,
}

fn render_plan_output_rows(dashboard: &PlanDashboard, width: usize) -> Vec<RenderedPlanOutputRow> {
    let mut rows = Vec::new();
    let mut index = 0;
    while index < dashboard.output_lines.len() {
        let line = &dashboard.output_lines[index];
        if let Some(block_key) = plan_output_block_key(line) {
            let block_len = block_len_at(&dashboard.output_lines, index, &block_key);
            let collapsed = !dashboard.output_state.expanded_blocks.contains(&block_key);
            if collapsed {
                rows.push(RenderedPlanOutputRow {
                    line_number: line.line_number,
                    kind: line.kind,
                    text: collapsed_block_summary(
                        &dashboard.output_lines[index..index + block_len],
                        width,
                    ),
                    collapsed: true,
                    block_key: Some(block_key),
                });
                index += block_len;
                continue;
            }
        }

        for text in output_display_lines(line, width) {
            rows.push(RenderedPlanOutputRow {
                line_number: line.line_number,
                kind: line.kind,
                text,
                collapsed: false,
                block_key: plan_output_block_key(line),
            });
        }
        index += 1;
    }
    rows
}

fn block_len_at(lines: &[PlanOutputLine], index: usize, block_key: &str) -> usize {
    let mut len = 0;
    for line in &lines[index..] {
        if plan_output_block_key(line).as_deref() != Some(block_key) {
            break;
        }
        len += 1;
    }
    len.max(1)
}

fn selected_rendered_output_index(
    dashboard: &PlanDashboard,
    rendered_output: &[RenderedPlanOutputRow],
) -> usize {
    let Some(selected_line) = dashboard.output_lines.get(
        dashboard
            .output_state
            .cursor
            .min(dashboard.output_lines.len().saturating_sub(1)),
    ) else {
        return 0;
    };
    let selected_block_key = plan_output_block_key(selected_line);
    if let Some(block_key) = selected_block_key.as_deref()
        && let Some(index) = rendered_output
            .iter()
            .position(|row| row.collapsed && row.block_key.as_deref() == Some(block_key))
    {
        return index;
    }
    rendered_output
        .iter()
        .position(|row| row.line_number == selected_line.line_number)
        .or_else(|| {
            selected_block_key.as_deref().and_then(|block_key| {
                rendered_output
                    .iter()
                    .position(|row| row.block_key.as_deref() == Some(block_key))
            })
        })
        .unwrap_or_else(|| rendered_output.len().saturating_sub(1))
}

fn collapsed_block_summary(lines: &[PlanOutputLine], width: usize) -> String {
    let Some(first) = lines.first() else {
        return String::new();
    };
    let line_count = lines
        .iter()
        .map(|line| line.text.lines().count().max(1))
        .sum::<usize>();
    let text = first.text.lines().next().unwrap_or("").replace('\n', " ");
    truncate_line(
        &format!("[+] L{} {} lines  {}", first.line_number, line_count, text),
        width,
    )
}

fn output_display_lines(line: &PlanOutputLine, width: usize) -> Vec<String> {
    let rows = line.text.lines().collect::<Vec<_>>();
    if rows.is_empty() {
        return vec![String::new()];
    }
    rows.into_iter()
        .map(|text| truncate_line(text, width))
        .collect()
}

fn format_plan_rendered_output_row(
    row: &RenderedPlanOutputRow,
    selected: bool,
    width: usize,
) -> String {
    let marker = if selected { ">" } else { " " };
    let fold = if row.block_key.is_some() {
        if row.collapsed { "[+]" } else { "[-]" }
    } else {
        "   "
    };
    let kind = plan_output_kind_label(row.kind);
    let text = truncate_line(&row.text, width.saturating_sub(22));
    let text = color_diff_output(row.kind, &text);
    ansi_cell(
        &format!(
            "{} {} L{:<4} {} {}",
            color(marker, if selected { "1;36" } else { "90" }),
            fold,
            row.line_number,
            styled_cell(kind, 10, plan_output_kind_color(row.kind)),
            text
        ),
        width,
    )
}

fn color_diff_output(kind: PlanOutputKind, text: &str) -> String {
    if kind != PlanOutputKind::Diff {
        return text.to_string();
    }
    if text.starts_with("+++") || text.starts_with("---") || text.starts_with("@@") {
        color(text, "1;36")
    } else if text.starts_with('+') {
        color(text, "32")
    } else if text.starts_with('-') {
        color(text, "31")
    } else {
        text.to_string()
    }
}

fn plan_mode_label(mode: PlanRunMode) -> &'static str {
    match mode {
        PlanRunMode::Sequential => "sequential",
        PlanRunMode::Parallel => "parallel",
    }
}

fn auto_mode_label(mode: AutoRunMode) -> &'static str {
    match mode {
        AutoRunMode::Standard => "standard",
        AutoRunMode::PlanFirst => "plan_first",
    }
}

fn auto_source_label(source: AutoImplementationSource) -> &'static str {
    match source {
        AutoImplementationSource::Prompt => "prompt",
        AutoImplementationSource::ExistingPlan => "plan file",
        AutoImplementationSource::DraftPlan => "draft plan",
    }
}

fn auto_run_status_label(status: AutoRunStatus) -> &'static str {
    match status {
        AutoRunStatus::Queued => "queued",
        AutoRunStatus::Running => "running",
        AutoRunStatus::Paused => "paused",
        AutoRunStatus::Done => "done",
        AutoRunStatus::Failed => "failed",
        AutoRunStatus::Aborted => "aborted",
    }
}

fn auto_step_status_label(status: AutoStepStatus) -> &'static str {
    match status {
        AutoStepStatus::Queued => "queued",
        AutoStepStatus::Starting => "starting",
        AutoStepStatus::Running => "running",
        AutoStepStatus::Waiting => "waiting",
        AutoStepStatus::Done => "done",
        AutoStepStatus::Failed => "failed",
        AutoStepStatus::Aborted => "aborted",
        AutoStepStatus::Skipped => "skipped",
    }
}

fn auto_step_status_color(status: AutoStepStatus) -> &'static str {
    match status {
        AutoStepStatus::Done => "32",
        AutoStepStatus::Failed | AutoStepStatus::Aborted => "1;31",
        AutoStepStatus::Running | AutoStepStatus::Starting | AutoStepStatus::Waiting => "1;33",
        AutoStepStatus::Queued | AutoStepStatus::Skipped => "37",
    }
}

fn auto_output_kind_label(kind: AutoOutputKind) -> &'static str {
    match kind {
        AutoOutputKind::Assistant => "assistant",
        AutoOutputKind::Tool => "tool",
        AutoOutputKind::ToolOutput => "tool out",
        AutoOutputKind::Diff => "diff",
        AutoOutputKind::Status => "status",
        AutoOutputKind::System => "system",
        AutoOutputKind::Error => "error",
        AutoOutputKind::RawJson => "json",
    }
}

fn auto_output_kind_color(kind: AutoOutputKind) -> &'static str {
    match kind {
        AutoOutputKind::Assistant => "37",
        AutoOutputKind::Tool | AutoOutputKind::ToolOutput => "33",
        AutoOutputKind::Diff => "36",
        AutoOutputKind::Error => "1;31",
        AutoOutputKind::Status | AutoOutputKind::System | AutoOutputKind::RawJson => "90",
    }
}

fn plan_run_status_label(status: PlanRunStatus) -> &'static str {
    match status {
        PlanRunStatus::Draft => "draft",
        PlanRunStatus::Queued => "queued",
        PlanRunStatus::Running => "running",
        PlanRunStatus::Paused => "paused",
        PlanRunStatus::Done => "done",
        PlanRunStatus::Failed => "failed",
        PlanRunStatus::Aborted => "aborted",
    }
}

fn plan_step_status_label(status: PlanStepStatus) -> &'static str {
    match status {
        PlanStepStatus::Queued => "queued",
        PlanStepStatus::Starting => "starting",
        PlanStepStatus::Running => "running",
        PlanStepStatus::Done => "done",
        PlanStepStatus::Failed => "failed",
        PlanStepStatus::Aborted => "aborted",
        PlanStepStatus::Skipped => "skipped",
    }
}

fn plan_step_status_color(status: PlanStepStatus) -> &'static str {
    match status {
        PlanStepStatus::Done => "32",
        PlanStepStatus::Failed | PlanStepStatus::Aborted => "1;31",
        PlanStepStatus::Running | PlanStepStatus::Starting => "1;33",
        PlanStepStatus::Queued | PlanStepStatus::Skipped => "37",
    }
}

fn plan_output_kind_label(kind: PlanOutputKind) -> &'static str {
    match kind {
        PlanOutputKind::Assistant => "assistant",
        PlanOutputKind::Tool => "tool",
        PlanOutputKind::ToolOutput => "tool out",
        PlanOutputKind::Diff => "diff",
        PlanOutputKind::Todo => "todo",
        PlanOutputKind::Status => "status",
        PlanOutputKind::RawJson => "json",
        PlanOutputKind::System => "system",
        PlanOutputKind::Error => "error",
    }
}

fn plan_output_kind_color(kind: PlanOutputKind) -> &'static str {
    match kind {
        PlanOutputKind::Assistant => "37",
        PlanOutputKind::Tool | PlanOutputKind::ToolOutput => "33",
        PlanOutputKind::Diff => "36",
        PlanOutputKind::Todo => "35",
        PlanOutputKind::Error => "1;31",
        PlanOutputKind::Status | PlanOutputKind::RawJson | PlanOutputKind::System => "90",
    }
}

fn plan_todo_summary(step: &PlanStepRun) -> String {
    let mut pending = 0;
    let mut active = 0;
    let mut done = 0;
    for todo in &step.todos {
        match todo.status.as_str() {
            "completed" | "complete" | "done" => done += 1,
            "in_progress" | "in-progress" | "active" | "running" => active += 1,
            _ => pending += 1,
        }
    }
    let mut parts = Vec::new();
    if pending > 0 {
        parts.push(format!("pending {pending}"));
    }
    if active > 0 {
        parts.push(format!("active {active}"));
    }
    if done > 0 {
        parts.push(format!("done {done}"));
    }
    parts.join("  ")
}

fn elapsed_step_label(step: &PlanStepRun) -> String {
    match (step.started_unix_ms, step.finished_unix_ms) {
        (Some(start), Some(end)) => elapsed_label(start, end),
        (Some(start), None) => elapsed_label(start, now_unix_ms()),
        _ => String::new(),
    }
}

fn elapsed_label(start_unix_ms: u64, end_unix_ms: u64) -> String {
    let total_seconds = end_unix_ms.saturating_sub(start_unix_ms) / 1000;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{minutes:02}:{seconds:02}")
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn todo_summary(todos: &[crate::opencode::OpencodeTodo]) -> String {
    let mut pending = 0;
    let mut active = 0;
    let mut completed = 0;
    for todo in todos {
        match todo.status.as_str() {
            "completed" | "complete" | "done" => completed += 1,
            "in_progress" | "in-progress" | "active" | "running" => active += 1,
            _ => pending += 1,
        }
    }
    let mut parts = Vec::new();
    if pending > 0 {
        parts.push(format!("pending {pending}"));
    }
    if active > 0 {
        parts.push(format!("active {active}"));
    }
    if completed > 0 {
        parts.push(format!("done {completed}"));
    }
    parts.join("  ")
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

fn format_repo_github_panel_lines(
    config: &Config,
    sessions: &[Session],
    session_indices: &[usize],
    selected: Option<usize>,
    width: usize,
    visible_rows: usize,
) -> Vec<String> {
    if width >= 96 && visible_rows >= 8 {
        let left_width = (width * 58 / 100).max(42).min(width.saturating_sub(36));
        let right_width = width.saturating_sub(left_width + 1);
        let left = format_repo_work_list_lines(
            config,
            sessions,
            session_indices,
            selected,
            left_width,
            visible_rows,
        );
        let preview = selected.and_then(|index| sessions.get(index));
        let right = format_repo_preview_lines(config, preview, right_width);
        let row_count = left.len().max(right.len()).min(visible_rows);
        return (0..row_count)
            .map(|row| {
                format!(
                    "{} {}",
                    ansi_cell(
                        left.get(row).map(String::as_str).unwrap_or_default(),
                        left_width
                    ),
                    ansi_cell(
                        right.get(row).map(String::as_str).unwrap_or_default(),
                        right_width
                    ),
                )
            })
            .collect();
    }

    let mut lines = format_repo_work_list_lines(
        config,
        sessions,
        session_indices,
        selected,
        width,
        visible_rows,
    );
    if lines.len() < visible_rows {
        lines.push(String::new());
    }
    let preview = selected.and_then(|index| sessions.get(index));
    lines.extend(format_repo_preview_lines(config, preview, width));
    lines.truncate(visible_rows);
    lines
}

fn format_repo_work_list_lines(
    config: &Config,
    sessions: &[Session],
    session_indices: &[usize],
    selected: Option<usize>,
    width: usize,
    visible_rows: usize,
) -> Vec<String> {
    if visible_rows == 0 {
        return Vec::new();
    }
    let mut lines = vec![color("PRs / Work", "1;36")];
    if session_indices.is_empty() {
        lines.push(color("No worktrees discovered", "90"));
        lines.push(color("Create one with c", "33"));
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
        lines.push(format_repo_work_item_line(
            config,
            session,
            Some(*index) == selected,
            width,
        ));
    }
    lines
}

fn format_repo_work_item_line(
    config: &Config,
    session: &Session,
    selected: bool,
    width: usize,
) -> String {
    let marker = if selected {
        color("▶", "1;36")
    } else {
        " ".to_string()
    };
    let code = if selected { "1;37" } else { "37" };
    let kind = repo_work_kind_label(config, session);
    let detail = repo_work_detail_label(config, session);
    let label_width = width.saturating_sub(20);
    let label = session
        .pr
        .summary
        .as_ref()
        .map(|summary| format!("{} - {}", session.branch, summary.title))
        .unwrap_or_else(|| session.branch.clone());
    let text = format!(
        "{} {} {} {}",
        marker,
        styled_cell(&kind, 8, "90"),
        styled_cell(&truncate_line(&label, label_width), label_width, code),
        color(&detail, "90"),
    );
    ansi_cell(&text, width)
}

fn repo_work_kind_label(config: &Config, session: &Session) -> String {
    if session.is_default_branch(config) {
        "default".to_string()
    } else if let Some(summary) = &session.pr.summary {
        format!("#{}", summary.number)
    } else {
        "local".to_string()
    }
}

fn repo_work_detail_label(config: &Config, session: &Session) -> String {
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
            ci_icon(config, session),
            summary.check_status
        ));
        parts.push(pr_comment_count_label(&session.pr));
    } else {
        parts.push("no PR".to_string());
    }
    let git = git_status_indicator(&session.status_label);
    if !git.is_empty() {
        parts.push(git);
    }
    if matches!(
        session.agent_state,
        AgentState::Running | AgentState::NeedsInput | AgentState::NeedsRestart
    ) {
        parts.push(format!("agent {}", agent_icon(session.agent_state)));
    }
    truncate_line(&parts.join("  "), 48)
}

fn format_repo_preview_lines(
    config: &Config,
    session: Option<&Session>,
    width: usize,
) -> Vec<String> {
    let Some(session) = session else {
        return vec![
            color("Preview", "1;36"),
            color("No selected worktree", "90"),
            color("Enter focuses worktrees", "33"),
        ];
    };
    let mut lines = vec![color("Preview", "1;36")];
    if session.is_default_branch(config) {
        lines.push(color("Default branch", "1;37"));
        lines.push(format!("branch {}", truncate_line(&session.branch, width)));
        lines.push(format!(
            "status {}",
            truncate_line(&session.status_label, width.saturating_sub(7))
        ));
        lines.push(color("PR tracking disabled", "90"));
        return lines;
    }
    if let Some(error) = &session.pr.error {
        lines.push(color("✕ PR refresh error", "1;31"));
        lines.push(truncate_line(error, width));
        return lines;
    }
    let Some(summary) = &session.pr.summary else {
        lines.push(color("○ No PR detected", "90"));
        lines.push(format!("branch {}", truncate_line(&session.branch, width)));
        lines.push(format!(
            "status {}",
            truncate_line(&session.status_label, width.saturating_sub(7))
        ));
        lines.push(color("Space g P creates one", "33"));
        return lines;
    };
    let review = review_decision_for_display(summary, session.pr.details.as_ref());
    lines.push(color(
        &format!(
            "{} PR #{} {}",
            pr_state_icon(summary),
            summary.number,
            pr_state_label(summary),
        ),
        pr_state_color(summary),
    ));
    lines.push(color(&truncate_line(&summary.title, width), "1;37"));
    lines.push(format!(
        "{} {}  {} {}",
        color("review", "90"),
        color(review_label(&review), review_color(&review)),
        color("ci", "90"),
        color(&summary.check_status, ci_color(config, session)),
    ));
    lines.push(format!(
        "{} {}  {} {}",
        color("base", "90"),
        truncate_line(&summary.base_ref, 18),
        color("head", "90"),
        truncate_line(&summary.head_ref, 18),
    ));
    if !summary.requested_reviewers.is_empty() {
        lines.push(format!(
            "{} {}",
            color("awaiting", "90"),
            truncate_line(&summary.requested_reviewers.join(", "), width),
        ));
    }
    lines.push(String::new());
    if let Some(details) = &session.pr.details {
        lines.push(format!(
            "{} {}  {} {}  {} {}",
            color("comments", "90"),
            details.comments.len() + details.review_comments.len(),
            color("reviews", "90"),
            details.reviews.len(),
            color("files", "90"),
            details.files.len(),
        ));
        lines.extend(pr_comment_lines(details, 3));
        if !details.failing_checks.is_empty() {
            lines.push(color("Failing checks", "1;31"));
            for check in details.failing_checks.iter().take(2) {
                lines.push(format!(
                    "{} {}",
                    color("✕", "31"),
                    truncate_line(check, width)
                ));
            }
        }
        if !details.ci_failures.is_empty() {
            lines.push(format!(
                "{} {}",
                color("CI failures cached", "90"),
                details.ci_failures.len()
            ));
        }
    } else {
        lines.push(color("Activity pending", "90"));
    }
    lines
}

fn pr_state_label(summary: &crate::github::PrSummary) -> &'static str {
    if summary.merged {
        "merged"
    } else if summary.draft {
        "draft"
    } else if summary.state == "OPEN" {
        "open"
    } else {
        "closed"
    }
}

fn pr_comment_count_label(cache: &PrCache) -> String {
    if let Some(details) = &cache.details {
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
    if session.is_default_branch(config) {
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

fn worktree_status_icons(config: &Config, row: &WorktreeRow) -> String {
    if row.kind != WorktreeKind::FeatureWorktree || config.is_default_branch(&row.branch) {
        return String::new();
    }

    let mut icons = String::new();
    let Some(summary) = &row.pr.summary else {
        icons.push_str(&color("◇", "90"));
        return icons;
    };

    icons.push_str(&color(pr_state_icon(summary), pr_state_color(summary)));
    icons.push_str(&color(
        ci_icon_for_row(config, row),
        ci_color_for_row(config, row),
    ));
    icons.push_str(&color(
        &comment_count_label_for_row(config, row),
        comment_count_color_for_row(row),
    ));
    icons
}

fn comment_count_label_for_row(config: &Config, row: &WorktreeRow) -> String {
    if row.kind != WorktreeKind::FeatureWorktree || config.is_default_branch(&row.branch) {
        return String::new();
    }

    pr_comment_count_label(&row.pr)
}

fn comment_count_color_for_row(row: &WorktreeRow) -> &'static str {
    if pr_cache_has_comments(&row.pr) {
        "36"
    } else {
        "90"
    }
}

fn ci_icon(config: &Config, session: &Session) -> &'static str {
    if session.is_default_branch(config) {
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
        Some("running") => "•",
        Some("mixed") => "±",
        Some("unknown") | None => "?",
        Some(_) => "!",
    }
}

fn ci_icon_for_row(config: &Config, row: &WorktreeRow) -> &'static str {
    if row.kind != WorktreeKind::FeatureWorktree || config.is_default_branch(&row.branch) {
        return "";
    }
    match row
        .pr
        .summary
        .as_ref()
        .map(|summary| summary.check_status.as_str())
    {
        Some("passed") => "✓",
        Some("failed") => "✕",
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

fn ci_color(config: &Config, session: &Session) -> &'static str {
    if session.is_default_branch(config) {
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
    if row.kind != WorktreeKind::FeatureWorktree || config.is_default_branch(&row.branch) {
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

fn format_pr_panel_lines(config: &Config, session: Option<&Session>) -> Vec<String> {
    let Some(session) = session else {
        return vec![color("No selected worktree", "90")];
    };
    if session.is_default_branch(config) {
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
        if !details.ci_failures.is_empty() {
            lines.push(format!(
                "{} {}",
                color("CI failures cached", "90"),
                details.ci_failures.len()
            ));
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
        "⋈"
    } else if summary.draft {
        "◐"
    } else if summary.state == "OPEN" {
        "⇄"
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
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    use crate::agent::AgentState;
    use crate::auto_flow::{
        AutoImplementationSource, AutoOutputKind, AutoOutputLine, AutoRun, AutoRunMode,
        AutoRunStatus, AutoStepKey, AutoStepRun, AutoStepStatus, PersistedAutoRun,
    };
    use crate::config::{Checks, Config, EscapeKey, MergeMethod};
    use crate::github::{PrCache, PrComment, PrDetails, PrReviewComment, PrSummary};
    use crate::opencode::{OpencodeState, OpencodeStatus, OpencodeTodo};
    use crate::plan_run::{
        PersistedPlanRun, PlanOutputKind, PlanOutputLine, PlanRun, PlanRunMode, PlanRunStatus,
        PlanStepRun, PlanStepStatus, PlanTodo,
    };
    use crate::session::Session;
    use crate::tui::PanelFocus;

    use super::{
        AutoDashboard, AutoOutputViewerState, FrameModel, PlanDashboard, RepoMainView, RepoRow,
        StatusRow, WorktreeKind, WorktreeRow, configured_column_label, format_column_value,
        format_plan_rendered_output_row, format_repo_row, git_status_indicator, render_model_frame,
        render_plan_output_rows, selected_rendered_output_index, visible_len,
        worktree_status_icons,
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
        assert!(frame.contains("<Space> for more options"));
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
            opencode_port_base: 41_000,
            opencode_port_span: 1_000,
            opencode_shutdown_owned_servers: false,
            opencode_plan_plugin: false,
            escape_key: EscapeKey::EscEsc,
            merge_method: MergeMethod::Squash,
            auto: crate::config::AutoConfig::default(),
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
            agent_state: AgentState::Idle,
            opencode_status: None,
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
    fn worktree_status_icons_show_pr_ci_and_comment_counts() {
        let config = test_config(Some("main"));
        let mut pr = test_pr(12, false);
        pr.details = Some(PrDetails {
            comments: vec![PrComment {
                author: "a".to_string(),
                body: "top-level".to_string(),
                ..PrComment::default()
            }],
            review_comments: vec![
                PrReviewComment {
                    author: "b".to_string(),
                    path: "src/lib.rs".to_string(),
                    line: "1".to_string(),
                    body: "open".to_string(),
                    created_at: "now".to_string(),
                    resolved: false,
                    ..PrReviewComment::default()
                },
                PrReviewComment {
                    author: "c".to_string(),
                    path: "src/lib.rs".to_string(),
                    line: "2".to_string(),
                    body: "resolved".to_string(),
                    created_at: "now".to_string(),
                    resolved: true,
                    ..PrReviewComment::default()
                },
            ],
            ..PrDetails::default()
        });
        let session = test_session("feature", "clean", AgentState::Idle, pr);
        let row = test_worktree_row(&config, &session, 0, true);

        let icons = crate::util::strip_ansi(&worktree_status_icons(&config, &row));

        assert_eq!(icons, "⇄✓#2✓1");
    }

    #[test]
    fn detached_worktree_status_icons_hide_stale_pr_state() {
        let config = test_config(Some("main"));
        let session = test_session("(detached)", "clean", AgentState::Idle, test_pr(12, false));
        let row = test_worktree_row(&config, &session, 0, true);

        let icons = worktree_status_icons(&config, &row);

        assert!(icons.is_empty());
    }

    #[test]
    fn repo_overview_lists_pr_work_and_preview_comments() {
        let config = test_config(Some("main"));
        let mut reviewed_pr = test_pr(12, false);
        reviewed_pr.details = Some(PrDetails {
            comments: vec![PrComment {
                author: "reviewer".to_string(),
                body: "please tighten this panel".to_string(),
                ..PrComment::default()
            }],
            review_comments: vec![PrReviewComment {
                author: "reviewer".to_string(),
                path: "src/view.rs".to_string(),
                line: "120".to_string(),
                body: "this should stay readable".to_string(),
                created_at: "now".to_string(),
                resolved: false,
                ..PrReviewComment::default()
            }],
            files: vec!["src/view.rs".to_string()],
            ..PrDetails::default()
        });
        let sessions = vec![
            test_session("main", "clean", AgentState::Idle, PrCache::default()),
            test_session(
                "planned-work",
                "clean",
                AgentState::Idle,
                PrCache::default(),
            ),
            test_session("impl-work", "dirty 1", AgentState::Idle, PrCache::default()),
            test_session("pr-work", "clean", AgentState::Idle, reviewed_pr),
            test_session("merged-work", "clean", AgentState::Idle, test_pr(13, true)),
        ];
        let model = test_model(&config, &sessions, Some(3), PanelFocus::Repos, None);
        let frame = render_model_frame(&model, 160, 20);
        let frame = crate::util::strip_ansi(&frame);

        assert!(frame.contains("prs 1"));
        assert!(frame.contains("local 2"));
        assert!(frame.contains("PRs / Work"));
        assert!(frame.contains("planned-work"));
        assert!(frame.contains("impl-work"));
        assert!(frame.contains("#12"));
        assert!(frame.contains("Preview"));
        assert!(frame.contains("PR #12 open"));
        assert!(frame.contains("please tighten this panel"));
        assert!(frame.contains("merged-work"));
    }

    #[test]
    fn repo_overview_can_show_kanban_view() {
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
        let mut model = test_model(&config, &sessions, Some(2), PanelFocus::Repos, None);
        model.repo_main_view = RepoMainView::Kanban;
        let frame = crate::util::strip_ansi(&render_model_frame(&model, 160, 20));

        let plan = frame.find("plan 1").expect("plan lane");
        let implementation = frame.find("impl 1").expect("impl lane");
        let pr_ci = frame.find("pr/ci 1").expect("pr/ci lane");
        let merged = frame.find("merged 1").expect("merged lane");
        assert!(plan < implementation);
        assert!(implementation < pr_ci);
        assert!(pr_ci < merged);
        assert!(frame.contains("view kanban"));
        assert!(frame.contains("planned-work"));
        assert!(frame.contains("impl-work"));
        assert!(frame.contains("pr-work"));
        assert!(frame.contains("merged-work"));
    }

    #[test]
    fn render_model_frame_shows_plan_dashboard() {
        let config = test_config(Some("main"));
        let sessions = vec![test_session(
            "feature",
            "clean",
            AgentState::Idle,
            PrCache::default(),
        )];
        let mut model = test_model(&config, &sessions, Some(0), PanelFocus::Status, None);
        let run = PlanRun {
            id: "plan-test".to_string(),
            repo_root: "/repo".to_string(),
            scope_path: PathBuf::from("/repo"),
            plan_path: PathBuf::from("/repo/plan.md"),
            plan_display: "plan.md".to_string(),
            step_name: "phase".to_string(),
            start_step: 1,
            total_steps: 3,
            mode: PlanRunMode::Sequential,
            status: PlanRunStatus::Running,
            pause_requested: false,
            selected_step: 2,
            created_unix_ms: 1_000,
            updated_unix_ms: 61_000,
            archived_unix_ms: None,
        };
        let mut step = PlanStepRun::queued("plan-test", 2, "Implement plan.md phase 2".to_string());
        step.status = PlanStepStatus::Running;
        step.latest_message = Some("adding dashboard rows".to_string());
        step.active_tool = Some("tool bash running: cargo test".to_string());
        step.todos = vec![PlanTodo::new("render status", "in_progress")];
        model.plan_dashboard = Some(PlanDashboard {
            run: PersistedPlanRun {
                run,
                steps: vec![
                    PlanStepRun::queued("plan-test", 1, "Implement plan.md phase 1".to_string()),
                    step,
                    PlanStepRun::queued("plan-test", 3, "Implement plan.md phase 3".to_string()),
                ],
            },
            output_lines: vec![PlanOutputLine {
                run_id: "plan-test".to_string(),
                step: 2,
                line_number: 1,
                time_unix_ms: 2_000,
                kind: PlanOutputKind::Assistant,
                text: "dashboard output preview".to_string(),
                block_id: None,
            }],
            output_state: super::PlanOutputViewerState {
                cursor: 0,
                follow: true,
                expanded_blocks: BTreeSet::new(),
            },
        });

        let frame = crate::util::strip_ansi(&render_model_frame(&model, 120, 28));

        assert!(frame.contains("Plan Run"));
        assert!(frame.contains("plan.md"));
        assert!(frame.contains("phase  2/3 running"));
        assert!(frame.contains("tool bash running: cargo test"));
        assert!(frame.contains("adding dashboard rows"));
        assert!(frame.contains("todos  active 1"));
        assert!(frame.contains("dashboard output preview"));
    }

    #[test]
    fn render_model_frame_shows_auto_dashboard() {
        let config = test_config(Some("main"));
        let sessions = vec![test_session(
            "feature",
            "clean",
            AgentState::Idle,
            PrCache::default(),
        )];
        let mut model = test_model(&config, &sessions, Some(0), PanelFocus::Status, None);
        let run = AutoRun {
            id: "auto-test".to_string(),
            repo_root: "/repo".to_string(),
            worktree_path: PathBuf::from("/repo/feature"),
            branch: "feature".to_string(),
            mode: AutoRunMode::Standard,
            implementation_source: AutoImplementationSource::Prompt,
            plan_path: None,
            plan_run_mode: PlanRunMode::Sequential,
            variant: "default".to_string(),
            agent_profile: None,
            prompt_summary: "Implement the thing".to_string(),
            initial_prompt: "Implement the thing".to_string(),
            status: AutoRunStatus::Running,
            pause_requested: false,
            selected_step_run_id: Some(7),
            pr_number: Some(42),
            pr_url: Some("https://example.com/pr/42".to_string()),
            current_head_sha: None,
            review_baseline_json: None,
            created_unix_ms: 1_000,
            updated_unix_ms: 61_000,
            archived_unix_ms: None,
        };
        let mut step = AutoStepRun::queued(
            "auto-test",
            1,
            AutoStepKey::Prepare,
            1,
            Some("validate worktree".to_string()),
        );
        step.id = Some(7);
        step.status = AutoStepStatus::Waiting;
        step.summary = Some("waiting for safe boundary".to_string());
        model.auto_dashboard = Some(AutoDashboard {
            run: PersistedAutoRun {
                run,
                steps: vec![step],
            },
            linked_plan_dashboard: None,
            output_lines: vec![AutoOutputLine {
                step_run_id: 7,
                line_number: 1,
                time_unix_ms: 2_000,
                kind: AutoOutputKind::System,
                text: "created no-op auto run".to_string(),
                block_id: None,
            }],
            output_state: AutoOutputViewerState {
                cursor: 0,
                follow: true,
            },
        });

        let frame = crate::util::strip_ansi(&render_model_frame(&model, 120, 28));

        assert!(frame.contains("Auto Flow"));
        assert!(frame.contains("Implement the thing"));
        assert!(frame.contains("source prompt"));
        assert!(frame.contains("step   #1 prepare attempt 1 waiting"));
        assert!(frame.contains("waiting for safe boundary"));
        assert!(frame.contains("created no-op auto run"));
    }

    #[test]
    fn render_auto_dashboard_shows_linked_plan_run() {
        let config = test_config(Some("main"));
        let sessions = vec![test_session(
            "feature",
            "clean",
            AgentState::Idle,
            PrCache::default(),
        )];
        let mut model = test_model(&config, &sessions, Some(0), PanelFocus::Status, None);
        let run = AutoRun {
            id: "auto-test".to_string(),
            repo_root: "/repo".to_string(),
            worktree_path: PathBuf::from("/repo/feature"),
            branch: "feature".to_string(),
            mode: AutoRunMode::Standard,
            implementation_source: AutoImplementationSource::ExistingPlan,
            plan_path: Some(PathBuf::from("plan.md")),
            plan_run_mode: PlanRunMode::Sequential,
            variant: "default".to_string(),
            agent_profile: None,
            prompt_summary: "Run the plan".to_string(),
            initial_prompt: "Run the plan".to_string(),
            status: AutoRunStatus::Running,
            pause_requested: false,
            selected_step_run_id: Some(9),
            pr_number: None,
            pr_url: None,
            current_head_sha: None,
            review_baseline_json: None,
            created_unix_ms: 1_000,
            updated_unix_ms: 61_000,
            archived_unix_ms: None,
        };
        let mut auto_step = AutoStepRun::queued(
            "auto-test",
            1,
            AutoStepKey::RunPlan,
            1,
            Some("run plan phases".to_string()),
        );
        auto_step.id = Some(9);
        auto_step.status = AutoStepStatus::Running;
        auto_step.plan_run_id = Some("plan-test".to_string());
        let plan_run = PlanRun {
            id: "plan-test".to_string(),
            repo_root: "/repo".to_string(),
            scope_path: PathBuf::from("/repo/feature"),
            plan_path: PathBuf::from("/repo/feature/plan.md"),
            plan_display: "plan.md".to_string(),
            step_name: "phase".to_string(),
            start_step: 1,
            total_steps: 2,
            mode: PlanRunMode::Sequential,
            status: PlanRunStatus::Running,
            pause_requested: false,
            selected_step: 2,
            created_unix_ms: 1_000,
            updated_unix_ms: 61_000,
            archived_unix_ms: None,
        };
        let mut phase =
            PlanStepRun::queued("plan-test", 2, "Implement plan.md phase 2".to_string());
        phase.status = PlanStepStatus::Running;
        phase.latest_message = Some("building phase detail".to_string());
        model.auto_dashboard = Some(AutoDashboard {
            run: PersistedAutoRun {
                run,
                steps: vec![auto_step],
            },
            linked_plan_dashboard: Some(PlanDashboard {
                run: PersistedPlanRun {
                    run: plan_run,
                    steps: vec![
                        PlanStepRun::queued(
                            "plan-test",
                            1,
                            "Implement plan.md phase 1".to_string(),
                        ),
                        phase,
                    ],
                },
                output_lines: vec![PlanOutputLine {
                    run_id: "plan-test".to_string(),
                    step: 2,
                    line_number: 1,
                    time_unix_ms: 2_000,
                    kind: PlanOutputKind::Assistant,
                    text: "linked plan output preview".to_string(),
                    block_id: None,
                }],
                output_state: super::PlanOutputViewerState {
                    cursor: 0,
                    follow: true,
                    expanded_blocks: BTreeSet::new(),
                },
            }),
            output_lines: vec![AutoOutputLine {
                step_run_id: 9,
                line_number: 1,
                time_unix_ms: 2_000,
                kind: AutoOutputKind::Status,
                text: "running plan phases from plan.md".to_string(),
                block_id: None,
            }],
            output_state: AutoOutputViewerState {
                cursor: 0,
                follow: true,
            },
        });

        let frame = crate::util::strip_ansi(&render_model_frame(&model, 140, 32));

        assert!(frame.contains("source plan file  plan plan.md"));
        assert!(frame.contains("step   #1 run_plan attempt 1 running"));
        assert!(frame.contains("linked plan plan.md  status running  mode sequential"));
        assert!(frame.contains("phase 2/2 running  building phase detail"));
        assert!(frame.contains("plan output linked plan output preview"));
        assert!(frame.contains("running plan phases from plan.md"));
    }

    #[test]
    fn plan_output_viewer_collapses_tool_blocks_by_default() {
        let dashboard = test_plan_dashboard_with_output(
            vec![
                PlanOutputLine {
                    run_id: "plan-test".to_string(),
                    step: 1,
                    line_number: 1,
                    time_unix_ms: 1_000,
                    kind: PlanOutputKind::Tool,
                    text: "tool bash running: cargo test".to_string(),
                    block_id: Some("call-1".to_string()),
                },
                PlanOutputLine {
                    run_id: "plan-test".to_string(),
                    step: 1,
                    line_number: 2,
                    time_unix_ms: 1_001,
                    kind: PlanOutputKind::ToolOutput,
                    text: "stdout line that should be hidden while collapsed".to_string(),
                    block_id: Some("call-1".to_string()),
                },
            ],
            super::PlanOutputViewerState {
                cursor: 0,
                follow: false,
                expanded_blocks: BTreeSet::new(),
            },
        );

        let lines = render_plan_output_rows(&dashboard, 120);

        assert_eq!(lines.len(), 1);
        assert!(lines[0].collapsed);
        assert!(lines[0].text.contains("[+] L1 2 lines"));
        assert!(!lines[0].text.contains("stdout line"));
    }

    #[test]
    fn plan_output_viewer_expands_and_highlights_diff_lines() {
        let mut expanded_blocks = BTreeSet::new();
        expanded_blocks.insert("diff-line:3".to_string());
        let dashboard = test_plan_dashboard_with_output(
            vec![PlanOutputLine {
                run_id: "plan-test".to_string(),
                step: 1,
                line_number: 3,
                time_unix_ms: 1_000,
                kind: PlanOutputKind::Diff,
                text: "@@ -1 +1 @@\n- old\n+ new".to_string(),
                block_id: None,
            }],
            super::PlanOutputViewerState {
                cursor: 0,
                follow: false,
                expanded_blocks,
            },
        );

        let rows = render_plan_output_rows(&dashboard, 120);
        let rendered = rows
            .iter()
            .map(|row| format_plan_rendered_output_row(row, false, 120))
            .collect::<Vec<_>>()
            .join("\n");
        let plain = crate::util::strip_ansi(&rendered);

        assert_eq!(rows.len(), 3);
        assert!(plain.contains("@@ -1 +1 @@"));
        assert!(plain.contains("- old"));
        assert!(plain.contains("+ new"));
        assert!(rendered.contains("\x1b[32m+ new\x1b[0m"));
        assert!(rendered.contains("\x1b[31m- old\x1b[0m"));
    }

    #[test]
    fn plan_output_viewer_maps_output_cursor_to_rendered_rows() {
        let collapsed_dashboard = test_plan_dashboard_with_output(
            vec![
                PlanOutputLine {
                    run_id: "plan-test".to_string(),
                    step: 1,
                    line_number: 1,
                    time_unix_ms: 1_000,
                    kind: PlanOutputKind::Tool,
                    text: "tool bash running: cargo test".to_string(),
                    block_id: Some("call-1".to_string()),
                },
                PlanOutputLine {
                    run_id: "plan-test".to_string(),
                    step: 1,
                    line_number: 2,
                    time_unix_ms: 1_001,
                    kind: PlanOutputKind::ToolOutput,
                    text: "hidden output line".to_string(),
                    block_id: Some("call-1".to_string()),
                },
            ],
            super::PlanOutputViewerState {
                cursor: 1,
                follow: false,
                expanded_blocks: BTreeSet::new(),
            },
        );
        let collapsed_rows = render_plan_output_rows(&collapsed_dashboard, 120);

        assert_eq!(
            selected_rendered_output_index(&collapsed_dashboard, &collapsed_rows),
            0
        );

        let mut expanded_blocks = BTreeSet::new();
        expanded_blocks.insert("diff-line:3".to_string());
        let expanded_dashboard = test_plan_dashboard_with_output(
            vec![PlanOutputLine {
                run_id: "plan-test".to_string(),
                step: 1,
                line_number: 3,
                time_unix_ms: 1_000,
                kind: PlanOutputKind::Diff,
                text: "@@ -1 +1 @@\n- old\n+ new".to_string(),
                block_id: None,
            }],
            super::PlanOutputViewerState {
                cursor: 0,
                follow: false,
                expanded_blocks,
            },
        );
        let expanded_rows = render_plan_output_rows(&expanded_dashboard, 120);

        assert_eq!(
            selected_rendered_output_index(&expanded_dashboard, &expanded_rows),
            0
        );
    }

    #[test]
    fn plan_output_viewer_cursor_and_follow_select_latest_row() {
        let dashboard = test_plan_dashboard_with_output(
            vec![
                PlanOutputLine {
                    run_id: "plan-test".to_string(),
                    step: 1,
                    line_number: 1,
                    time_unix_ms: 1_000,
                    kind: PlanOutputKind::Assistant,
                    text: "first".to_string(),
                    block_id: None,
                },
                PlanOutputLine {
                    run_id: "plan-test".to_string(),
                    step: 1,
                    line_number: 2,
                    time_unix_ms: 1_001,
                    kind: PlanOutputKind::Assistant,
                    text: "latest".to_string(),
                    block_id: None,
                },
            ],
            super::PlanOutputViewerState {
                cursor: 1,
                follow: true,
                expanded_blocks: BTreeSet::new(),
            },
        );

        let frame = crate::util::strip_ansi(&render_model_frame(
            &test_model_with_plan_dashboard(dashboard),
            120,
            24,
        ));

        assert!(frame.contains(">     L2"));
        assert!(frame.contains("latest"));
    }

    fn test_model_with_plan_dashboard(dashboard: PlanDashboard) -> FrameModel<'static> {
        let config = Box::leak(Box::new(test_config(Some("main"))));
        let sessions = Box::leak(Box::new(vec![test_session(
            "feature",
            "clean",
            AgentState::Idle,
            PrCache::default(),
        )]));
        let mut model = test_model(config, sessions, Some(0), PanelFocus::Status, None);
        model.plan_dashboard = Some(dashboard);
        model
    }

    fn test_plan_dashboard_with_output(
        output_lines: Vec<PlanOutputLine>,
        output_state: super::PlanOutputViewerState,
    ) -> PlanDashboard {
        let run = PlanRun {
            id: "plan-test".to_string(),
            repo_root: "/repo".to_string(),
            scope_path: PathBuf::from("/repo"),
            plan_path: PathBuf::from("/repo/plan.md"),
            plan_display: "plan.md".to_string(),
            step_name: "phase".to_string(),
            start_step: 1,
            total_steps: 1,
            mode: PlanRunMode::Sequential,
            status: PlanRunStatus::Running,
            pause_requested: false,
            selected_step: 1,
            created_unix_ms: 1_000,
            updated_unix_ms: 2_000,
            archived_unix_ms: None,
        };
        PlanDashboard {
            run: PersistedPlanRun {
                run,
                steps: vec![PlanStepRun::queued(
                    "plan-test",
                    1,
                    "Implement plan.md phase 1".to_string(),
                )],
            },
            output_lines,
            output_state,
        }
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
    fn render_model_frame_keeps_panes_within_narrow_viewport() {
        let config = test_config(Some("main"));
        let sessions = vec![test_session(
            "feature/very-long-branch-name-that-must-fit",
            "dirty 12 ahead 3",
            AgentState::Running,
            PrCache::default(),
        )];
        let model = test_model(&config, &sessions, Some(0), PanelFocus::Worktrees, None);
        let frame = render_model_frame(&model, 20, 10);
        let first_line = crate::util::strip_ansi(frame.lines().next().unwrap_or_default());
        let chars = first_line.chars().collect::<Vec<_>>();

        assert_eq!(chars.len(), 20);
        assert_eq!(chars[9], '|');
        for line in frame.lines() {
            assert_eq!(visible_len(line), 20);
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
            repo_main_view: RepoMainView::Github,
            mode_label: "normal",
            status_message: None,
            repo_filter: "",
            worktree_filter: "",
            leader_hint: None,
            auto_dashboard: None,
            plan_dashboard: None,
        };

        let frame = render_model_frame(&model, 100, 24);
        let stripped = crate::util::strip_ansi(&frame);
        let lines = stripped.lines().collect::<Vec<_>>();

        assert!(lines[0].contains("1 Status"));
        assert!(lines[0].contains("Main"));
        assert!(lines[7].contains("2 Repos"));
        assert!(lines[14].contains("3 Worktrees / Sessions"));
        assert!(stripped.contains("feature"));
        for line in frame.lines() {
            assert_eq!(visible_len(line), 100);
        }
    }

    #[test]
    fn worktree_detail_renders_opencode_status_snapshot() {
        let config = test_config(Some("main"));
        let mut session = test_session("feature", "clean", AgentState::Running, PrCache::default());
        session.opencode_status = Some(OpencodeStatus {
            server_url: Some("http://127.0.0.1:41000".to_string()),
            session_id: Some("ses_123456789".to_string()),
            title: Some("feature work".to_string()),
            state: OpencodeState::Busy,
            latest_message: Some("implementing phase five".to_string()),
            active_tool: Some("bash running".to_string()),
            todos: vec![
                OpencodeTodo {
                    text: "poll".to_string(),
                    status: "in_progress".to_string(),
                },
                OpencodeTodo {
                    text: "render".to_string(),
                    status: "pending".to_string(),
                },
            ],
            last_updated_unix_ms: Some(42),
        });
        let sessions = vec![session];
        let model = test_model(&config, &sessions, Some(0), PanelFocus::Worktrees, None);
        let frame = crate::util::strip_ansi(&render_model_frame(&model, 140, 24));

        assert!(frame.contains("opencode busy  session ses_1234"));
        assert!(frame.contains("tool bash running"));
        assert!(frame.contains("latest implementing phase five"));
        assert!(frame.contains("todos pending 1  active 1"));
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

        assert_eq!(chars[50], '|');
        assert!(chars[..50].iter().all(|ch| *ch == '-'));
        assert!(chars[51..].iter().all(|ch| *ch == '-'));
        assert_eq!(chars.len(), 160);
    }

    fn test_config(default_base: Option<&str>) -> Config {
        Config {
            default_agent: "opencode".to_string(),
            default_base: default_base.map(str::to_string),
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            opencode_port_base: 41_000,
            opencode_port_span: 1_000,
            opencode_shutdown_owned_servers: false,
            opencode_plan_plugin: false,
            escape_key: EscapeKey::EscEsc,
            merge_method: MergeMethod::Squash,
            auto: crate::config::AutoConfig::default(),
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
            agent_state,
            opencode_status: None,
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
            repo_main_view: RepoMainView::Github,
            mode_label: "normal",
            status_message,
            repo_filter: "",
            worktree_filter: "",
            leader_hint: None,
            auto_dashboard: None,
            plan_dashboard: None,
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
            kind: if session.is_default_branch(config) {
                WorktreeKind::DefaultBranch
            } else if session.branch == "(detached)" {
                WorktreeKind::Detached
            } else {
                WorktreeKind::FeatureWorktree
            },
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
