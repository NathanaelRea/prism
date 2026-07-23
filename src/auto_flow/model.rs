use super::*;

pub(super) const MAX_LOCAL_VERIFY_ATTEMPTS: usize = 3;
pub(super) const MAX_REVIEW_FIX_ATTEMPTS: usize = 3;
pub(super) const MAX_CI_FIX_ATTEMPTS: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoRun {
    pub id: String,
    pub harness_id: String,
    pub adapter_id: String,
    pub repo_root: String,
    pub worktree_path: PathBuf,
    pub worktree_incarnation: Option<String>,
    pub branch: String,
    pub mode: AutoRunMode,
    pub implementation_source: AutoImplementationSource,
    pub plan_path: Option<PathBuf>,
    pub plan_run_mode: PlanRunMode,
    pub variant: String,
    pub agent_profile: Option<String>,
    pub prompt_summary: String,
    pub initial_prompt: String,
    pub status: AutoRunStatus,
    pub pause_requested: bool,
    pub selected_step_run_id: Option<i64>,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
    pub current_head_sha: Option<String>,
    pub review_baseline_json: Option<String>,
    pub stabilization_status: Option<stabilization_model::StabilizationStatus>,
    pub stabilization_blocker: Option<stabilization_model::StabilizationBlocker>,
    pub stabilization_next_work: Option<stabilization_model::StabilizationWorkKind>,
    pub pending_push: Option<stabilization_model::PendingPushGuard>,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
    pub archived_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoStepRun {
    pub id: Option<i64>,
    pub run_id: String,
    pub sequence: usize,
    pub step_key: AutoStepKey,
    pub reason: Option<String>,
    pub status: AutoStepStatus,
    pub attempt: usize,
    pub started_unix_ms: Option<u64>,
    pub finished_unix_ms: Option<u64>,
    pub execution: crate::harness::ExecutionRef,
    pub session: crate::harness::SessionRef,
    pub plan_run_id: Option<String>,
    pub commit_sha: Option<String>,
    pub head_sha: Option<String>,
    pub work_guard: Option<stabilization_model::WorkGuard>,
    pub blocker: Option<stabilization_model::StabilizationBlocker>,
    pub summary: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoOutputLine {
    pub step_run_id: i64,
    pub line_number: u64,
    pub time_unix_ms: u64,
    pub kind: AutoOutputKind,
    pub text: String,
    pub block_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoEvent {
    pub id: Option<i64>,
    pub run_id: String,
    pub step_run_id: Option<i64>,
    pub time_unix_ms: u64,
    pub kind: String,
    pub data_json: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoLaunch {
    pub repo_root: String,
    pub worktree_path: PathBuf,
    worktree_incarnation: Option<String>,
    pub branch: String,
    pub mode: AutoRunMode,
    pub implementation_source: AutoImplementationSource,
    pub plan_path: Option<PathBuf>,
    pub plan_run_mode: PlanRunMode,
    pub variant: String,
    pub agent_profile: Option<String>,
    pub prompt_summary: String,
    pub initial_prompt: String,
    harness_id: String,
    adapter_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoLaunchOptions {
    pub branch: String,
    pub mode: AutoRunMode,
    pub implementation_source: AutoImplementationSource,
    pub plan_path: Option<PathBuf>,
    pub plan_run_mode: PlanRunMode,
    pub variant: String,
    pub agent_profile: Option<String>,
    pub initial_prompt: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedAutoRun {
    pub run: AutoRun,
    pub steps: Vec<AutoStepRun>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoExecutorConfig {
    pub harness_id: String,
    pub harness_config: crate::harness::HarnessConfig,
    pub server_url: Option<String>,
    pub worktree_path: PathBuf,
    pub title_prefix: String,
    pub max_output_lines_per_step: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoRunMode {
    Standard,
    PlanFirst,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoImplementationSource {
    Prompt,
    ExistingPlan,
    DraftPlan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoRunStatus {
    Queued,
    Running,
    Paused,
    Done,
    Failed,
    Aborted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoStepKey {
    Prepare,
    CreatePlan,
    ReviewPlan,
    ApprovePlan,
    RunPlan,
    Implement,
    LocalVerify,
    FixLocalVerify,
    CommitImpl,
    PushPr,
    WaitReview,
    FixReview,
    VerifyReviewFix,
    CommitReviewFix,
    WaitCi,
    FixCi,
    VerifyCiFix,
    CommitCiFix,
    Merge,
    Cleanup,
    Custom(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoStepStatus {
    Queued,
    Starting,
    Running,
    Waiting,
    Done,
    Failed,
    Aborted,
    Skipped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoOutputKind {
    Assistant,
    Tool,
    ToolOutput,
    Diff,
    Status,
    System,
    Error,
    RawJson,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutoStatusCounts {
    pub queued: usize,
    pub starting: usize,
    pub running: usize,
    pub waiting: usize,
    pub done: usize,
    pub failed: usize,
    pub aborted: usize,
    pub skipped: usize,
}

impl AutoRunMode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::PlanFirst => "plan_first",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "standard" => Ok(Self::Standard),
            "plan_first" => Ok(Self::PlanFirst),
            _ => Err(format!("unknown auto run mode: {value}")),
        }
    }
}

impl AutoImplementationSource {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::ExistingPlan => "existing_plan",
            Self::DraftPlan => "draft_plan",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "prompt" => Ok(Self::Prompt),
            "existing_plan" => Ok(Self::ExistingPlan),
            "draft_plan" => Ok(Self::DraftPlan),
            _ => Err(format!("unknown auto implementation source: {value}")),
        }
    }
}

impl AutoRunStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
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
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "aborted" => Ok(Self::Aborted),
            _ => Err(format!("unknown auto run status: {value}")),
        }
    }
}

impl AutoStepKey {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Prepare => "prepare",
            Self::CreatePlan => "create_plan",
            Self::ReviewPlan => "review_plan",
            Self::ApprovePlan => "approve_plan",
            Self::RunPlan => "run_plan",
            Self::Implement => "implement",
            Self::LocalVerify => "local_verify",
            Self::FixLocalVerify => "fix_local_verify",
            Self::CommitImpl => "commit_impl",
            Self::PushPr => "push_pr",
            Self::WaitReview => "wait_review",
            Self::FixReview => "fix_review",
            Self::VerifyReviewFix => "verify_review_fix",
            Self::CommitReviewFix => "commit_review_fix",
            Self::WaitCi => "wait_ci",
            Self::FixCi => "fix_ci",
            Self::VerifyCiFix => "verify_ci_fix",
            Self::CommitCiFix => "commit_ci_fix",
            Self::Merge => "merge",
            Self::Cleanup => "cleanup",
            Self::Custom(value) => value.as_str(),
        }
    }

    pub(super) fn parse(value: &str) -> Self {
        match value {
            "prepare" => Self::Prepare,
            "create_plan" => Self::CreatePlan,
            "review_plan" => Self::ReviewPlan,
            "approve_plan" => Self::ApprovePlan,
            "run_plan" => Self::RunPlan,
            "implement" => Self::Implement,
            "local_verify" => Self::LocalVerify,
            "fix_local_verify" => Self::FixLocalVerify,
            "commit_impl" => Self::CommitImpl,
            "push_pr" => Self::PushPr,
            "wait_review" => Self::WaitReview,
            "fix_review" => Self::FixReview,
            "verify_review_fix" => Self::VerifyReviewFix,
            "commit_review_fix" => Self::CommitReviewFix,
            "wait_ci" => Self::WaitCi,
            "fix_ci" => Self::FixCi,
            "verify_ci_fix" => Self::VerifyCiFix,
            "commit_ci_fix" => Self::CommitCiFix,
            "merge" => Self::Merge,
            "cleanup" => Self::Cleanup,
            other => Self::Custom(other.to_string()),
        }
    }
}

impl AutoStepStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Waiting => "waiting",
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
            "waiting" => Ok(Self::Waiting),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "aborted" => Ok(Self::Aborted),
            "skipped" => Ok(Self::Skipped),
            _ => Err(format!("unknown auto step status: {value}")),
        }
    }
}

impl AutoOutputKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::ToolOutput => "tool_output",
            Self::Diff => "diff",
            Self::Status => "status",
            Self::System => "system",
            Self::Error => "error",
            Self::RawJson => "raw_json",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "assistant" => Ok(Self::Assistant),
            "tool" => Ok(Self::Tool),
            "tool_output" => Ok(Self::ToolOutput),
            "diff" => Ok(Self::Diff),
            "status" => Ok(Self::Status),
            "system" => Ok(Self::System),
            "error" => Ok(Self::Error),
            "raw_json" => Ok(Self::RawJson),
            _ => Err(format!("unknown auto output kind: {value}")),
        }
    }
}

