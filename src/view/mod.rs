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
    },
    config::{Config, IconStyle},
    github::PrCache,
    opencode::{OpencodeState, OpencodeStatus},
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
        info_lines: Vec<String>,
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
