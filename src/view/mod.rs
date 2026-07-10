#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

use crate::{
    agent::AgentState,
    auto_flow::{
        AutoImplementationSource, AutoOutputKind, AutoOutputLine, AutoRunMode, AutoRunStatus,
        AutoStepKey, AutoStepRun, AutoStepStatus, PersistedAutoRun,
        stabilization_model::{PendingPushGuard, StabilizationBlocker},
    },
    config::{Config, IconStyle},
    github::PrCache,
    opencode::OpencodeState,
    plan_run::{
        PersistedPlanRun, PlanOutputKind, PlanOutputLine, PlanRunMode, PlanRunStatus, PlanStepRun,
        PlanStepStatus, plan_output_block_key,
    },
    session::{Session, SessionClassification},
    tui::{PanelFocus, WorktreeListMode},
    util::{status_count, truncate},
};

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
    pub selected_comment: usize,
    pub focus: PanelFocus,
    pub main_focused: bool,
    pub repo_main_view: RepoMainView,
    pub worktree_main_view: WorktreeMainView,
    pub worktree_list_mode: WorktreeListMode,
    pub mode_label: &'a str,
    pub status_message: Option<&'a str>,
    pub repo_filter: &'a str,
    pub worktree_filter: &'a str,
    pub leader_hint: Option<LeaderHintModel>,
    pub auto_dashboard: Option<AutoDashboard>,
    pub plan_dashboard: Option<PlanDashboard>,
    pub dialog: Option<DialogModel>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DialogModel {
    Help {
        filter: String,
        editing_filter: bool,
        info_lines: Vec<Line<'static>>,
        items: Vec<String>,
        scroll: usize,
    },
    Confirm {
        title: String,
        lines: Vec<DialogLine>,
        confirm_label: String,
        cancel_label: String,
    },
    Prompt {
        title: String,
        prompt: String,
        input: String,
    },
    Choice {
        choices: ChoiceList,
    },
    WorktreeColumns {
        title: String,
        columns: Vec<WorktreeColumnChoice>,
        selected: usize,
    },
    Progress {
        title: String,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DialogLine {
    pub text: String,
    pub attention: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChoiceList {
    pub title: String,
    pub choices: Vec<KeyChoice>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeyChoice {
    pub key: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorktreeColumnChoice {
    pub key: String,
    pub enabled: bool,
}

pub(crate) type LeaderHintModel = ChoiceList;

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
    pub repo_label: String,
    pub repo_root: String,
    pub worktree_path: String,
    pub branch: String,
    pub visibility: i16,
    pub kind: WorktreeKind,
    pub agent_state: AgentState,
    pub status_label: String,
    pub pr: PrCache,
    pub wt_columns: BTreeMap<String, String>,
    pub auto_status: Option<AutoRunStatus>,
    pub plan_status: Option<PlanRunStatus>,
    pub updated_label: String,
    pub unseen_comments: bool,
    pub prompt_summary: String,
    pub classification: SessionClassification,
    pub selected: bool,
}

pub(crate) struct PlanDashboard {
    pub run: PersistedPlanRun,
    pub runs: Vec<PlanRunSummary>,
    pub output_lines: Vec<PlanOutputLine>,
    pub output_state: PlanOutputViewerState,
}

pub(crate) struct PlanRunSummary {
    pub id: String,
    pub plan_display: String,
    pub scope_path: String,
    pub status: crate::plan_run::PlanRunStatus,
    pub updated_unix_ms: u64,
    pub selected: bool,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorktreeMainView {
    Details,
    Plan,
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

mod auto_dashboard;
mod dialog;
mod format;
mod layout;
mod main_panel;
mod plan_dashboard;
mod pr;
mod repo_panel;
mod shell;
mod sidebar;
mod style;
mod worktree_panel;

#[cfg(test)]
mod tests;

pub(crate) use shell::render;

pub(crate) fn sidebar_width_for(cols: u16, configured_width: Option<u16>) -> u16 {
    layout::sidebar_width(cols, configured_width)
}

use auto_dashboard::*;
use dialog::*;
use format::*;
use layout::*;
use main_panel::*;
use plan_dashboard::*;
pub(crate) use pr::*;
use repo_panel::*;
use sidebar::*;
use style::*;
use worktree_panel::*;

pub(crate) fn keybinding_info_lines(
    focus: PanelFocus,
    icon_style: IconStyle,
) -> Vec<Line<'static>> {
    match focus {
        PanelFocus::Status => Vec::new(),
        PanelFocus::Repos => vec![
            Line::from(Span::styled("Repository columns", title_style(true))),
            info_columns_row(&[
                repo_info_cell(
                    repo_health_icon(RepoHealthKind::Dirty, icon_style),
                    health_style("dirty"),
                    "dirty",
                ),
                repo_info_cell(
                    repo_health_icon(RepoHealthKind::Agents, icon_style),
                    Style::default().fg(Color::Green),
                    "agents",
                ),
                repo_info_cell(
                    repo_health_icon(RepoHealthKind::Attention, icon_style),
                    attention_style(),
                    "attention",
                ),
            ]),
            info_columns_row(&[
                repo_info_cell(
                    repo_health_icon(RepoHealthKind::PullRequests, icon_style),
                    Style::default().fg(Color::Green),
                    "pull requests",
                ),
                repo_info_cell(
                    repo_health_icon(RepoHealthKind::CiFailed, icon_style),
                    error_style(),
                    "failed CI",
                ),
                repo_info_cell(
                    repo_health_icon(RepoHealthKind::CiRunning, icon_style),
                    attention_style(),
                    "running CI",
                ),
            ]),
            info_columns_row(&[repo_info_cell(
                repo_health_icon(RepoHealthKind::Behind, icon_style),
                attention_style(),
                "behind",
            )]),
        ],
        PanelFocus::Worktrees => vec![
            Line::from(Span::styled("Worktree columns", title_style(true))),
            info_columns_row(&[
                info_cell("↕", muted_style(), "visibility"),
                info_cell("K", muted_style(), "kind"),
            ]),
            info_columns_row(&[
                info_cell("↑", visibility_style(1), "raised"),
                info_cell(
                    "p",
                    classification_style(SessionClassification::Planning),
                    "planning",
                ),
            ]),
            info_columns_row(&[
                info_cell("↓", visibility_style(-1), "lowered"),
                info_cell(
                    "e",
                    classification_style(SessionClassification::Exploration),
                    "explore",
                ),
            ]),
            info_columns_row(&[
                info_cell("·", visibility_style(0), "normal"),
                info_cell(
                    "blank",
                    classification_style(SessionClassification::Work),
                    "work",
                ),
            ]),
            Line::from(""),
            info_columns_row(&[
                info_cell("A", muted_style(), "agent"),
                info_cell("P", muted_style(), "PR"),
                info_cell("G", muted_style(), "git"),
                info_cell("C", muted_style(), "CI"),
            ]),
            info_columns_row(&[
                info_cell("○", agent_style(AgentState::Idle), "idle"),
                info_cell("⇄", Style::default().fg(Color::Green), "open"),
                info_cell("✓", Style::default().fg(Color::Green), "clean"),
                info_cell("✓", pr_check_style("passed"), "passed"),
            ]),
            info_columns_row(&[
                info_cell("●", agent_style(AgentState::Running), "running"),
                info_cell("◐", muted_style(), "draft"),
                info_cell("✗", error_style(), "dirty"),
                info_cell("✕", pr_check_style("failed"), "failed"),
            ]),
            info_columns_row(&[
                info_cell("✓", agent_style(AgentState::ExitedOk), "done"),
                info_cell("⋈", Style::default().fg(Color::Magenta), "merged"),
                info_cell("↑", attention_style(), "ahead"),
                info_cell("•", pr_check_style("running"), "running"),
            ]),
            info_columns_row(&[
                info_cell("✕", agent_style(AgentState::ExitedError), "failed"),
                info_cell("×", Style::default().fg(Color::Red), "closed"),
                info_cell("↓", attention_style(), "behind"),
                info_cell("±", pr_check_style("mixed"), "mixed"),
            ]),
            info_columns_row(&[
                info_cell("↻", agent_style(AgentState::NeedsRestart), "restart"),
                info_cell("⚔", Style::default().fg(Color::Red), "conflict"),
                info_cell("↕", attention_style(), "diverged"),
                info_cell("?", pr_check_style("unknown"), "unknown"),
            ]),
            info_columns_row(&[
                info_cell("!", agent_style(AgentState::NeedsInput), "input"),
                info_blank_cell(),
                info_blank_cell(),
                info_blank_cell(),
            ]),
            Line::from(""),
            info_symbol_line("@", muted_style(), "unresolved/resolved review comments"),
            info_symbol_line("!", error_style(), "errors or attention needed"),
        ],
    }
}

#[derive(Clone, Copy)]
struct InfoCell {
    symbol: &'static str,
    style: Style,
    count_marker: Option<&'static str>,
    description: &'static str,
}

fn info_cell(symbol: &'static str, style: Style, description: &'static str) -> Option<InfoCell> {
    Some(InfoCell {
        symbol,
        style,
        count_marker: None,
        description,
    })
}

fn repo_info_cell(
    symbol: &'static str,
    style: Style,
    description: &'static str,
) -> Option<InfoCell> {
    Some(InfoCell {
        symbol,
        style,
        count_marker: Some("#"),
        description,
    })
}

fn info_blank_cell() -> Option<InfoCell> {
    None
}

fn info_columns_row(cells: &[Option<InfoCell>]) -> Line<'static> {
    const CELL_WIDTH: usize = 24;
    const COMPACT_CELL_WIDTH: usize = 18;
    const SYMBOL_WIDTH: usize = 2;

    let mut spans = Vec::new();
    let padded_cell_width = if cells.len() > 3 {
        COMPACT_CELL_WIDTH
    } else {
        CELL_WIDTH
    };
    for (index, cell) in cells.iter().enumerate() {
        let cell_width = if index + 1 == cells.len() {
            0
        } else {
            padded_cell_width
        };
        match cell {
            Some(cell) => {
                spans.push(Span::styled(cell.symbol, cell.style));
                let symbol_width = display_width(cell.symbol);
                if symbol_width < SYMBOL_WIDTH {
                    spans.push(Span::raw(" ".repeat(SYMBOL_WIDTH - symbol_width)));
                }
                let marker_width = if let Some(marker) = cell.count_marker {
                    spans.push(Span::styled(marker, muted_style()));
                    display_width(marker)
                } else {
                    spans.push(Span::raw(" "));
                    1
                };
                spans.push(Span::raw(format!(" {}", cell.description)));
                let width = symbol_width.max(SYMBOL_WIDTH)
                    + marker_width
                    + 1
                    + display_width(cell.description);
                if width < cell_width {
                    spans.push(Span::raw(" ".repeat(cell_width - width)));
                }
            }
            None => {
                if cell_width > 0 {
                    spans.push(Span::raw(" ".repeat(cell_width)));
                }
            }
        }
    }
    Line::from(spans)
}

fn display_width(text: &str) -> usize {
    Line::from(text).width()
}

fn info_symbol_line(
    symbol: &'static str,
    style: Style,
    description: &'static str,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(symbol, style),
        Span::raw(format!(": {description}")),
    ])
}

#[derive(Clone, Copy)]
pub(crate) enum RepoHealthKind {
    Dirty,
    Agents,
    Attention,
    PullRequests,
    CiFailed,
    CiRunning,
    Behind,
}

pub(crate) fn repo_health_icon(kind: RepoHealthKind, icon_style: IconStyle) -> &'static str {
    match kind {
        RepoHealthKind::Dirty => icon(icon_style, "■", ""),
        RepoHealthKind::Agents => icon(icon_style, "●", ""),
        RepoHealthKind::Attention => icon(icon_style, "▲", ""),
        RepoHealthKind::PullRequests => icon(icon_style, "◇", ""),
        RepoHealthKind::CiFailed => icon(icon_style, "✕", ""),
        RepoHealthKind::CiRunning => icon(icon_style, "◐", ""),
        RepoHealthKind::Behind => icon(icon_style, "▼", ""),
    }
}

pub(crate) fn repo_health_style(kind: RepoHealthKind) -> Style {
    match kind {
        RepoHealthKind::Dirty => health_style("dirty"),
        RepoHealthKind::Agents | RepoHealthKind::PullRequests => Style::default().fg(Color::Green),
        RepoHealthKind::Attention | RepoHealthKind::CiRunning | RepoHealthKind::Behind => {
            attention_style()
        }
        RepoHealthKind::CiFailed => error_style(),
    }
}