impl AutoLaunch {
    pub fn new(
        repo_root: &Path,
        worktree_path: &Path,
        branch: impl Into<String>,
        initial_prompt: impl Into<String>,
    ) -> Result<Self, String> {
        Self::with_options(
            repo_root,
            worktree_path,
            AutoLaunchOptions {
                branch: branch.into(),
                mode: AutoRunMode::Standard,
                implementation_source: AutoImplementationSource::Prompt,
                plan_path: None,
                plan_run_mode: PlanRunMode::Sequential,
                variant: "default".to_string(),
                agent_profile: None,
                initial_prompt: initial_prompt.into(),
            },
        )
    }

    pub fn with_options(
        repo_root: &Path,
        worktree_path: &Path,
        options: AutoLaunchOptions,
    ) -> Result<Self, String> {
        let AutoLaunchOptions {
            branch,
            mode,
            implementation_source,
            plan_path,
            plan_run_mode,
            variant,
            agent_profile,
            initial_prompt,
        } = options;
        if branch.trim().is_empty() {
            return Err("auto flow branch cannot be empty".to_string());
        }
        if initial_prompt.trim().is_empty() {
            return Err("auto flow prompt cannot be empty".to_string());
        }
        if implementation_source == AutoImplementationSource::ExistingPlan && plan_path.is_none() {
            return Err("existing-plan auto flow requires a plan path".to_string());
        }
        if implementation_source == AutoImplementationSource::Prompt && plan_path.is_some() {
            return Err("prompt auto flow cannot have a plan path".to_string());
        }
        if variant.trim().is_empty() {
            return Err("auto flow variant cannot be empty".to_string());
        }
        Ok(Self {
            repo_root: repo_root.display().to_string(),
            worktree_path: worktree_path.to_path_buf(),
            worktree_incarnation: nonempty_incarnation(crate::session::worktree_incarnation(
                worktree_path,
            )),
            branch,
            mode,
            implementation_source,
            plan_path,
            plan_run_mode,
            variant,
            agent_profile,
            prompt_summary: summarize_prompt(&initial_prompt),
            initial_prompt,
            harness_id: "opencode".to_string(),
            adapter_id: "opencode".to_string(),
        })
    }

