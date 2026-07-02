#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use crate::agent::AgentState;
use crate::auto_flow::{AutoOutputLine, AutoRunStatus, PersistedAutoRun};
use crate::config::Config;
use crate::github::PrCache;
use crate::plan_run::{PersistedPlanRun, PlanOutputLine};
use crate::session::Session;
use crate::session::SessionClassification;
use crate::tui::PanelFocus;

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
    pub main_focused: bool,
    pub repo_main_view: RepoMainView,
    pub worktree_main_view: WorktreeMainView,
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
        items: Vec<String>,
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
    pub repo_root: String,
    pub worktree_path: String,
    pub branch: String,
    pub kind: WorktreeKind,
    pub agent_state: AgentState,
    pub status_label: String,
    pub pr: PrCache,
    pub wt_columns: BTreeMap<String, String>,
    pub auto_status: Option<AutoRunStatus>,
    pub unseen_comments: bool,
    pub prompt_summary: String,
    pub classification: SessionClassification,
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
