use super::*;

pub const DEFAULT_OUTPUT_LINES_PER_STEP: usize = 2_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanRun {
    pub id: String,
    pub harness_id: String,
    pub adapter_id: String,
    pub repo_root: String,
    pub scope_path: PathBuf,
    pub plan_path: PathBuf,
    pub plan_display: String,
    pub step_name: String,
    pub start_step: usize,
    pub total_steps: usize,
    pub mode: PlanRunMode,
    pub status: PlanRunStatus,
    pub pause_requested: bool,
    pub selected_step: usize,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
    pub archived_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanStepRun {
    pub run_id: String,
    pub step: usize,
    pub prompt: String,
    pub status: PlanStepStatus,
    pub execution: crate::harness::ExecutionRef,
    pub session: crate::harness::SessionRef,
    pub agent_variant: Option<String>,
    pub started_unix_ms: Option<u64>,
    pub finished_unix_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub latest_message: Option<String>,
    pub active_tool: Option<String>,
    pub todos: Vec<PlanTodo>,
    pub summary: Option<String>,
    pub error: Option<String>,
}

pub type PlanTodo = crate::harness::AgentTodo;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanOutputLine {
    pub run_id: String,
    pub step: usize,
    pub line_number: u64,
    pub time_unix_ms: u64,
    pub kind: PlanOutputKind,
    pub text: String,
    pub block_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanRunMode {
    Sequential,
    Parallel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanRunStatus {
    Draft,
    Queued,
    Running,
    Paused,
    Done,
    Failed,
    Aborted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanStepStatus {
    Queued,
    Starting,
    Running,
    Done,
    Failed,
    Aborted,
    Skipped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanOutputKind {
    Assistant,
    Tool,
    ToolOutput,
    Diff,
    Todo,
    Status,
    RawJson,
    System,
    Error,
}

pub fn plan_output_block_key(line: &PlanOutputLine) -> Option<String> {
    match line.kind {
        PlanOutputKind::Tool | PlanOutputKind::ToolOutput => line
            .block_id
            .as_ref()
            .map(|block_id| format!("tool:{block_id}"))
            .or_else(|| Some(format!("tool-line:{}", line.line_number))),
        PlanOutputKind::Diff => line
            .block_id
            .as_ref()
            .map(|block_id| format!("diff:{block_id}"))
            .or_else(|| Some(format!("diff-line:{}", line.line_number))),
        PlanOutputKind::RawJson => Some(format!("raw:{}", line.line_number)),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedPlanRun {
    pub run: PlanRun,
    pub steps: Vec<PlanStepRun>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlanStatusCounts {
    pub queued: usize,
    pub starting: usize,
    pub running: usize,
    pub done: usize,
    pub failed: usize,
    pub aborted: usize,
    pub skipped: usize,
}

pub type PlanAgentEvent = crate::harness::AgentEvent;

impl PlanRunMode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Parallel => "parallel",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "sequential" => Ok(Self::Sequential),
            "parallel" => Ok(Self::Parallel),
            _ => Err(format!("unknown plan run mode: {value}")),
        }
    }
}

impl PlanRunStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "draft" => Ok(Self::Draft),
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "aborted" => Ok(Self::Aborted),
            _ => Err(format!("unknown plan run status: {value}")),
        }
    }
}

impl PlanStepStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
            Self::Skipped => "skipped",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "queued" => Ok(Self::Queued),
            "starting" => Ok(Self::Starting),
            "running" => Ok(Self::Running),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "aborted" => Ok(Self::Aborted),
            "skipped" => Ok(Self::Skipped),
            _ => Err(format!("unknown plan step status: {value}")),
        }
    }
}

impl PlanOutputKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::ToolOutput => "tool_output",
            Self::Diff => "diff",
            Self::Todo => "todo",
            Self::Status => "status",
            Self::RawJson => "raw_json",
            Self::System => "system",
            Self::Error => "error",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "assistant" => Ok(Self::Assistant),
            "tool" => Ok(Self::Tool),
            "tool_output" => Ok(Self::ToolOutput),
            "diff" => Ok(Self::Diff),
            "todo" => Ok(Self::Todo),
            "status" => Ok(Self::Status),
            "raw_json" => Ok(Self::RawJson),
            "system" => Ok(Self::System),
            "error" => Ok(Self::Error),
            _ => Err(format!("unknown plan output kind: {value}")),
        }
    }
}

impl PlanStepRun {
    pub fn queued(run_id: &str, step: usize, prompt: String) -> Self {
        Self {
            run_id: run_id.to_string(),
            step,
            prompt,
            status: PlanStepStatus::Queued,
            execution: crate::harness::ExecutionRef::default(),
            session: crate::harness::SessionRef::default(),
            agent_variant: None,
            started_unix_ms: None,
            finished_unix_ms: None,
            exit_code: None,
            latest_message: None,
            active_tool: None,
            todos: Vec::new(),
            summary: None,
            error: None,
        }
    }
}

impl PersistedPlanRun {
    pub fn status_counts(&self) -> PlanStatusCounts {
        PlanStatusCounts::from_steps(&self.steps)
    }

    pub fn aggregate_status(&self) -> PlanRunStatus {
        aggregate_step_status(self.steps.iter().map(|step| step.status))
    }
}

impl PlanStatusCounts {
    pub fn from_steps<'a>(steps: impl IntoIterator<Item = &'a PlanStepRun>) -> Self {
        let mut counts = Self::default();
        for step in steps {
            match step.status {
                PlanStepStatus::Queued => counts.queued += 1,
                PlanStepStatus::Starting => counts.starting += 1,
                PlanStepStatus::Running => counts.running += 1,
                PlanStepStatus::Done => counts.done += 1,
                PlanStepStatus::Failed => counts.failed += 1,
                PlanStepStatus::Aborted => counts.aborted += 1,
                PlanStepStatus::Skipped => counts.skipped += 1,
            }
        }
        counts
    }
}

pub fn aggregate_step_status(statuses: impl IntoIterator<Item = PlanStepStatus>) -> PlanRunStatus {
    let mut saw_status = false;
    let mut has_queued = false;
    let mut has_running = false;
    let mut has_failed = false;
    let mut has_aborted = false;
    for status in statuses {
        saw_status = true;
        match status {
            PlanStepStatus::Failed => has_failed = true,
            PlanStepStatus::Aborted => has_aborted = true,
            PlanStepStatus::Starting | PlanStepStatus::Running => has_running = true,
            PlanStepStatus::Queued => has_queued = true,
            PlanStepStatus::Done | PlanStepStatus::Skipped => {}
        }
    }
    if !saw_status {
        PlanRunStatus::Draft
    } else if has_failed {
        PlanRunStatus::Failed
    } else if has_aborted {
        PlanRunStatus::Aborted
    } else if has_running {
        PlanRunStatus::Running
    } else if has_queued {
        PlanRunStatus::Queued
    } else {
        PlanRunStatus::Done
    }
}

pub(super) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}