    pub fn with_harness(
        mut self,
        harness_id: impl Into<String>,
        adapter_id: impl Into<String>,
    ) -> Self {
        self.harness_id = harness_id.into();
        self.adapter_id = adapter_id.into();
        self
    }

    pub(crate) fn with_worktree_incarnation(mut self, incarnation: String) -> Self {
        self.worktree_incarnation = nonempty_incarnation(incarnation);
        self
    }

    pub fn create_run(&self) -> PersistedAutoRun {
        let now = unix_ms();
        let id = self.default_run_id(now);
        let run = AutoRun {
            id: id.clone(),
            harness_id: self.harness_id.clone(),
            adapter_id: self.adapter_id.clone(),
            repo_root: self.repo_root.clone(),
            worktree_path: self.worktree_path.clone(),
            worktree_incarnation: self.worktree_incarnation.clone(),
            branch: self.branch.clone(),
            mode: self.mode,
            implementation_source: self.implementation_source,
            plan_path: self.plan_path.clone(),
            plan_run_mode: self.plan_run_mode,
            variant: self.variant.clone(),
            agent_profile: self.agent_profile.clone(),
            prompt_summary: self.prompt_summary.clone(),
            initial_prompt: self.initial_prompt.clone(),
            status: AutoRunStatus::Queued,
            pause_requested: false,
            selected_step_run_id: None,
            pr_number: None,
            pr_url: None,
            current_head_sha: None,
            review_baseline_json: None,
            stabilization_status: None,
            stabilization_blocker: None,
            stabilization_next_work: None,
            pending_push: None,
            created_unix_ms: now,
            updated_unix_ms: now,
            archived_unix_ms: None,
        };
        let steps = vec![AutoStepRun::queued(
            &id,
            1,
            AutoStepKey::Prepare,
            1,
            Some("validate selected worktree and save launch metadata".to_string()),
        )];
        PersistedAutoRun { run, steps }
    }

    pub(super) fn default_run_id(&self, now: u64) -> String {
        format!(
            "auto-{:016x}-{now}",
            crate::util::stable_hash(&self.worktree_path)
                ^ stable_string_hash(&self.branch)
                ^ stable_string_hash(&self.initial_prompt)
        )
    }
}

fn nonempty_incarnation(incarnation: String) -> Option<String> {
    (!incarnation.is_empty()).then_some(incarnation)
}

impl AutoStepRun {
    pub fn queued(
        run_id: &str,
        sequence: usize,
        step_key: AutoStepKey,
        attempt: usize,
        reason: Option<String>,
    ) -> Self {
        Self {
            id: None,
            run_id: run_id.to_string(),
            sequence,
            step_key,
            reason,
            status: AutoStepStatus::Queued,
            attempt,
            started_unix_ms: None,
            finished_unix_ms: None,
            execution: crate::harness::ExecutionRef::default(),
            session: crate::harness::SessionRef::default(),
            plan_run_id: None,
            commit_sha: None,
            head_sha: None,
            work_guard: None,
            blocker: None,
            summary: None,
            error: None,
        }
    }

    pub fn running(run_id: &str, sequence: usize, step_key: AutoStepKey, attempt: usize) -> Self {
        let mut step = Self::queued(run_id, sequence, step_key, attempt, None);
        step.status = AutoStepStatus::Running;
        step.started_unix_ms = Some(unix_ms());
        step
    }
}

impl AutoExecutorConfig {
    pub fn new(
        opencode_program: impl Into<String>,
        server_url: Option<String>,
        worktree_path: impl Into<PathBuf>,
        title_prefix: impl Into<String>,
    ) -> Self {
        let opencode_program = opencode_program.into();
        Self {
            harness_id: "opencode".to_string(),
            harness_config: crate::harness::HarnessConfig::opencode(opencode_program.clone()),
            server_url,
            worktree_path: worktree_path.into(),
            title_prefix: title_prefix.into(),
            max_output_lines_per_step: DEFAULT_OUTPUT_LINES_PER_STEP,
        }
    }

    pub fn for_harness(
        harness_id: impl Into<String>,
        harness_config: crate::harness::HarnessConfig,
        server_url: Option<String>,
        worktree_path: impl Into<PathBuf>,
        title_prefix: impl Into<String>,
    ) -> Self {
        Self {
            harness_id: harness_id.into(),
            harness_config,
            server_url,
            worktree_path: worktree_path.into(),
            title_prefix: title_prefix.into(),
            max_output_lines_per_step: DEFAULT_OUTPUT_LINES_PER_STEP,
        }
    }
}

impl PersistedAutoRun {
    pub fn aggregate_status(&self) -> AutoRunStatus {
        aggregate_step_status(self.steps.iter().map(|step| step.status))
    }

    pub fn authoritative_status(&self) -> AutoRunStatus {
        let aggregate = self.aggregate_status();
        let stabilization_active = self
            .run
            .stabilization_status
            .is_some_and(stabilization_model::StabilizationStatus::keeps_run_active);
        if self.run.pending_push.is_some()
            || (stabilization_active && matches!(aggregate, AutoRunStatus::Done))
        {
            AutoRunStatus::Paused
        } else {
            aggregate
        }
    }

    pub fn status_counts(&self) -> AutoStatusCounts {
        AutoStatusCounts::from_steps(&self.steps)
    }

    pub fn next_sequence(&self) -> usize {
        self.steps
            .iter()
            .map(|step| step.sequence)
            .max()
            .unwrap_or(0)
            + 1
    }

    pub fn next_attempt_for(&self, step_key: &AutoStepKey) -> usize {
        self.steps
            .iter()
            .filter(|step| step.step_key.as_str() == step_key.as_str())
            .map(|step| step.attempt)
            .max()
            .unwrap_or(0)
            + 1
    }
}

impl AutoStatusCounts {
    pub fn from_steps<'a>(steps: impl IntoIterator<Item = &'a AutoStepRun>) -> Self {
        let mut counts = Self::default();
        for step in steps {
            match step.status {
                AutoStepStatus::Queued => counts.queued += 1,
                AutoStepStatus::Starting => counts.starting += 1,
                AutoStepStatus::Running => counts.running += 1,
                AutoStepStatus::Waiting => counts.waiting += 1,
                AutoStepStatus::Done => counts.done += 1,
                AutoStepStatus::Failed => counts.failed += 1,
                AutoStepStatus::Aborted => counts.aborted += 1,
                AutoStepStatus::Skipped => counts.skipped += 1,
            }
        }
        counts
    }
}

pub fn aggregate_step_status(statuses: impl IntoIterator<Item = AutoStepStatus>) -> AutoRunStatus {
    let mut saw_status = false;
    let mut has_queued = false;
    let mut has_running = false;
    let mut has_failed = false;
    let mut has_aborted = false;
    for status in statuses {
        saw_status = true;
        match status {
            AutoStepStatus::Failed => has_failed = true,
            AutoStepStatus::Aborted => has_aborted = true,
            AutoStepStatus::Starting | AutoStepStatus::Running | AutoStepStatus::Waiting => {
                has_running = true;
            }
            AutoStepStatus::Queued => has_queued = true,
            AutoStepStatus::Done | AutoStepStatus::Skipped => {}
        }
    }
    if has_failed {
        AutoRunStatus::Failed
    } else if has_aborted {
        AutoRunStatus::Aborted
    } else if has_running {
        AutoRunStatus::Running
    } else if has_queued || !saw_status {
        AutoRunStatus::Queued
    } else {
        AutoRunStatus::Done
    }
}
