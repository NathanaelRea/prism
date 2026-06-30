use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::github::{PrCheckState, PrDetails, PrSummary};
use crate::observability::{self, LogLevel};
use crate::plan::PlanExecution;
use crate::plan_run::{
    DEFAULT_OUTPUT_LINES_PER_STEP, PlanAgentEvent, PlanExecutorConfig, PlanRunMode, PlanRunStatus,
    execute_plan_parallel, execute_plan_sequential, load_plan_run, prepare_plan_plugin_config,
    prepare_plan_run_for_resume, request_plan_run_pause,
    retry_failed_steps as retry_plan_failed_steps, retry_from_step as retry_plan_from_step,
    save_plan_run,
};
use crate::repo::Repository;
use crate::review::{ReviewFeedback, ReviewFeedbackFilter, actionable_review_feedback};
use crate::verify::{VerifyMode, VerifyResult};

const MAX_LOCAL_VERIFY_ATTEMPTS: usize = 3;
const MAX_REVIEW_FIX_ATTEMPTS: usize = 3;
const MAX_CI_FIX_ATTEMPTS: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoRun {
    pub id: String,
    pub repo_root: String,
    pub worktree_path: PathBuf,
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
    pub opencode_server_url: Option<String>,
    pub opencode_session_id: Option<String>,
    pub process_id: Option<u32>,
    pub plan_run_id: Option<String>,
    pub commit_sha: Option<String>,
    pub head_sha: Option<String>,
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
    pub branch: String,
    pub mode: AutoRunMode,
    pub implementation_source: AutoImplementationSource,
    pub plan_path: Option<PathBuf>,
    pub plan_run_mode: PlanRunMode,
    pub variant: String,
    pub agent_profile: Option<String>,
    pub prompt_summary: String,
    pub initial_prompt: String,
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
    pub opencode_program: String,
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
    fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::PlanFirst => "plan_first",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "standard" => Ok(Self::Standard),
            "plan_first" => Ok(Self::PlanFirst),
            _ => Err(format!("unknown auto run mode: {value}")),
        }
    }
}

impl AutoImplementationSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::ExistingPlan => "existing_plan",
            Self::DraftPlan => "draft_plan",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "prompt" => Ok(Self::Prompt),
            "existing_plan" => Ok(Self::ExistingPlan),
            "draft_plan" => Ok(Self::DraftPlan),
            _ => Err(format!("unknown auto implementation source: {value}")),
        }
    }
}

impl AutoRunStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
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

    fn parse(value: &str) -> Self {
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
    fn as_str(self) -> &'static str {
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

    fn parse(value: &str) -> Result<Self, String> {
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
    fn as_str(self) -> &'static str {
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

    fn parse(value: &str) -> Result<Self, String> {
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
            branch,
            mode,
            implementation_source,
            plan_path,
            plan_run_mode,
            variant,
            agent_profile,
            prompt_summary: summarize_prompt(&initial_prompt),
            initial_prompt,
        })
    }

    pub fn create_run(&self) -> PersistedAutoRun {
        let now = unix_ms();
        let id = self.default_run_id(now);
        let run = AutoRun {
            id: id.clone(),
            repo_root: self.repo_root.clone(),
            worktree_path: self.worktree_path.clone(),
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

    fn default_run_id(&self, now: u64) -> String {
        format!(
            "auto-{:016x}-{now}",
            crate::util::stable_hash(&self.worktree_path)
                ^ stable_string_hash(&self.branch)
                ^ stable_string_hash(&self.initial_prompt)
        )
    }
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
            opencode_server_url: None,
            opencode_session_id: None,
            process_id: None,
            plan_run_id: None,
            commit_sha: None,
            head_sha: None,
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
        Self {
            opencode_program: opencode_program.into(),
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

pub fn migrate_schema(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        create table if not exists auto_run (
          id text primary key,
          repo_root text not null,
          worktree_path text not null,
          branch text not null,
          mode text not null,
          implementation_source text not null default 'prompt',
          plan_path text,
          plan_run_mode text not null default 'sequential',
          variant text not null,
          agent_profile text,
          prompt_summary text not null,
          initial_prompt text not null,
          status text not null,
          pause_requested integer not null default 0,
          selected_step_run_id integer,
          pr_number integer,
          pr_url text,
          current_head_sha text,
          review_baseline_json text,
          created_unix_ms integer not null,
          updated_unix_ms integer not null,
          archived_unix_ms integer,
          foreign key (selected_step_run_id) references auto_step_run(id) on delete set null
        );

        create table if not exists auto_step_run (
          id integer primary key autoincrement,
          run_id text not null references auto_run(id) on delete cascade,
          sequence integer not null,
          step_key text not null,
          reason text,
          status text not null,
          attempt integer not null,
          started_unix_ms integer,
          finished_unix_ms integer,
          opencode_server_url text,
          opencode_session_id text,
          process_id integer,
          plan_run_id text,
          commit_sha text,
          head_sha text,
          summary text,
          error text,
          unique(run_id, sequence)
        );

        create table if not exists auto_output_line (
          step_run_id integer not null references auto_step_run(id) on delete cascade,
          line_number integer not null,
          time_unix_ms integer not null,
          kind text not null,
          text text not null,
          block_id text,
          primary key (step_run_id, line_number)
        );

        create table if not exists auto_event (
          id integer primary key autoincrement,
          run_id text not null references auto_run(id) on delete cascade,
          step_run_id integer references auto_step_run(id) on delete set null,
          time_unix_ms integer not null,
          kind text not null,
          data_json text not null
        );

        create index if not exists auto_run_repo_idx
          on auto_run(repo_root, updated_unix_ms);
        create index if not exists auto_run_worktree_idx
          on auto_run(worktree_path, updated_unix_ms);
        create index if not exists auto_run_status_idx
          on auto_run(status, updated_unix_ms);
        create index if not exists auto_step_run_run_idx
          on auto_step_run(run_id, sequence);
        create index if not exists auto_step_run_key_idx
          on auto_step_run(run_id, step_key, attempt);
        create index if not exists auto_output_line_step_idx
          on auto_output_line(step_run_id, line_number);
        create index if not exists auto_event_run_idx
          on auto_event(run_id, time_unix_ms);
        ",
    )
    .map_err(|error| format!("create auto flow schema: {error}"))?;
    if !table_has_column(conn, "auto_run", "pr_url")? {
        conn.execute("alter table auto_run add column pr_url text", [])
            .map_err(|error| format!("migrate auto_run pr_url column: {error}"))?;
    }
    if !table_has_column(conn, "auto_run", "implementation_source")? {
        conn.execute(
            "alter table auto_run add column implementation_source text not null default 'prompt'",
            [],
        )
        .map_err(|error| format!("migrate auto_run implementation_source column: {error}"))?;
        conn.execute(
            "update auto_run
             set implementation_source = case mode
               when 'plan_first' then 'draft_plan'
               else 'prompt'
             end",
            [],
        )
        .map_err(|error| format!("backfill auto_run implementation_source: {error}"))?;
    }
    if !table_has_column(conn, "auto_run", "plan_path")? {
        conn.execute("alter table auto_run add column plan_path text", [])
            .map_err(|error| format!("migrate auto_run plan_path column: {error}"))?;
    }
    if !table_has_column(conn, "auto_run", "plan_run_mode")? {
        conn.execute(
            "alter table auto_run add column plan_run_mode text not null default 'sequential'",
            [],
        )
        .map_err(|error| format!("migrate auto_run plan_run_mode column: {error}"))?;
    }
    if !table_has_column(conn, "auto_step_run", "plan_run_id")? {
        conn.execute("alter table auto_step_run add column plan_run_id text", [])
            .map_err(|error| format!("migrate auto_step_run plan_run_id column: {error}"))?;
    }
    Ok(())
}

pub fn save_auto_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<(), String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| format!("begin auto run transaction: {error}"))?;
    let mut run_without_selection = persisted.run.clone();
    run_without_selection.selected_step_run_id = None;
    save_run_with_conn(&tx, &run_without_selection)?;
    for step in &mut persisted.steps {
        save_step_with_conn(&tx, step)?;
    }
    save_run_with_conn(&tx, &persisted.run)?;
    tx.commit()
        .map_err(|error| format!("commit auto run transaction: {error}"))?;
    Ok(())
}

pub fn load_auto_run(
    conn: &rusqlite::Connection,
    run_id: &str,
) -> Result<Option<PersistedAutoRun>, String> {
    let run = load_run_with_conn(conn, run_id)?;
    let Some(run) = run else {
        return Ok(None);
    };
    let steps = load_steps_with_conn(conn, run_id)?;
    Ok(Some(PersistedAutoRun { run, steps }))
}

pub fn load_recent_active_runs_for_repo(
    conn: &rusqlite::Connection,
    repo_root: &Path,
    limit: usize,
) -> Result<Vec<PersistedAutoRun>, String> {
    let mut statement = conn
        .prepare(
            "select id
             from auto_run
             where repo_root = ?1
               and archived_unix_ms is null
               and status in ('queued', 'running', 'paused', 'failed')
             order by
               case status
                  when 'running' then 0
                  when 'queued' then 1
                  when 'paused' then 2
                  when 'failed' then 3
                  else 4
                end,
               updated_unix_ms desc
             limit ?2",
        )
        .map_err(|error| format!("prepare recent auto run load: {error}"))?;
    let ids = statement
        .query_map(
            params![repo_root.display().to_string(), usize_to_i64(limit)],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| format!("load recent auto run ids: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read recent auto run ids: {error}"))?;
    ids.into_iter()
        .filter_map(|id| load_auto_run(conn, &id).transpose())
        .collect()
}

pub fn append_step_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    step_key: AutoStepKey,
    reason: Option<String>,
) -> Result<i64, String> {
    let mut step = AutoStepRun::queued(
        &persisted.run.id,
        persisted.next_sequence(),
        step_key.clone(),
        persisted.next_attempt_for(&step_key),
        reason,
    );
    let id = save_step_with_conn(conn, &mut step)?;
    persisted.run.selected_step_run_id = Some(id);
    persisted.steps.push(step);
    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)?;
    Ok(id)
}

pub fn request_auto_run_pause(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<(), String> {
    if matches!(
        persisted.run.status,
        AutoRunStatus::Done | AutoRunStatus::Failed | AutoRunStatus::Aborted
    ) {
        return Err("cannot pause a completed auto flow run".to_string());
    }
    persisted.run.pause_requested = true;
    if !persisted.steps.iter().any(|step| {
        matches!(
            step.status,
            AutoStepStatus::Starting | AutoStepStatus::Running | AutoStepStatus::Waiting
        )
    }) {
        persisted.run.status = AutoRunStatus::Paused;
    }
    request_active_linked_plan_pause(conn, persisted)?;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub fn resume_paused_auto_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<(), String> {
    if !persisted.run.pause_requested && persisted.run.status != AutoRunStatus::Paused {
        return Err("auto flow run is not paused".to_string());
    }
    persisted.run.pause_requested = false;
    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub fn fail_auto_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    error: impl Into<String>,
) -> Result<(), String> {
    persisted.run.pause_requested = false;
    persisted.run.status = AutoRunStatus::Failed;
    persisted.run.updated_unix_ms = unix_ms();
    let error = error.into();
    append_auto_event(
        conn,
        &AutoEvent {
            id: None,
            run_id: persisted.run.id.clone(),
            step_run_id: persisted.run.selected_step_run_id,
            time_unix_ms: persisted.run.updated_unix_ms,
            kind: "run_failed".to_string(),
            data_json: format!("{{\"error\":{}}}", json_string(&error)),
        },
    )?;
    save_run_with_conn(conn, &persisted.run)
}

pub fn retry_failed_auto_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<(), String> {
    let failed_index = persisted
        .steps
        .iter()
        .rposition(|step| {
            matches!(
                step.status,
                AutoStepStatus::Failed | AutoStepStatus::Aborted
            )
        })
        .ok_or_else(|| "auto flow run has no failed step to retry".to_string())?;
    if persisted.steps[failed_index].step_key == AutoStepKey::RunPlan
        && let Some(plan_run_id) = persisted.steps[failed_index].plan_run_id.clone()
        && let Some(mut plan_run) = load_plan_run(conn, &plan_run_id)?
    {
        retry_plan_failed_steps(conn, &mut plan_run)?;
        reset_auto_step_for_retry(&mut persisted.steps[failed_index]);
        append_step_status_output(
            conn,
            &persisted.steps[failed_index],
            "retrying linked plan run failed phases",
            DEFAULT_OUTPUT_LINES_PER_STEP,
        )?;
        save_step_with_conn(conn, &mut persisted.steps[failed_index])?;
        persisted.run.pause_requested = false;
        persisted.run.status = persisted.aggregate_status();
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        return Ok(());
    }

    let step_key = persisted.steps[failed_index].step_key.clone();
    append_step_run(conn, persisted, step_key, Some("manual retry".to_string()))?;
    Ok(())
}

pub fn retry_auto_from_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    selected_step_run_id: i64,
) -> Result<(), String> {
    let selected_index = persisted
        .steps
        .iter()
        .position(|step| step.id == Some(selected_step_run_id))
        .ok_or_else(|| format!("auto flow step not found: {selected_step_run_id}"))?;
    let selected_sequence = persisted.steps[selected_index].sequence;
    if persisted.steps[selected_index].step_key == AutoStepKey::RunPlan
        && let Some(plan_run_id) = persisted.steps[selected_index].plan_run_id.clone()
        && let Some(mut plan_run) = load_plan_run(conn, &plan_run_id)?
    {
        let start_step = plan_run.run.start_step;
        retry_plan_from_step(conn, &mut plan_run, start_step)?;
    }
    for step in persisted
        .steps
        .iter_mut()
        .filter(|step| step.sequence >= selected_sequence)
    {
        reset_auto_step_for_retry(step);
        save_step_with_conn(conn, step)?;
    }
    persisted.run.selected_step_run_id = persisted.steps[selected_index].id;
    persisted.run.pause_requested = false;
    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub fn archive_auto_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<(), String> {
    if matches!(
        persisted.run.status,
        AutoRunStatus::Queued | AutoRunStatus::Running | AutoRunStatus::Paused
    ) {
        return Err("cannot archive a queued or running auto flow run".to_string());
    }
    let now = unix_ms();
    persisted.run.archived_unix_ms = Some(now);
    persisted.run.updated_unix_ms = now;
    save_run_with_conn(conn, &persisted.run)
}

pub fn execute_auto_initial_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    executor: &AutoExecutorConfig,
    output: &mut dyn Write,
) -> Result<(), String> {
    persisted.run.pause_requested = false;
    persisted.run.status = AutoRunStatus::Running;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)?;

    complete_queued_prepare(conn, persisted, executor.max_output_lines_per_step)?;
    if !persisted.steps.iter().any(|step| {
        matches!(
            step.step_key,
            AutoStepKey::CreatePlan
                | AutoStepKey::ReviewPlan
                | AutoStepKey::RunPlan
                | AutoStepKey::Implement
                | AutoStepKey::FixLocalVerify
                | AutoStepKey::FixReview
                | AutoStepKey::FixCi
        )
    }) {
        let (step_key, reason) = initial_agent_step(persisted);
        append_step_run(conn, persisted, step_key, Some(reason.to_string()))?;
    }

    if reload_pause_request(conn, persisted)? {
        return Ok(());
    }

    loop {
        if reload_pause_request(conn, persisted)? {
            return Ok(());
        }

        if let Some(step_index) = next_queued_agent_step(persisted) {
            if let Err(error) =
                execute_one_agent_step(conn, persisted, step_index, executor, output)
            {
                persisted.run.status = AutoRunStatus::Failed;
                persisted.run.pause_requested = false;
                persisted.run.updated_unix_ms = unix_ms();
                save_run_with_conn(conn, &persisted.run)?;
                return Err(error);
            }
            continue;
        }

        if let Some(step_index) = next_queued_non_agent_step(persisted) {
            if let Err(error) = execute_one_non_agent_step(
                conn,
                repo,
                config,
                persisted,
                step_index,
                executor.max_output_lines_per_step,
            ) {
                persisted.run.status = AutoRunStatus::Failed;
                persisted.run.pause_requested = false;
                persisted.run.updated_unix_ms = unix_ms();
                save_run_with_conn(conn, &persisted.run)?;
                return Err(error);
            }
            continue;
        }

        if ensure_next_auto_step(conn, persisted)? {
            continue;
        }

        persisted.run.pause_requested = false;
        persisted.run.status = persisted.aggregate_status();
        if matches!(persisted.run.status, AutoRunStatus::Queued) {
            persisted.run.status = AutoRunStatus::Paused;
        }
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        return Ok(());
    }
}

pub fn abort_auto_step(conn: &rusqlite::Connection, step: &mut AutoStepRun) -> Result<(), String> {
    let mut errors = Vec::new();
    if let (Some(server_url), Some(session_id)) = (
        step.opencode_server_url.as_deref(),
        step.opencode_session_id.as_deref(),
    ) && let Err(error) = crate::opencode::abort_session(server_url, session_id)
    {
        errors.push(error);
    }
    if let Some(process_id) = step.process_id
        && let Err(error) = terminate_process(process_id)
    {
        errors.push(error);
    }
    step.status = AutoStepStatus::Aborted;
    step.process_id = None;
    step.finished_unix_ms = Some(unix_ms());
    step.error = if errors.is_empty() {
        Some("aborted".to_string())
    } else {
        Some(format!("aborted with errors: {}", errors.join("; ")))
    };
    save_step_with_conn(conn, step)?;
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub fn reconcile_stale_auto_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<bool, String> {
    let mut changed = false;
    for step in &mut persisted.steps {
        if !matches!(
            step.status,
            AutoStepStatus::Starting | AutoStepStatus::Running | AutoStepStatus::Waiting
        ) {
            continue;
        }
        if step.step_key == AutoStepKey::RunPlan && step.plan_run_id.is_some() {
            continue;
        }
        let message = match step.process_id {
            Some(process_id) => format!(
                "Prism restarted while auto flow step {} attempt {} was active in process {process_id}; the attempt was marked failed for retry.",
                step.step_key.as_str(),
                step.attempt
            ),
            None => format!(
                "Prism restarted while auto flow step {} attempt {} was active, but no child process id was recorded.",
                step.step_key.as_str(),
                step.attempt
            ),
        };
        step.status = AutoStepStatus::Failed;
        step.process_id = None;
        step.finished_unix_ms = Some(unix_ms());
        step.error = Some(message.clone());
        save_step_with_conn(conn, step)?;
        if let Some(step_run_id) = step.id {
            append_output_line(
                conn,
                &AutoOutputLine {
                    step_run_id,
                    line_number: next_output_line_number(conn, step_run_id)?,
                    time_unix_ms: unix_ms(),
                    kind: AutoOutputKind::Error,
                    text: message,
                    block_id: None,
                },
            )?;
        }
        changed = true;
    }
    if matches!(
        persisted.run.status,
        AutoRunStatus::Queued | AutoRunStatus::Running | AutoRunStatus::Paused
    ) {
        persisted.run.pause_requested = false;
        persisted.run.status = persisted.aggregate_status();
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        changed = true;
    }
    Ok(changed)
}

fn reconcile_linked_plan_runs(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    max_output_lines_per_step: usize,
) -> Result<bool, String> {
    crate::plan_run::migrate_schema(conn)?;
    let mut changed = false;
    for index in 0..persisted.steps.len() {
        if persisted.steps[index].step_key != AutoStepKey::RunPlan {
            continue;
        }
        let Some(plan_run_id) = persisted.steps[index].plan_run_id.clone() else {
            continue;
        };
        let Some(mut plan_run) = load_plan_run(conn, &plan_run_id)? else {
            if matches!(
                persisted.steps[index].status,
                AutoStepStatus::Starting | AutoStepStatus::Running | AutoStepStatus::Waiting
            ) {
                let error = format!("linked plan run {plan_run_id} was not found");
                fail_step(
                    conn,
                    &mut persisted.steps[index],
                    &error,
                    max_output_lines_per_step,
                )?;
                changed = true;
            }
            continue;
        };
        let can_resume =
            prepare_plan_run_for_resume(conn, &mut plan_run, max_output_lines_per_step)?;
        let before = persisted.steps[index].status;
        match plan_run.run.status {
            PlanRunStatus::Done => {
                if persisted.steps[index].status != AutoStepStatus::Done {
                    let summary = format!("plan run {} completed", plan_run.run.id);
                    finish_non_agent_step(
                        conn,
                        &mut persisted.steps[index],
                        AutoStepStatus::Done,
                        Some(summary),
                        None,
                    )?;
                }
            }
            PlanRunStatus::Failed | PlanRunStatus::Aborted => {
                if persisted.steps[index].status != AutoStepStatus::Failed {
                    let error = format!(
                        "plan run {} ended with status {}; inspect linked plan dashboard",
                        plan_run.run.id,
                        plan_run_status_label(plan_run.run.status)
                    );
                    finish_non_agent_step(
                        conn,
                        &mut persisted.steps[index],
                        AutoStepStatus::Failed,
                        Some("plan run failed".to_string()),
                        Some(error),
                    )?;
                }
            }
            PlanRunStatus::Paused => {
                if persisted.steps[index].status != AutoStepStatus::Waiting {
                    let summary = format!(
                        "plan run {} paused; resume linked plan run",
                        plan_run.run.id
                    );
                    set_auto_step_waiting(conn, &mut persisted.steps[index], summary)?;
                }
            }
            PlanRunStatus::Draft | PlanRunStatus::Queued => {
                if can_resume && persisted.steps[index].status != AutoStepStatus::Queued {
                    reset_auto_step_for_retry(&mut persisted.steps[index]);
                    save_step_with_conn(conn, &mut persisted.steps[index])?;
                }
            }
            PlanRunStatus::Running => {
                if can_resume {
                    reset_auto_step_for_retry(&mut persisted.steps[index]);
                    save_step_with_conn(conn, &mut persisted.steps[index])?;
                } else if persisted.steps[index].status != AutoStepStatus::Waiting {
                    let summary = format!(
                        "plan run {} is running; Auto Flow is waiting",
                        plan_run.run.id
                    );
                    set_auto_step_waiting(conn, &mut persisted.steps[index], summary)?;
                }
            }
        }
        changed |= persisted.steps[index].status != before;
    }
    if changed {
        persisted.run.status = persisted.aggregate_status();
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
    }
    Ok(changed)
}

pub fn prepare_auto_run_for_resume(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    max_output_lines_per_step: usize,
) -> Result<bool, String> {
    let linked_changed = reconcile_linked_plan_runs(conn, persisted, max_output_lines_per_step)?;
    let changed = reconcile_stale_auto_run(conn, persisted)? || linked_changed;
    if matches!(persisted.run.status, AutoRunStatus::Paused) {
        persisted.run.pause_requested = false;
        persisted.run.status = persisted.aggregate_status();
        if matches!(persisted.run.status, AutoRunStatus::Done) {
            persisted.run.status = AutoRunStatus::Paused;
        }
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
    }
    if changed {
        append_auto_event(
            conn,
            &AutoEvent {
                id: None,
                run_id: persisted.run.id.clone(),
                step_run_id: persisted.run.selected_step_run_id,
                time_unix_ms: unix_ms(),
                kind: "resume_reconciled".to_string(),
                data_json: "{}".to_string(),
            },
        )?;
    }
    let has_queued_agent_step = persisted.steps.iter().any(|step| {
        step.status == AutoStepStatus::Queued
            && matches!(
                step.step_key,
                AutoStepKey::CreatePlan
                    | AutoStepKey::ReviewPlan
                    | AutoStepKey::RunPlan
                    | AutoStepKey::Implement
                    | AutoStepKey::FixLocalVerify
                    | AutoStepKey::FixReview
                    | AutoStepKey::FixCi
                    | AutoStepKey::Custom(_)
            )
    });
    if has_queued_agent_step
        || has_queued_non_agent_step(persisted)
        || queued_prepare_needs_initial_agent_step(persisted)
        || next_state_machine_step_needed(persisted)
        || implementation_follow_up_step_needed(persisted)
    {
        Ok(true)
    } else {
        let _ = max_output_lines_per_step;
        Ok(false)
    }
}

pub fn append_output_line(
    conn: &rusqlite::Connection,
    line: &AutoOutputLine,
) -> Result<(), String> {
    append_output_line_limited(conn, line, 0)
}

pub fn append_output_line_limited(
    conn: &rusqlite::Connection,
    line: &AutoOutputLine,
    max_lines_per_step: usize,
) -> Result<(), String> {
    conn.execute(
        "insert or replace into auto_output_line (
           step_run_id, line_number, time_unix_ms, kind, text, block_id
         ) values (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            line.step_run_id,
            u64_to_i64(line.line_number),
            u64_to_i64(line.time_unix_ms),
            line.kind.as_str(),
            line.text.as_str(),
            line.block_id.as_deref(),
        ],
    )
    .map_err(|error| format!("write auto output line: {error}"))?;
    trim_output_lines(conn, line.step_run_id, max_lines_per_step)
}

pub fn load_output_lines(
    conn: &rusqlite::Connection,
    step_run_id: i64,
) -> Result<Vec<AutoOutputLine>, String> {
    let mut statement = conn
        .prepare(
            "select step_run_id, line_number, time_unix_ms, kind, text, block_id
             from auto_output_line
             where step_run_id = ?1
             order by line_number",
        )
        .map_err(|error| format!("prepare auto output load: {error}"))?;
    let rows = statement
        .query_map(params![step_run_id], |row| {
            let kind: String = row.get(3)?;
            Ok(AutoOutputLine {
                step_run_id: row.get(0)?,
                line_number: i64_to_u64(row.get(1)?, 1),
                time_unix_ms: i64_to_u64(row.get(2)?, 2),
                kind: AutoOutputKind::parse(&kind).map_err(from_string_error)?,
                text: row.get(4)?,
                block_id: row.get(5)?,
            })
        })
        .map_err(|error| format!("load auto output lines: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read auto output lines: {error}"))
}

pub fn append_auto_event(conn: &rusqlite::Connection, event: &AutoEvent) -> Result<i64, String> {
    conn.execute(
        "insert into auto_event (
           run_id, step_run_id, time_unix_ms, kind, data_json
         ) values (?1, ?2, ?3, ?4, ?5)",
        params![
            event.run_id.as_str(),
            event.step_run_id,
            u64_to_i64(event.time_unix_ms),
            event.kind.as_str(),
            event.data_json.as_str(),
        ],
    )
    .map_err(|error| format!("write auto event: {error}"))?;
    emit_auto_event_log(event);
    Ok(conn.last_insert_rowid())
}

fn save_run_with_conn(conn: &rusqlite::Connection, run: &AutoRun) -> Result<(), String> {
    conn.execute(
        "insert into auto_run (
           id, repo_root, worktree_path, branch, mode, implementation_source, plan_path,
           plan_run_mode, variant, agent_profile, prompt_summary, initial_prompt, status, pause_requested,
           selected_step_run_id, pr_number, pr_url, current_head_sha, review_baseline_json,
           created_unix_ms, updated_unix_ms, archived_unix_ms
         ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
         on conflict(id) do update set
           repo_root = excluded.repo_root,
           worktree_path = excluded.worktree_path,
           branch = excluded.branch,
           mode = excluded.mode,
           implementation_source = excluded.implementation_source,
           plan_path = excluded.plan_path,
           plan_run_mode = excluded.plan_run_mode,
           variant = excluded.variant,
           agent_profile = excluded.agent_profile,
           prompt_summary = excluded.prompt_summary,
           initial_prompt = excluded.initial_prompt,
           status = excluded.status,
           pause_requested = excluded.pause_requested,
           selected_step_run_id = excluded.selected_step_run_id,
           pr_number = excluded.pr_number,
           pr_url = excluded.pr_url,
           current_head_sha = excluded.current_head_sha,
           review_baseline_json = excluded.review_baseline_json,
           updated_unix_ms = excluded.updated_unix_ms,
           archived_unix_ms = excluded.archived_unix_ms",
        params![
            run.id.as_str(),
            run.repo_root.as_str(),
            run.worktree_path.display().to_string(),
            run.branch.as_str(),
            run.mode.as_str(),
            run.implementation_source.as_str(),
            run.plan_path.as_ref().map(|path| path.display().to_string()),
            plan_run_mode_label(run.plan_run_mode),
            run.variant.as_str(),
            run.agent_profile.as_deref(),
            run.prompt_summary.as_str(),
            run.initial_prompt.as_str(),
            run.status.as_str(),
            bool_to_i64(run.pause_requested),
            run.selected_step_run_id,
            run.pr_number.map(u64_to_i64),
            run.pr_url.as_deref(),
            run.current_head_sha.as_deref(),
            run.review_baseline_json.as_deref(),
            u64_to_i64(run.created_unix_ms),
            u64_to_i64(run.updated_unix_ms),
            run.archived_unix_ms.map(u64_to_i64),
        ],
    )
    .map_err(|error| format!("write auto run: {error}"))?;
    emit_auto_run_log(run);
    Ok(())
}

fn save_step_with_conn(conn: &rusqlite::Connection, step: &mut AutoStepRun) -> Result<i64, String> {
    if let Some(id) = step.id {
        conn.execute(
            "update auto_step_run
             set run_id = ?1,
                 sequence = ?2,
                 step_key = ?3,
                 reason = ?4,
                 status = ?5,
                 attempt = ?6,
                 started_unix_ms = ?7,
                 finished_unix_ms = ?8,
                  opencode_server_url = ?9,
                  opencode_session_id = ?10,
                  process_id = ?11,
                  plan_run_id = ?12,
                  commit_sha = ?13,
                  head_sha = ?14,
                  summary = ?15,
                  error = ?16
             where id = ?17",
            params![
                step.run_id.as_str(),
                usize_to_i64(step.sequence),
                step.step_key.as_str(),
                step.reason.as_deref(),
                step.status.as_str(),
                usize_to_i64(step.attempt),
                step.started_unix_ms.map(u64_to_i64),
                step.finished_unix_ms.map(u64_to_i64),
                step.opencode_server_url.as_deref(),
                step.opencode_session_id.as_deref(),
                step.process_id.map(i64::from),
                step.plan_run_id.as_deref(),
                step.commit_sha.as_deref(),
                step.head_sha.as_deref(),
                step.summary.as_deref(),
                step.error.as_deref(),
                id,
            ],
        )
        .map_err(|error| format!("write auto step run: {error}"))?;
        emit_auto_step_log(step);
        Ok(id)
    } else {
        conn.execute(
            "insert into auto_step_run (
               run_id, sequence, step_key, reason, status, attempt, started_unix_ms,
               finished_unix_ms, opencode_server_url, opencode_session_id, process_id,
               plan_run_id, commit_sha, head_sha, summary, error
             ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                step.run_id.as_str(),
                usize_to_i64(step.sequence),
                step.step_key.as_str(),
                step.reason.as_deref(),
                step.status.as_str(),
                usize_to_i64(step.attempt),
                step.started_unix_ms.map(u64_to_i64),
                step.finished_unix_ms.map(u64_to_i64),
                step.opencode_server_url.as_deref(),
                step.opencode_session_id.as_deref(),
                step.process_id.map(i64::from),
                step.plan_run_id.as_deref(),
                step.commit_sha.as_deref(),
                step.head_sha.as_deref(),
                step.summary.as_deref(),
                step.error.as_deref(),
            ],
        )
        .map_err(|error| format!("write auto step run: {error}"))?;
        let id = conn.last_insert_rowid();
        step.id = Some(id);
        emit_auto_step_log(step);
        Ok(id)
    }
}

fn load_run_with_conn(
    conn: &rusqlite::Connection,
    run_id: &str,
) -> Result<Option<AutoRun>, String> {
    conn.query_row(
        "select id, repo_root, worktree_path, branch, mode, implementation_source, plan_path,
                plan_run_mode, variant, agent_profile, prompt_summary, initial_prompt, status, pause_requested,
                selected_step_run_id, pr_number, pr_url, current_head_sha, review_baseline_json,
                created_unix_ms, updated_unix_ms, archived_unix_ms
         from auto_run
         where id = ?1",
        params![run_id],
        |row| {
            let mode: String = row.get(4)?;
            let implementation_source: String = row.get(5)?;
            let plan_run_mode: String = row.get(7)?;
            let status: String = row.get(12)?;
            Ok(AutoRun {
                id: row.get(0)?,
                repo_root: row.get(1)?,
                worktree_path: PathBuf::from(row.get::<_, String>(2)?),
                branch: row.get(3)?,
                mode: AutoRunMode::parse(&mode).map_err(from_string_error)?,
                implementation_source: AutoImplementationSource::parse(&implementation_source)
                    .map_err(from_string_error)?,
                plan_path: row.get::<_, Option<String>>(6)?.map(PathBuf::from),
                plan_run_mode: parse_plan_run_mode(&plan_run_mode).map_err(from_string_error)?,
                variant: row.get(8)?,
                agent_profile: row.get(9)?,
                prompt_summary: row.get(10)?,
                initial_prompt: row.get(11)?,
                status: AutoRunStatus::parse(&status).map_err(from_string_error)?,
                pause_requested: row.get::<_, i64>(13)? != 0,
                selected_step_run_id: row.get(14)?,
                pr_number: row
                    .get::<_, Option<i64>>(15)?
                    .map(|value| value.max(0) as u64),
                pr_url: row.get(16)?,
                current_head_sha: row.get(17)?,
                review_baseline_json: row.get(18)?,
                created_unix_ms: i64_to_u64(row.get(19)?, 19),
                updated_unix_ms: i64_to_u64(row.get(20)?, 20),
                archived_unix_ms: row
                    .get::<_, Option<i64>>(21)?
                    .map(|value| value.max(0) as u64),
            })
        },
    )
    .optional()
    .map_err(|error| format!("load auto run: {error}"))
}

fn load_steps_with_conn(
    conn: &rusqlite::Connection,
    run_id: &str,
) -> Result<Vec<AutoStepRun>, String> {
    let mut statement = conn
        .prepare(
            "select id, run_id, sequence, step_key, reason, status, attempt, started_unix_ms,
                    finished_unix_ms, opencode_server_url, opencode_session_id, process_id,
                    plan_run_id, commit_sha, head_sha, summary, error
             from auto_step_run
             where run_id = ?1
             order by sequence",
        )
        .map_err(|error| format!("prepare auto step load: {error}"))?;
    let rows = statement
        .query_map(params![run_id], |row| {
            let step_key: String = row.get(3)?;
            let status: String = row.get(5)?;
            Ok(AutoStepRun {
                id: row.get(0)?,
                run_id: row.get(1)?,
                sequence: i64_to_usize(row.get(2)?, 2),
                step_key: AutoStepKey::parse(&step_key),
                reason: row.get(4)?,
                status: AutoStepStatus::parse(&status).map_err(from_string_error)?,
                attempt: i64_to_usize(row.get(6)?, 6),
                started_unix_ms: row
                    .get::<_, Option<i64>>(7)?
                    .map(|value| value.max(0) as u64),
                finished_unix_ms: row
                    .get::<_, Option<i64>>(8)?
                    .map(|value| value.max(0) as u64),
                opencode_server_url: row.get(9)?,
                opencode_session_id: row.get(10)?,
                process_id: row
                    .get::<_, Option<i64>>(11)?
                    .map(|value| value.max(0) as u32),
                plan_run_id: row.get(12)?,
                commit_sha: row.get(13)?,
                head_sha: row.get(14)?,
                summary: row.get(15)?,
                error: row.get(16)?,
            })
        })
        .map_err(|error| format!("load auto steps: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read auto steps: {error}"))
}

fn complete_queued_prepare(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    for step in &mut persisted.steps {
        if step.step_key != AutoStepKey::Prepare || step.status != AutoStepStatus::Queued {
            continue;
        }
        let now = unix_ms();
        step.status = AutoStepStatus::Done;
        step.started_unix_ms = Some(now);
        step.finished_unix_ms = Some(now);
        step.summary = Some("prepared worktree for headless execution".to_string());
        let step_id = save_step_with_conn(conn, step)?;
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::System,
            "prepared worktree for headless execution",
            None,
            max_output_lines_per_step,
        )?;
    }
    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

fn queued_prepare_needs_initial_agent_step(persisted: &PersistedAutoRun) -> bool {
    persisted
        .steps
        .iter()
        .any(|step| step.step_key == AutoStepKey::Prepare && step.status == AutoStepStatus::Queued)
        && !persisted.steps.iter().any(|step| {
            matches!(
                step.step_key,
                AutoStepKey::CreatePlan | AutoStepKey::RunPlan | AutoStepKey::Implement
            )
        })
}

fn execute_one_agent_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    executor: &AutoExecutorConfig,
    output: &mut dyn Write,
) -> Result<(), String> {
    {
        let step = &mut persisted.steps[step_index];
        step.status = AutoStepStatus::Starting;
        step.started_unix_ms = Some(unix_ms());
        step.finished_unix_ms = None;
        step.opencode_server_url = executor.server_url.clone();
        step.opencode_session_id = None;
        step.process_id = None;
        step.error = None;
        persisted.run.selected_step_run_id = step.id;
        persisted.run.status = AutoRunStatus::Running;
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        save_step_with_conn(conn, step)?;
    }

    let prompt = prompt_for_step(&persisted.run, &persisted.steps[step_index]);
    let label = persisted.steps[step_index].step_key.as_str().to_string();
    writeln!(
        output,
        "\n==> Auto Flow {label} attempt {}\n",
        persisted.steps[step_index].attempt
    )
    .map_err(|error| format!("write auto output: {error}"))?;

    let mut command = opencode_run_command(executor, &persisted.steps[step_index], &prompt, true);
    let spawn_result = spawn_opencode(&mut command);
    let (mut child, used_attach) = match spawn_result {
        Ok(child) => (child, true),
        Err(error) if executor.server_url.is_some() => {
            if let Some(step_id) = persisted.steps[step_index].id {
                append_system_output(
                    conn,
                    step_id,
                    AutoOutputKind::Error,
                    &format!("attach launch failed, retrying without --attach: {error}"),
                    None,
                    executor.max_output_lines_per_step,
                )?;
            }
            let mut fallback =
                opencode_run_command(executor, &persisted.steps[step_index], &prompt, false);
            match spawn_opencode(&mut fallback) {
                Ok(child) => (child, false),
                Err(error) => {
                    mark_spawn_failure(
                        conn,
                        &mut persisted.steps[step_index],
                        &error,
                        executor.max_output_lines_per_step,
                    )?;
                    return Err(error);
                }
            }
        }
        Err(error) => {
            mark_spawn_failure(
                conn,
                &mut persisted.steps[step_index],
                &error,
                executor.max_output_lines_per_step,
            )?;
            return Err(error);
        }
    };

    {
        let step = &mut persisted.steps[step_index];
        step.status = AutoStepStatus::Running;
        step.process_id = Some(child.id());
        save_step_with_conn(conn, step)?;
    }

    let exit_code = collect_child_output(
        conn,
        &mut persisted.steps[step_index],
        &mut child,
        executor.max_output_lines_per_step,
        output,
    )?;
    finish_step_after_exit(
        conn,
        &mut persisted.steps[step_index],
        exit_code,
        used_attach,
    )?;
    if exit_code == 0 {
        Ok(())
    } else {
        let step = &persisted.steps[step_index];
        Err(format!(
            "auto flow step {} attempt {} failed: {}",
            step.step_key.as_str(),
            step.attempt,
            step.error.as_deref().unwrap_or("opencode run failed")
        ))
    }
}

fn next_queued_agent_step(persisted: &PersistedAutoRun) -> Option<usize> {
    persisted.steps.iter().position(|step| {
        step.status == AutoStepStatus::Queued
            && matches!(
                step.step_key,
                AutoStepKey::CreatePlan
                    | AutoStepKey::ReviewPlan
                    | AutoStepKey::Implement
                    | AutoStepKey::FixLocalVerify
                    | AutoStepKey::FixReview
                    | AutoStepKey::FixCi
                    | AutoStepKey::Custom(_)
            )
    })
}

fn next_queued_non_agent_step(persisted: &PersistedAutoRun) -> Option<usize> {
    persisted.steps.iter().position(|step| {
        step.status == AutoStepStatus::Queued
            && matches!(
                step.step_key,
                AutoStepKey::ApprovePlan
                    | AutoStepKey::RunPlan
                    | AutoStepKey::LocalVerify
                    | AutoStepKey::CommitImpl
                    | AutoStepKey::PushPr
                    | AutoStepKey::WaitReview
                    | AutoStepKey::VerifyReviewFix
                    | AutoStepKey::CommitReviewFix
                    | AutoStepKey::WaitCi
                    | AutoStepKey::VerifyCiFix
                    | AutoStepKey::CommitCiFix
                    | AutoStepKey::Merge
                    | AutoStepKey::Cleanup
            )
    })
}

fn has_queued_non_agent_step(persisted: &PersistedAutoRun) -> bool {
    next_queued_non_agent_step(persisted).is_some()
}

fn next_state_machine_step_needed(persisted: &PersistedAutoRun) -> bool {
    if persisted.run.implementation_source == AutoImplementationSource::DraftPlan {
        if !has_step_key(persisted, &AutoStepKey::CreatePlan) {
            return true;
        }
        if latest_step_status(persisted, &AutoStepKey::CreatePlan) == Some(AutoStepStatus::Done)
            && !has_step_key(persisted, &AutoStepKey::ReviewPlan)
        {
            return true;
        }
        if latest_step_status(persisted, &AutoStepKey::ReviewPlan) == Some(AutoStepStatus::Done)
            && !has_step_key(persisted, &AutoStepKey::ApprovePlan)
        {
            return true;
        }
        if latest_step_status(persisted, &AutoStepKey::ApprovePlan) != Some(AutoStepStatus::Done) {
            return false;
        }
    }
    !has_step_key(persisted, &implementation_step_key(persisted))
}

fn implementation_follow_up_step_needed(persisted: &PersistedAutoRun) -> bool {
    latest_step_status(persisted, &implementation_step_key(persisted)) == Some(AutoStepStatus::Done)
        && !has_step_key(persisted, &AutoStepKey::LocalVerify)
}

fn ensure_next_auto_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<bool, String> {
    if merge_or_manual_merge_complete(persisted) {
        persisted.run.status = AutoRunStatus::Done;
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        return Ok(false);
    }
    if ci_loop_complete(persisted) && !has_step_key(persisted, &AutoStepKey::Merge) {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::Merge,
            Some("run final merge safety gate".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::Merge) == Some(AutoStepStatus::Done)
        && !has_step_key(persisted, &AutoStepKey::Cleanup)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::Cleanup,
            Some("clean up merged local worktree/session data".to_string()),
        )?;
        return Ok(true);
    }
    if persisted.run.implementation_source == AutoImplementationSource::DraftPlan
        && !has_step_key(persisted, &AutoStepKey::CreatePlan)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::CreatePlan,
            Some("create implementation plan.md".to_string()),
        )?;
        return Ok(true);
    }
    if persisted.run.implementation_source == AutoImplementationSource::DraftPlan
        && latest_step_status(persisted, &AutoStepKey::CreatePlan) == Some(AutoStepStatus::Done)
        && !has_step_key(persisted, &AutoStepKey::ReviewPlan)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::ReviewPlan,
            Some("review implementation plan.md before coding".to_string()),
        )?;
        return Ok(true);
    }
    if persisted.run.implementation_source == AutoImplementationSource::DraftPlan
        && latest_step_status(persisted, &AutoStepKey::ReviewPlan) == Some(AutoStepStatus::Done)
        && !has_step_key(persisted, &AutoStepKey::ApprovePlan)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::ApprovePlan,
            Some("pause for user approval of plan.md".to_string()),
        )?;
        return Ok(true);
    }
    if persisted.run.implementation_source == AutoImplementationSource::DraftPlan
        && latest_step_status(persisted, &AutoStepKey::ApprovePlan) != Some(AutoStepStatus::Done)
    {
        return Ok(false);
    }
    let implementation_step_key = implementation_step_key(persisted);
    if !has_step_key(persisted, &implementation_step_key) {
        append_step_run(
            conn,
            persisted,
            implementation_step_key,
            Some(implementation_step_reason(persisted).to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &implementation_step_key) == Some(AutoStepStatus::Done)
        && !has_step_key(persisted, &AutoStepKey::LocalVerify)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::LocalVerify,
            Some("run local verification before committing".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::FixLocalVerify) == Some(AutoStepStatus::Done)
        && latest_unfinished_verify_after_fix(persisted) == Some(AutoStepKey::LocalVerify)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::LocalVerify,
            Some("retry local verification after repair".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::FixLocalVerify) == Some(AutoStepStatus::Done)
        && latest_unfinished_verify_after_fix(persisted) == Some(AutoStepKey::VerifyReviewFix)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::VerifyReviewFix,
            Some("retry review-fix verification after repair".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::LocalVerify) == Some(AutoStepStatus::Done)
        && !has_step_key(persisted, &AutoStepKey::CommitImpl)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::CommitImpl,
            Some("commit verified implementation changes".to_string()),
        )?;
        return Ok(true);
    }
    if matches!(
        latest_step_status(persisted, &AutoStepKey::CommitImpl),
        Some(AutoStepStatus::Done | AutoStepStatus::Skipped)
    ) && !has_step_key(persisted, &AutoStepKey::PushPr)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::PushPr,
            Some("push branch and create or refresh pull request".to_string()),
        )?;
        return Ok(true);
    }
    if has_step_status(persisted, &AutoStepKey::PushPr, AutoStepStatus::Done)
        && !has_step_key(persisted, &AutoStepKey::WaitReview)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::WaitReview,
            Some("wait for automated review feedback".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::FixReview) == Some(AutoStepStatus::Done)
        && latest_step_status(persisted, &AutoStepKey::VerifyReviewFix)
            != Some(AutoStepStatus::Queued)
        && latest_step_status(persisted, &AutoStepKey::VerifyReviewFix)
            != Some(AutoStepStatus::Done)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::VerifyReviewFix,
            Some("run review-fix verification before committing".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::VerifyReviewFix) == Some(AutoStepStatus::Done)
        && latest_step_status(persisted, &AutoStepKey::CommitReviewFix)
            != Some(AutoStepStatus::Queued)
        && latest_step_status(persisted, &AutoStepKey::CommitReviewFix)
            != Some(AutoStepStatus::Done)
        && latest_step_status(persisted, &AutoStepKey::CommitReviewFix)
            != Some(AutoStepStatus::Skipped)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::CommitReviewFix,
            Some("commit and push verified review fixes".to_string()),
        )?;
        return Ok(true);
    }
    if matches!(
        latest_step_status(persisted, &AutoStepKey::CommitReviewFix),
        Some(AutoStepStatus::Done | AutoStepStatus::Skipped)
    ) {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::WaitReview,
            Some("wait for fresh automated review feedback after review-fix push".to_string()),
        )?;
        return Ok(true);
    }
    if review_loop_complete(persisted) && !has_step_key(persisted, &AutoStepKey::WaitCi) {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::WaitCi,
            Some("wait for pull request checks".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::FixCi) == Some(AutoStepStatus::Done)
        && latest_step_status(persisted, &AutoStepKey::VerifyCiFix) != Some(AutoStepStatus::Queued)
        && latest_step_status(persisted, &AutoStepKey::VerifyCiFix) != Some(AutoStepStatus::Done)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::VerifyCiFix,
            Some("run CI-fix verification before committing".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::FixLocalVerify) == Some(AutoStepStatus::Done)
        && latest_unfinished_verify_after_fix(persisted) == Some(AutoStepKey::VerifyCiFix)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::VerifyCiFix,
            Some("retry CI-fix verification after repair".to_string()),
        )?;
        return Ok(true);
    }
    if latest_step_status(persisted, &AutoStepKey::VerifyCiFix) == Some(AutoStepStatus::Done)
        && latest_step_status(persisted, &AutoStepKey::CommitCiFix) != Some(AutoStepStatus::Queued)
        && latest_step_status(persisted, &AutoStepKey::CommitCiFix) != Some(AutoStepStatus::Done)
        && latest_step_status(persisted, &AutoStepKey::CommitCiFix) != Some(AutoStepStatus::Skipped)
    {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::CommitCiFix,
            Some("commit and push verified CI fixes".to_string()),
        )?;
        return Ok(true);
    }
    if matches!(
        latest_step_status(persisted, &AutoStepKey::CommitCiFix),
        Some(AutoStepStatus::Done)
    ) {
        append_step_run(
            conn,
            persisted,
            AutoStepKey::WaitCi,
            Some("wait for pull request checks after CI-fix push".to_string()),
        )?;
        return Ok(true);
    }
    Ok(false)
}

fn initial_agent_step(persisted: &PersistedAutoRun) -> (AutoStepKey, &'static str) {
    match persisted.run.implementation_source {
        AutoImplementationSource::Prompt => {
            (AutoStepKey::Implement, "run initial implementation prompt")
        }
        AutoImplementationSource::ExistingPlan => (AutoStepKey::RunPlan, "run plan phases"),
        AutoImplementationSource::DraftPlan => {
            (AutoStepKey::CreatePlan, "create implementation plan.md")
        }
    }
}

fn implementation_step_key(persisted: &PersistedAutoRun) -> AutoStepKey {
    match persisted.run.implementation_source {
        AutoImplementationSource::Prompt => AutoStepKey::Implement,
        AutoImplementationSource::ExistingPlan | AutoImplementationSource::DraftPlan => {
            AutoStepKey::RunPlan
        }
    }
}

fn implementation_step_reason(persisted: &PersistedAutoRun) -> &'static str {
    match persisted.run.implementation_source {
        AutoImplementationSource::Prompt => "run initial implementation prompt",
        AutoImplementationSource::ExistingPlan => "run plan phases from selected plan",
        AutoImplementationSource::DraftPlan => "run plan phases from approved plan.md",
    }
}

fn execute_one_non_agent_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    start_non_agent_step(conn, persisted, step_index)?;
    let result = match persisted.steps[step_index].step_key {
        AutoStepKey::ApprovePlan => {
            execute_approve_plan_step(conn, persisted, step_index, max_output_lines_per_step)
        }
        AutoStepKey::RunPlan => execute_run_plan_step(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::LocalVerify => execute_local_verify_step(
            conn,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::CommitImpl => execute_commit_impl_step(
            conn,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::PushPr => execute_push_pr_step(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::WaitReview => execute_wait_review_step(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::VerifyReviewFix => execute_verify_review_fix_step(
            conn,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::CommitReviewFix => execute_commit_review_fix_step(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::WaitCi => execute_wait_ci_step(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::VerifyCiFix => execute_verify_ci_fix_step(
            conn,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::CommitCiFix => execute_commit_ci_fix_step(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::Merge => execute_merge_step(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::Cleanup => execute_cleanup_step(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        _ => Ok(()),
    };
    if let Err(error) = result {
        fail_step(
            conn,
            &mut persisted.steps[step_index],
            &error,
            max_output_lines_per_step,
        )?;
        return Err(error);
    }
    Ok(())
}

fn execute_approve_plan_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto plan approval step must be saved before output".to_string())?;
    let plan_path = plan_first_plan_path(&persisted.run);
    let summary = format!(
        "plan review complete; approve by resuming this Auto Flow after reviewing {}",
        plan_path.display()
    );
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &summary,
        None,
        max_output_lines_per_step,
    )?;
    finish_non_agent_step(
        conn,
        &mut persisted.steps[step_index],
        AutoStepStatus::Done,
        Some(summary),
        None,
    )?;
    persisted.run.pause_requested = true;
    persisted.run.status = AutoRunStatus::Paused;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

fn execute_run_plan_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    crate::plan_run::migrate_schema(conn)?;
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto run-plan step must be saved before output".to_string())?;
    let plan_path = auto_plan_path(&persisted.run)?;
    let execution = PlanExecution::prepare(
        &persisted.run.worktree_path,
        config,
        Some(plan_path.as_path()),
    )?;
    let mode = persisted.run.plan_run_mode;
    let launch = execution.launch(Path::new(&persisted.run.repo_root), mode)?;
    let mut plan_run = if let Some(plan_run_id) = persisted.steps[step_index].plan_run_id.as_deref()
    {
        load_plan_run(conn, plan_run_id)?.ok_or_else(|| {
            format!("linked plan run {plan_run_id} was not found for auto run-plan step")
        })?
    } else {
        let plan_run = launch.create_run();
        save_plan_run(conn, &plan_run)?;
        persisted.steps[step_index].plan_run_id = Some(plan_run.run.id.clone());
        save_step_with_conn(conn, &mut persisted.steps[step_index])?;
        plan_run
    };

    let summary = format!("running plan phases from {}", plan_run.run.plan_display);
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &summary,
        None,
        max_output_lines_per_step,
    )?;

    let mut plan_executor = PlanExecutorConfig::new(
        config.tool("opencode"),
        None,
        persisted.run.worktree_path.clone(),
        plan_run.run.plan_display.clone(),
    );
    plan_executor.max_output_lines_per_step = max_output_lines_per_step;
    if config.opencode_plan_plugin
        && let Ok(plugin) = prepare_plan_plugin_config(&repo.prism_dir())
    {
        plan_executor = plan_executor.with_plugin_config(plugin);
    }

    let mut output = Vec::new();
    let result = match mode {
        PlanRunMode::Sequential => {
            execute_plan_sequential(conn, &mut plan_run, &plan_executor, &mut output)
        }
        PlanRunMode::Parallel => {
            execute_plan_parallel(conn, &mut plan_run, &plan_executor, &mut output)
        }
    };
    if let Err(error) = result
        && !matches!(
            plan_run.run.status,
            PlanRunStatus::Failed | PlanRunStatus::Aborted
        )
    {
        return Err(error);
    }

    match plan_run.run.status {
        PlanRunStatus::Done => {
            let summary = format!("plan run {} completed", plan_run.run.id);
            append_system_output(
                conn,
                step_id,
                AutoOutputKind::Status,
                &summary,
                None,
                max_output_lines_per_step,
            )?;
            finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                AutoStepStatus::Done,
                Some(summary),
                None,
            )
        }
        PlanRunStatus::Paused => {
            let summary = format!(
                "plan run {} paused; resume linked plan run",
                plan_run.run.id
            );
            append_system_output(
                conn,
                step_id,
                AutoOutputKind::Status,
                &summary,
                None,
                max_output_lines_per_step,
            )?;
            finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                AutoStepStatus::Waiting,
                Some(summary),
                None,
            )
        }
        PlanRunStatus::Failed | PlanRunStatus::Aborted => {
            let error = format!(
                "plan run {} ended with status {}; inspect linked plan dashboard",
                plan_run.run.id,
                plan_run_status_label(plan_run.run.status)
            );
            finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                AutoStepStatus::Failed,
                Some("plan run failed".to_string()),
                Some(error.clone()),
            )?;
            Err(error)
        }
        PlanRunStatus::Draft | PlanRunStatus::Queued | PlanRunStatus::Running => {
            let summary = format!(
                "plan run {} is {}; Auto Flow is waiting",
                plan_run.run.id,
                plan_run_status_label(plan_run.run.status)
            );
            finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                AutoStepStatus::Waiting,
                Some(summary),
                None,
            )
        }
    }
}

fn start_non_agent_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
) -> Result<(), String> {
    let step = &mut persisted.steps[step_index];
    step.status = AutoStepStatus::Running;
    step.started_unix_ms = Some(unix_ms());
    step.finished_unix_ms = None;
    step.error = None;
    persisted.run.selected_step_run_id = step.id;
    persisted.run.status = AutoRunStatus::Running;
    persisted.run.updated_unix_ms = unix_ms();
    save_step_with_conn(conn, step)?;
    save_run_with_conn(conn, &persisted.run)
}

fn execute_local_verify_step(
    conn: &rusqlite::Connection,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let result =
        crate::verify::run_auto_verify(config, &persisted.run.worktree_path, VerifyMode::Normal);
    let summary = format_verify_result(&result);
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto verify step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        if result.passed {
            AutoOutputKind::Status
        } else {
            AutoOutputKind::Error
        },
        &summary,
        None,
        max_output_lines_per_step,
    )?;
    if result.passed {
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Done,
            Some("local verification passed".to_string()),
            None,
        )?;
        return Ok(());
    }

    finish_non_agent_step(
        conn,
        &mut persisted.steps[step_index],
        AutoStepStatus::Failed,
        Some("local verification failed".to_string()),
        Some(summary.clone()),
    )?;
    if persisted.next_attempt_for(&AutoStepKey::FixLocalVerify) <= MAX_LOCAL_VERIFY_ATTEMPTS {
        append_step_run(conn, persisted, AutoStepKey::FixLocalVerify, Some(summary))?;
        Ok(())
    } else {
        Err(format!(
            "local verification failed after {MAX_LOCAL_VERIFY_ATTEMPTS} repair attempts"
        ))
    }
}

fn execute_commit_impl_step(
    conn: &rusqlite::Connection,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let message = implementation_commit_message(&persisted.run);
    let result = crate::git::commit_if_dirty(&persisted.run.worktree_path, config, &message)?;
    let step = &mut persisted.steps[step_index];
    step.commit_sha = result.commit_sha.clone();
    step.head_sha = result
        .commit_sha
        .clone()
        .or_else(|| crate::git::current_head_sha(&persisted.run.worktree_path, config).ok());
    persisted.run.current_head_sha = step.head_sha.clone();
    let status = if result.committed {
        AutoStepStatus::Done
    } else {
        AutoStepStatus::Skipped
    };
    let summary = if result.committed {
        format!(
            "committed implementation as {}",
            result.commit_sha.as_deref().unwrap_or("unknown")
        )
    } else {
        result.message
    };
    let step_id = step
        .id
        .ok_or_else(|| "auto commit step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &summary,
        None,
        max_output_lines_per_step,
    )?;
    finish_non_agent_step(conn, step, status, Some(summary), None)?;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

fn execute_push_pr_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let head_sha = crate::git::current_head_sha(&persisted.run.worktree_path, config)?;
    crate::git::push_current_branch(&persisted.run.worktree_path, config)?;

    let mut cache = crate::github::load_pr_cache(repo, &persisted.run.branch);
    crate::lifecycle::refresh_branch_pr_cache(
        repo,
        config,
        &persisted.run.branch,
        &persisted.run.worktree_path,
        &mut cache,
        true,
    );
    if cache.summary.is_none() {
        let body = auto_pr_body(config, &persisted.run);
        crate::lifecycle::create_pull_request(
            repo,
            config,
            &persisted.run.branch,
            &persisted.run.worktree_path,
            &body,
            &mut cache,
        )?;
    }
    if cache.summary.is_none() {
        crate::lifecycle::refresh_branch_pr_cache(
            repo,
            config,
            &persisted.run.branch,
            &persisted.run.worktree_path,
            &mut cache,
            true,
        );
    }
    let summary = cache
        .summary
        .as_ref()
        .ok_or_else(|| "push/create PR completed but no PR summary was found".to_string())?;
    persisted.run.pr_number = Some(summary.number);
    persisted.run.pr_url = Some(summary.url.clone());
    persisted.run.current_head_sha = Some(if summary.head_sha.trim().is_empty() {
        head_sha.clone()
    } else {
        summary.head_sha.clone()
    });
    let step = &mut persisted.steps[step_index];
    step.head_sha = persisted.run.current_head_sha.clone();
    persisted.run.review_baseline_json = Some(review_baseline_json(summary));
    let message = format!("PR #{} {}", summary.number, summary.url);
    let step_id = step
        .id
        .ok_or_else(|| "auto push PR step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &message,
        None,
        max_output_lines_per_step,
    )?;
    finish_non_agent_step(conn, step, AutoStepStatus::Done, Some(message), None)?;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

fn execute_wait_review_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto review wait step must be saved before output".to_string())?;
    if !config.auto.review_wait_enabled {
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            "review wait disabled; continuing",
            None,
            max_output_lines_per_step,
        )?;
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Skipped,
            Some("review wait disabled".to_string()),
            None,
        )?;
        return Ok(());
    }

    let deadline = unix_ms().saturating_add(config.auto.review_max_wait_seconds * 1000);
    loop {
        let outcome = poll_review_feedback(repo, config, persisted)?;
        append_auto_event(
            conn,
            &AutoEvent {
                id: None,
                run_id: persisted.run.id.clone(),
                step_run_id: Some(step_id),
                time_unix_ms: unix_ms(),
                kind: "review_wait_poll".to_string(),
                data_json: format!("{{\"summary\":{}}}", json_string(&outcome.summary)),
            },
        )?;
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            &outcome.summary,
            None,
            max_output_lines_per_step,
        )?;

        if let Some(prompt) = outcome.fix_prompt {
            finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                AutoStepStatus::Done,
                Some(outcome.summary),
                None,
            )?;
            if persisted.next_attempt_for(&AutoStepKey::FixReview) <= MAX_REVIEW_FIX_ATTEMPTS {
                append_step_run(conn, persisted, AutoStepKey::FixReview, Some(prompt))?;
                return Ok(());
            }
            return Err(format!(
                "review feedback remained after {MAX_REVIEW_FIX_ATTEMPTS} repair attempts"
            ));
        }

        if outcome.complete {
            finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                AutoStepStatus::Skipped,
                Some(outcome.summary),
                None,
            )?;
            return Ok(());
        }

        if unix_ms() >= deadline {
            let summary = format!(
                "review wait timed out after {} second(s)",
                config.auto.review_max_wait_seconds
            );
            let status = if config.auto.review_continue_on_timeout {
                AutoStepStatus::Skipped
            } else {
                AutoStepStatus::Failed
            };
            finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                status,
                Some(summary.clone()),
                if status == AutoStepStatus::Failed {
                    Some(summary.clone())
                } else {
                    None
                },
            )?;
            if status == AutoStepStatus::Failed {
                return Err(summary);
            }
            return Ok(());
        }

        persisted.steps[step_index].status = AutoStepStatus::Waiting;
        save_step_with_conn(conn, &mut persisted.steps[step_index])?;
        std::thread::sleep(std::time::Duration::from_secs(
            config.auto.review_poll_interval_seconds,
        ));
        if reload_pause_request(conn, persisted)? {
            return Ok(());
        }
        persisted.steps[step_index].status = AutoStepStatus::Running;
        save_step_with_conn(conn, &mut persisted.steps[step_index])?;
    }
}

fn execute_verify_review_fix_step(
    conn: &rusqlite::Connection,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let result =
        crate::verify::run_auto_verify(config, &persisted.run.worktree_path, VerifyMode::ReviewFix);
    let summary = format_verify_result(&result);
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto review verify step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        if result.passed {
            AutoOutputKind::Status
        } else {
            AutoOutputKind::Error
        },
        &summary,
        None,
        max_output_lines_per_step,
    )?;
    if result.passed {
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Done,
            Some("review-fix verification passed".to_string()),
            None,
        )?;
        return Ok(());
    }
    finish_non_agent_step(
        conn,
        &mut persisted.steps[step_index],
        AutoStepStatus::Failed,
        Some("review-fix verification failed".to_string()),
        Some(summary.clone()),
    )?;
    if persisted.next_attempt_for(&AutoStepKey::FixLocalVerify) <= MAX_LOCAL_VERIFY_ATTEMPTS {
        append_step_run(conn, persisted, AutoStepKey::FixLocalVerify, Some(summary))?;
        Ok(())
    } else {
        Err(format!(
            "review-fix verification failed after {MAX_LOCAL_VERIFY_ATTEMPTS} repair attempts"
        ))
    }
}

fn execute_commit_review_fix_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let result = crate::git::commit_if_dirty(&persisted.run.worktree_path, config, "fix: cr")?;
    if result.committed {
        crate::git::push_current_branch(&persisted.run.worktree_path, config)?;
    }
    let head_sha = crate::git::current_head_sha(&persisted.run.worktree_path, config).ok();
    persisted.run.current_head_sha = head_sha.clone();
    let mut cache = crate::github::load_pr_cache(repo, &persisted.run.branch);
    crate::lifecycle::refresh_branch_pr_cache(
        repo,
        config,
        &persisted.run.branch,
        &persisted.run.worktree_path,
        &mut cache,
        true,
    );
    if let Some(summary) = cache.summary.as_ref() {
        persisted.run.pr_number = Some(summary.number);
        persisted.run.pr_url = Some(summary.url.clone());
        persisted.run.current_head_sha = Some(if summary.head_sha.trim().is_empty() {
            head_sha.unwrap_or_default()
        } else {
            summary.head_sha.clone()
        });
        persisted.run.review_baseline_json = Some(review_baseline_json(summary));
    }

    let step = &mut persisted.steps[step_index];
    step.commit_sha = result.commit_sha.clone();
    step.head_sha = persisted.run.current_head_sha.clone();
    let status = if result.committed {
        AutoStepStatus::Done
    } else {
        AutoStepStatus::Skipped
    };
    let summary = if result.committed {
        format!(
            "committed review fixes as {} and pushed",
            result.commit_sha.as_deref().unwrap_or("unknown")
        )
    } else {
        result.message
    };
    let step_id = step
        .id
        .ok_or_else(|| "auto review commit step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &summary,
        None,
        max_output_lines_per_step,
    )?;
    finish_non_agent_step(conn, step, status, Some(summary), None)?;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

fn execute_wait_ci_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto CI wait step must be saved before output".to_string())?;
    if !config.auto.ci_wait_enabled {
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            "CI wait disabled; continuing",
            None,
            max_output_lines_per_step,
        )?;
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Skipped,
            Some("CI wait disabled".to_string()),
            None,
        )?;
        return Ok(());
    }

    let deadline = unix_ms().saturating_add(config.auto.ci_max_wait_seconds * 1000);
    loop {
        let outcome = poll_ci_status(repo, config, persisted)?;
        append_auto_event(
            conn,
            &AutoEvent {
                id: None,
                run_id: persisted.run.id.clone(),
                step_run_id: Some(step_id),
                time_unix_ms: unix_ms(),
                kind: "ci_wait_poll".to_string(),
                data_json: format!(
                    "{{\"state\":{},\"summary\":{}}}",
                    json_string(outcome.state.label()),
                    json_string(&outcome.summary)
                ),
            },
        )?;
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            &outcome.summary,
            None,
            max_output_lines_per_step,
        )?;

        match outcome.state {
            PrCheckState::Success => {
                finish_non_agent_step(
                    conn,
                    &mut persisted.steps[step_index],
                    AutoStepStatus::Done,
                    Some(outcome.summary),
                    None,
                )?;
                return Ok(());
            }
            PrCheckState::Failed | PrCheckState::Mixed => {
                finish_non_agent_step(
                    conn,
                    &mut persisted.steps[step_index],
                    AutoStepStatus::Done,
                    Some(outcome.summary.clone()),
                    None,
                )?;
                if persisted.next_attempt_for(&AutoStepKey::FixCi) <= MAX_CI_FIX_ATTEMPTS {
                    append_step_run(conn, persisted, AutoStepKey::FixCi, Some(outcome.prompt))?;
                    return Ok(());
                }
                let error =
                    format!("CI remained failing after {MAX_CI_FIX_ATTEMPTS} repair attempts");
                return Err(error);
            }
            PrCheckState::Pending | PrCheckState::Unknown => {}
        }

        if unix_ms() >= deadline {
            let summary = format!(
                "CI wait timed out after {} second(s)",
                config.auto.ci_max_wait_seconds
            );
            finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                AutoStepStatus::Failed,
                Some(summary.clone()),
                Some(summary.clone()),
            )?;
            return Err(summary);
        }

        persisted.steps[step_index].status = AutoStepStatus::Waiting;
        save_step_with_conn(conn, &mut persisted.steps[step_index])?;
        std::thread::sleep(std::time::Duration::from_secs(
            config.auto.ci_poll_interval_seconds,
        ));
        if reload_pause_request(conn, persisted)? {
            return Ok(());
        }
        persisted.steps[step_index].status = AutoStepStatus::Running;
        save_step_with_conn(conn, &mut persisted.steps[step_index])?;
    }
}

fn execute_verify_ci_fix_step(
    conn: &rusqlite::Connection,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let result =
        crate::verify::run_auto_verify(config, &persisted.run.worktree_path, VerifyMode::Normal);
    let summary = format_verify_result(&result);
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto CI verify step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        if result.passed {
            AutoOutputKind::Status
        } else {
            AutoOutputKind::Error
        },
        &summary,
        None,
        max_output_lines_per_step,
    )?;
    if result.passed {
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Done,
            Some("CI-fix verification passed".to_string()),
            None,
        )?;
        return Ok(());
    }
    finish_non_agent_step(
        conn,
        &mut persisted.steps[step_index],
        AutoStepStatus::Failed,
        Some("CI-fix verification failed".to_string()),
        Some(summary.clone()),
    )?;
    if persisted.next_attempt_for(&AutoStepKey::FixLocalVerify) <= MAX_LOCAL_VERIFY_ATTEMPTS {
        append_step_run(conn, persisted, AutoStepKey::FixLocalVerify, Some(summary))?;
        Ok(())
    } else {
        Err(format!(
            "CI-fix verification failed after {MAX_LOCAL_VERIFY_ATTEMPTS} repair attempts"
        ))
    }
}

fn execute_commit_ci_fix_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let result = crate::git::commit_if_dirty(&persisted.run.worktree_path, config, "fix: ci")?;
    if !result.committed {
        let summary = "CI fix produced no commitable changes".to_string();
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Failed,
            Some(summary.clone()),
            Some(summary.clone()),
        )?;
        return Err(summary);
    }
    crate::git::push_current_branch(&persisted.run.worktree_path, config)?;
    let local_head = crate::git::current_head_sha(&persisted.run.worktree_path, config).ok();
    persisted.run.current_head_sha = local_head.clone();
    let mut cache = crate::github::load_pr_cache(repo, &persisted.run.branch);
    crate::lifecycle::refresh_branch_pr_cache(
        repo,
        config,
        &persisted.run.branch,
        &persisted.run.worktree_path,
        &mut cache,
        true,
    );
    if let Some(summary) = cache.summary.as_ref() {
        persisted.run.pr_number = Some(summary.number);
        persisted.run.pr_url = Some(summary.url.clone());
        persisted.run.current_head_sha = Some(if summary.head_sha.trim().is_empty() {
            local_head.unwrap_or_default()
        } else {
            summary.head_sha.clone()
        });
        persisted.run.review_baseline_json = Some(review_baseline_json(summary));
    }

    let step = &mut persisted.steps[step_index];
    step.commit_sha = result.commit_sha.clone();
    step.head_sha = persisted.run.current_head_sha.clone();
    let summary = format!(
        "committed CI fixes as {} and pushed",
        result.commit_sha.as_deref().unwrap_or("unknown")
    );
    let step_id = step
        .id
        .ok_or_else(|| "auto CI commit step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &summary,
        None,
        max_output_lines_per_step,
    )?;
    finish_non_agent_step(conn, step, AutoStepStatus::Done, Some(summary), None)?;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

fn execute_merge_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto merge step must be saved before output".to_string())?;
    if !config.auto.merge {
        let summary = "auto.merge is false; PR is ready for manual merge".to_string();
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            &summary,
            None,
            max_output_lines_per_step,
        )?;
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Skipped,
            Some(summary),
            None,
        )?;
        return Ok(());
    }

    let verify =
        crate::verify::run_auto_verify(config, &persisted.run.worktree_path, VerifyMode::Normal);
    let local_head = crate::git::current_head_sha(&persisted.run.worktree_path, config)?;
    let dirty = crate::git::selected_dirty(&persisted.run.worktree_path, config)?;
    let mut cache = crate::github::load_pr_cache(repo, &persisted.run.branch);
    crate::lifecycle::refresh_branch_pr_cache(
        repo,
        config,
        &persisted.run.branch,
        &persisted.run.worktree_path,
        &mut cache,
        true,
    );
    let summary = cache
        .summary
        .as_ref()
        .ok_or_else(|| "merge gate could not find pull request summary".to_string())?;
    persisted.run.pr_number = Some(summary.number);
    persisted.run.pr_url = Some(summary.url.clone());
    persisted.run.current_head_sha = Some(summary.head_sha.clone());

    let gate = evaluate_merge_gate(
        config,
        persisted,
        summary,
        cache.details.as_ref(),
        &local_head,
        dirty,
        &verify,
    );
    append_system_output(
        conn,
        step_id,
        if gate.allowed {
            AutoOutputKind::Status
        } else {
            AutoOutputKind::Error
        },
        &gate.summary,
        None,
        max_output_lines_per_step,
    )?;
    if !gate.allowed {
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Failed,
            Some("merge blocked by final gate".to_string()),
            Some(gate.summary.clone()),
        )?;
        return Err(gate.summary);
    }

    if !summary.merged {
        crate::lifecycle::merge_pull_request(config, &persisted.run.worktree_path, summary.number)?;
    }
    let merged =
        crate::github::wait_for_pr_merged(&persisted.run.worktree_path, summary.number, config)?;
    if !merged {
        let error = format!(
            "PR #{} merge command completed, but GitHub has not marked it merged yet",
            summary.number
        );
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Failed,
            Some("merge verification incomplete".to_string()),
            Some(error.clone()),
        )?;
        return Err(error);
    }

    let done = format!("merged PR #{}", summary.number);
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &done,
        None,
        max_output_lines_per_step,
    )?;
    finish_non_agent_step(
        conn,
        &mut persisted.steps[step_index],
        AutoStepStatus::Done,
        Some(done),
        None,
    )?;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

fn execute_cleanup_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto cleanup step must be saved before output".to_string())?;
    if !config.auto.cleanup_after_merge {
        let summary =
            "auto.cleanup_after_merge is false; leaving local worktree/session data".to_string();
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            &summary,
            None,
            max_output_lines_per_step,
        )?;
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Skipped,
            Some(summary),
            None,
        )?;
        return Ok(());
    }

    let warnings = cleanup_warnings(repo, config, &persisted.run.worktree_path);
    if !warnings.is_empty() {
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            &format!("cleanup warnings:\n- {}", warnings.join("\n- ")),
            None,
            max_output_lines_per_step,
        )?;
    }

    crate::lifecycle::delete_worktree_session(
        repo,
        config,
        &persisted.run.worktree_path,
        &persisted.run.branch,
    )?;
    let summary = "deleted local session data, worktree, and branch".to_string();
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &summary,
        None,
        max_output_lines_per_step,
    )?;
    finish_non_agent_step(
        conn,
        &mut persisted.steps[step_index],
        AutoStepStatus::Done,
        Some(summary),
        None,
    )?;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

fn finish_non_agent_step(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    status: AutoStepStatus,
    summary: Option<String>,
    error: Option<String>,
) -> Result<(), String> {
    step.status = status;
    step.finished_unix_ms = Some(unix_ms());
    step.summary = summary;
    step.error = error;
    save_step_with_conn(conn, step)?;
    Ok(())
}

fn set_auto_step_waiting(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    summary: String,
) -> Result<(), String> {
    step.status = AutoStepStatus::Waiting;
    step.finished_unix_ms = None;
    step.process_id = None;
    step.summary = Some(summary);
    step.error = None;
    save_step_with_conn(conn, step).map(|_| ())
}

fn reset_auto_step_for_retry(step: &mut AutoStepRun) {
    step.status = AutoStepStatus::Queued;
    step.started_unix_ms = None;
    step.finished_unix_ms = None;
    step.opencode_session_id = None;
    step.process_id = None;
    step.commit_sha = None;
    step.head_sha = None;
    step.summary = None;
    step.error = None;
}

fn append_step_status_output(
    conn: &rusqlite::Connection,
    step: &AutoStepRun,
    text: &str,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let Some(step_id) = step.id else {
        return Ok(());
    };
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        text,
        None,
        max_output_lines_per_step,
    )
}

fn request_active_linked_plan_pause(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<(), String> {
    for step in &persisted.steps {
        if step.step_key != AutoStepKey::RunPlan
            || !matches!(
                step.status,
                AutoStepStatus::Queued
                    | AutoStepStatus::Starting
                    | AutoStepStatus::Running
                    | AutoStepStatus::Waiting
            )
        {
            continue;
        }
        let Some(plan_run_id) = step.plan_run_id.as_deref() else {
            continue;
        };
        let Some(mut plan_run) = load_plan_run(conn, plan_run_id)? else {
            continue;
        };
        if !matches!(
            plan_run.run.status,
            PlanRunStatus::Done | PlanRunStatus::Failed | PlanRunStatus::Aborted
        ) {
            request_plan_run_pause(conn, &mut plan_run)?;
        }
    }
    Ok(())
}

fn fail_step(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    error: &str,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    step.status = AutoStepStatus::Failed;
    step.finished_unix_ms = Some(unix_ms());
    step.error = Some(error.to_string());
    let step_id = save_step_with_conn(conn, step)?;
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Error,
        error,
        None,
        max_output_lines_per_step,
    )
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct ReviewBaseline {
    head_sha: String,
    updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReviewPollOutcome {
    summary: String,
    fix_prompt: Option<String>,
    complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CiPollOutcome {
    state: PrCheckState,
    summary: String,
    prompt: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MergeGateOutcome {
    allowed: bool,
    summary: String,
}

fn evaluate_merge_gate(
    config: &Config,
    persisted: &PersistedAutoRun,
    summary: &PrSummary,
    details: Option<&PrDetails>,
    local_head: &str,
    dirty: bool,
    verify: &VerifyResult,
) -> MergeGateOutcome {
    let mut blockers = Vec::new();
    if dirty {
        blockers.push("worktree is dirty".to_string());
    }
    if !verify.passed {
        blockers.push("final local verification failed".to_string());
    }
    if summary.draft {
        blockers.push("PR is draft".to_string());
    }
    if summary.check_state() != PrCheckState::Success {
        blockers.push(format!("CI state is {}", summary.check_state().label()));
    }
    if summary.head_sha.trim().is_empty() {
        blockers.push("PR head SHA is unknown".to_string());
    } else if summary.head_sha.trim() != local_head.trim() {
        blockers.push(format!(
            "PR head {} does not match local head {}",
            empty_or_unknown(&summary.head_sha),
            empty_or_unknown(local_head)
        ));
    }

    let review_blocker = merge_gate_review_blocker(config, persisted, summary, details);
    if let Some(blocker) = review_blocker {
        blockers.push(blocker);
    }

    if blockers.is_empty() {
        MergeGateOutcome {
            allowed: true,
            summary: format!(
                "merge gate passed for PR #{} at head {}",
                summary.number,
                empty_or_unknown(local_head)
            ),
        }
    } else {
        MergeGateOutcome {
            allowed: false,
            summary: format!("merge blocked:\n- {}", blockers.join("\n- ")),
        }
    }
}

fn merge_gate_review_blocker(
    config: &Config,
    persisted: &PersistedAutoRun,
    summary: &PrSummary,
    details: Option<&PrDetails>,
) -> Option<String> {
    if !config.auto.review_wait_enabled {
        return None;
    }
    let details = details?;
    let baseline = parse_review_baseline(persisted.run.review_baseline_json.as_deref());
    let after = baseline
        .as_ref()
        .filter(|baseline| baseline.head_sha == summary.head_sha)
        .map(|baseline| baseline.updated_at.as_str());
    let authors = config
        .auto
        .review_reviewer_identities
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let feedback = actionable_review_feedback(
        details,
        ReviewFeedbackFilter {
            after,
            authors: &authors,
        },
    );
    feedback.is_actionable().then(|| {
        format!(
            "actionable review feedback remains: {} inline, {} review body, {} PR comment(s)",
            feedback.inline_comments.len(),
            feedback.review_bodies.len(),
            feedback.pr_comments.len()
        )
    })
}

fn poll_ci_status(
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
) -> Result<CiPollOutcome, String> {
    let mut cache = crate::github::load_pr_cache(repo, &persisted.run.branch);
    crate::lifecycle::refresh_branch_pr_cache(
        repo,
        config,
        &persisted.run.branch,
        &persisted.run.worktree_path,
        &mut cache,
        true,
    );
    let summary = cache
        .summary
        .as_ref()
        .ok_or_else(|| "CI wait could not find pull request summary".to_string())?;
    persisted.run.pr_number = Some(summary.number);
    persisted.run.pr_url = Some(summary.url.clone());
    persisted.run.current_head_sha = Some(summary.head_sha.clone());
    evaluate_ci_status(
        config,
        &persisted.run.branch,
        summary,
        cache.details.as_ref(),
    )
}

fn evaluate_ci_status(
    config: &Config,
    branch: &str,
    summary: &PrSummary,
    details: Option<&PrDetails>,
) -> Result<CiPollOutcome, String> {
    let state = summary.check_state();
    let details = details.cloned().unwrap_or_default();
    let failures = details.failing_checks.len().max(details.ci_failures.len());
    let prompt = crate::ci::build_ci_failure_prompt_from_input(
        crate::ci::CiFailurePromptInput {
            branch,
            summary,
            details: &details,
        },
        config,
    );
    let summary_text = match state {
        PrCheckState::Success => {
            format!("CI passed for head {}", empty_or_unknown(&summary.head_sha))
        }
        PrCheckState::Failed => {
            format!(
                "CI failed for head {} with {} failing check detail(s)",
                empty_or_unknown(&summary.head_sha),
                failures
            )
        }
        PrCheckState::Mixed => {
            format!(
                "CI is mixed for head {} with {} failing check detail(s)",
                empty_or_unknown(&summary.head_sha),
                failures
            )
        }
        PrCheckState::Pending => {
            format!(
                "CI is still running for head {}",
                empty_or_unknown(&summary.head_sha)
            )
        }
        PrCheckState::Unknown => {
            format!(
                "CI status is unknown for head {}; waiting for checks",
                empty_or_unknown(&summary.head_sha)
            )
        }
    };
    Ok(CiPollOutcome {
        state,
        summary: summary_text,
        prompt,
    })
}

fn poll_review_feedback(
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
) -> Result<ReviewPollOutcome, String> {
    let mut cache = crate::github::load_pr_cache(repo, &persisted.run.branch);
    crate::lifecycle::refresh_branch_pr_cache(
        repo,
        config,
        &persisted.run.branch,
        &persisted.run.worktree_path,
        &mut cache,
        true,
    );
    let summary = cache
        .summary
        .as_ref()
        .ok_or_else(|| "review wait could not find pull request summary".to_string())?;
    persisted.run.pr_number = Some(summary.number);
    persisted.run.pr_url = Some(summary.url.clone());
    persisted.run.current_head_sha = Some(summary.head_sha.clone());
    if persisted.run.review_baseline_json.is_none() {
        persisted.run.review_baseline_json = Some(review_baseline_json(summary));
    }
    evaluate_review_feedback(config, persisted, summary, cache.details.as_ref())
}

fn evaluate_review_feedback(
    config: &Config,
    persisted: &mut PersistedAutoRun,
    summary: &crate::github::PrSummary,
    details: Option<&crate::github::PrDetails>,
) -> Result<ReviewPollOutcome, String> {
    let baseline = parse_review_baseline(persisted.run.review_baseline_json.as_deref());
    let after = baseline
        .as_ref()
        .filter(|baseline| baseline.head_sha == summary.head_sha)
        .map(|baseline| baseline.updated_at.as_str());
    if !has_configured_reviewer_requested(summary, config) {
        return Ok(ReviewPollOutcome {
            summary:
                "no automated reviewer feedback or pending configured reviewer found; continuing"
                    .to_string(),
            fix_prompt: None,
            complete: true,
        });
    }
    let Some(details) = details else {
        return Ok(ReviewPollOutcome {
            summary: "PR details are not available yet; waiting for review feedback".to_string(),
            fix_prompt: None,
            complete: false,
        });
    };
    let authors = config
        .auto
        .review_reviewer_identities
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let feedback = actionable_review_feedback(
        details,
        ReviewFeedbackFilter {
            after,
            authors: &authors,
        },
    );
    if feedback.is_actionable() {
        let prompt =
            render_auto_review_fix_prompt(summary.number, &persisted.run.branch, &feedback);
        return Ok(ReviewPollOutcome {
            summary: format_review_feedback_summary(&feedback),
            fix_prompt: Some(prompt),
            complete: false,
        });
    }
    let total_feedback =
        details.comments.len() + details.reviews.len() + details.review_comments.len();
    if total_feedback > 0 {
        return Ok(ReviewPollOutcome {
            summary: format!(
                "no actionable review feedback; skipped {} resolved, old, empty, or filtered item(s)",
                feedback.skipped_resolved_inline
                    + feedback.skipped_old
                    + feedback.skipped_empty
                    + feedback.skipped_author
            ),
            fix_prompt: None,
            complete: true,
        });
    }
    if summary.review_decision == "APPROVED" {
        return Ok(ReviewPollOutcome {
            summary: "review decision is approved; continuing".to_string(),
            fix_prompt: None,
            complete: true,
        });
    }
    Ok(ReviewPollOutcome {
        summary: "no review feedback found yet".to_string(),
        fix_prompt: None,
        complete: false,
    })
}

fn has_configured_reviewer_requested(summary: &crate::github::PrSummary, config: &Config) -> bool {
    if config.auto.review_reviewer_identities.is_empty() {
        return !summary.requested_reviewers.is_empty();
    }
    summary.requested_reviewers.iter().any(|reviewer| {
        config
            .auto
            .review_reviewer_identities
            .iter()
            .any(|configured| reviewer.eq_ignore_ascii_case(configured))
    })
}

fn review_baseline_json(summary: &crate::github::PrSummary) -> String {
    serde_json::to_string(&ReviewBaseline {
        head_sha: summary.head_sha.clone(),
        updated_at: summary.updated_at.clone(),
    })
    .unwrap_or_else(|_| "{}".to_string())
}

fn parse_review_baseline(value: Option<&str>) -> Option<ReviewBaseline> {
    value.and_then(|value| serde_json::from_str(value).ok())
}

fn render_auto_review_fix_prompt(
    pr_number: u64,
    branch: &str,
    feedback: &ReviewFeedback<'_>,
) -> String {
    let mut prompt = format!(
        "Resolve the actionable review feedback for PR #{pr_number} on branch {branch}. Stop without committing.\n\n"
    );
    if !feedback.inline_comments.is_empty() {
        prompt.push_str("Inline review comments:\n\n");
        for comment in &feedback.inline_comments {
            let line = if comment.line.trim().is_empty() {
                String::new()
            } else {
                format!(" line {}", comment.line)
            };
            prompt.push_str(&format!(
                "- {}{} by {}\n\n{}\n\n",
                crate::util::empty_dash(&comment.path),
                line,
                crate::util::empty_dash(&comment.author),
                comment.body.trim()
            ));
        }
    }
    if !feedback.review_bodies.is_empty() {
        prompt.push_str("Review bodies:\n\n");
        for review in &feedback.review_bodies {
            let state = if review.state.trim().is_empty() {
                String::new()
            } else {
                format!(" ({})", review.state.trim())
            };
            prompt.push_str(&format!(
                "- Review from {}{}\n\n{}\n\n",
                crate::util::empty_dash(&review.author),
                state,
                review.body.trim()
            ));
        }
    }
    if !feedback.pr_comments.is_empty() {
        prompt.push_str("PR comments:\n\n");
        for comment in &feedback.pr_comments {
            prompt.push_str(&format!(
                "- Comment from {}\n\n{}\n\n",
                crate::util::empty_dash(&comment.author),
                comment.body.trim()
            ));
        }
    }
    prompt
}

fn format_review_feedback_summary(feedback: &ReviewFeedback<'_>) -> String {
    format!(
        "found actionable review feedback: {} inline, {} review body, {} PR comment(s)",
        feedback.inline_comments.len(),
        feedback.review_bodies.len(),
        feedback.pr_comments.len()
    )
}

fn has_step_key(persisted: &PersistedAutoRun, key: &AutoStepKey) -> bool {
    persisted
        .steps
        .iter()
        .any(|step| step.step_key.as_str() == key.as_str())
}

fn has_step_status(
    persisted: &PersistedAutoRun,
    key: &AutoStepKey,
    status: AutoStepStatus,
) -> bool {
    persisted
        .steps
        .iter()
        .any(|step| step.step_key.as_str() == key.as_str() && step.status == status)
}

fn latest_step_status(persisted: &PersistedAutoRun, key: &AutoStepKey) -> Option<AutoStepStatus> {
    persisted
        .steps
        .iter()
        .rev()
        .find(|step| step.step_key.as_str() == key.as_str())
        .map(|step| step.status)
}

fn latest_unfinished_verify_after_fix(persisted: &PersistedAutoRun) -> Option<AutoStepKey> {
    let fix_sequence = persisted
        .steps
        .iter()
        .rev()
        .find(|step| {
            step.step_key == AutoStepKey::FixLocalVerify && step.status == AutoStepStatus::Done
        })?
        .sequence;
    persisted
        .steps
        .iter()
        .rev()
        .find(|step| {
            step.sequence < fix_sequence
                && matches!(
                    step.step_key,
                    AutoStepKey::LocalVerify
                        | AutoStepKey::VerifyReviewFix
                        | AutoStepKey::VerifyCiFix
                )
                && step.status == AutoStepStatus::Failed
        })
        .map(|step| step.step_key.clone())
}

fn review_loop_complete(persisted: &PersistedAutoRun) -> bool {
    let Some(wait) = persisted
        .steps
        .iter()
        .rev()
        .find(|step| step.step_key == AutoStepKey::WaitReview)
    else {
        return false;
    };
    wait.status == AutoStepStatus::Skipped
        && persisted
            .steps
            .iter()
            .filter(|step| step.sequence > wait.sequence)
            .all(|step| {
                matches!(
                    step.status,
                    AutoStepStatus::Done | AutoStepStatus::Skipped | AutoStepStatus::Failed
                )
            })
}

fn ci_loop_complete(persisted: &PersistedAutoRun) -> bool {
    let Some(wait) = persisted
        .steps
        .iter()
        .rev()
        .find(|step| step.step_key == AutoStepKey::WaitCi)
    else {
        return false;
    };
    wait.status == AutoStepStatus::Done
        && persisted
            .steps
            .iter()
            .filter(|step| step.sequence > wait.sequence)
            .all(|step| {
                matches!(
                    step.status,
                    AutoStepStatus::Done | AutoStepStatus::Skipped | AutoStepStatus::Failed
                )
            })
}

fn merge_or_manual_merge_complete(persisted: &PersistedAutoRun) -> bool {
    match latest_step_status(persisted, &AutoStepKey::Merge) {
        Some(AutoStepStatus::Skipped) => true,
        Some(AutoStepStatus::Done) => matches!(
            latest_step_status(persisted, &AutoStepKey::Cleanup),
            Some(AutoStepStatus::Done | AutoStepStatus::Skipped)
        ),
        _ => false,
    }
}

fn cleanup_warnings(repo: &Repository, config: &Config, worktree_path: &Path) -> Vec<String> {
    crate::session::discover_sessions(repo, config)
        .ok()
        .and_then(|sessions| {
            sessions
                .into_iter()
                .find(|session| session.path == worktree_path)
                .map(|session| session.deletion_warnings())
        })
        .unwrap_or_default()
}

fn empty_or_unknown(value: &str) -> &str {
    if value.trim().is_empty() {
        "unknown"
    } else {
        value.trim()
    }
}

fn format_verify_result(result: &VerifyResult) -> String {
    let mut lines = Vec::new();
    lines.push(if result.passed {
        "local verification passed".to_string()
    } else {
        "local verification failed".to_string()
    });
    for check in &result.checks {
        let state = if check.passed { "passed" } else { "failed" };
        lines.push(format!("- {}: {state}: {}", check.label, check.message));
    }
    lines.join("\n")
}

fn implementation_commit_message(run: &AutoRun) -> String {
    let summary = run.prompt_summary.trim();
    if summary.is_empty() {
        "implement auto flow task".to_string()
    } else {
        format!("implement {summary}")
    }
}

fn auto_pr_body(config: &Config, run: &AutoRun) -> String {
    let template = config
        .prompt_templates
        .get("pr_body")
        .map(String::as_str)
        .unwrap_or("Automated Prism run for: {prompt_summary}\n\nAuto run: {auto_run_id}");
    template
        .replace("{prompt_summary}", &run.prompt_summary)
        .replace("{auto_run_id}", &run.id)
        .replace("{branch}", &run.branch)
        .replace("{head_sha}", run.current_head_sha.as_deref().unwrap_or(""))
}

fn reload_pause_request(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<bool, String> {
    let Some(run) = load_run_with_conn(conn, &persisted.run.id)? else {
        return Ok(false);
    };
    persisted.run.pause_requested = run.pause_requested;
    if run.pause_requested || run.status == AutoRunStatus::Paused {
        persisted.run.status = AutoRunStatus::Paused;
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        return Ok(true);
    }
    Ok(false)
}

fn prompt_for_step(run: &AutoRun, step: &AutoStepRun) -> String {
    match step.step_key {
        AutoStepKey::CreatePlan => auto_create_plan_prompt(run),
        AutoStepKey::ReviewPlan => auto_review_plan_prompt(run),
        AutoStepKey::Implement => auto_implementation_prompt(run),
        AutoStepKey::FixLocalVerify => auto_verify_fix_prompt(run, step),
        AutoStepKey::FixReview => auto_review_fix_prompt(run, step),
        AutoStepKey::FixCi => auto_ci_fix_prompt(run, step),
        _ => step
            .reason
            .clone()
            .filter(|reason| !reason.trim().is_empty())
            .unwrap_or_else(|| run.initial_prompt.clone()),
    }
}

fn auto_create_plan_prompt(run: &AutoRun) -> String {
    let plan_path = plan_first_plan_path(run);
    format!(
        "Create an implementation plan for the following task. Write the plan to `{}` in this repository. Do not implement the task, commit, push, create a pull request, or merge.\n\nThe plan should be concrete enough for automated execution and include phases, tests, verification, risks, observability needs, and architecture fit. Keep repository conventions and existing domain language in mind.\n\nTask:\n{}\n\nMode: {}\nVariant: {}\nAgent profile: {}",
        plan_path.display(),
        run.initial_prompt,
        run.mode.as_str(),
        run.variant,
        run.agent_profile.as_deref().unwrap_or("default")
    )
}

fn auto_review_plan_prompt(run: &AutoRun) -> String {
    let plan_path = plan_first_plan_path(run);
    format!(
        "Review `{}` for the Auto Flow task below. Edit the plan in place so it is ready for implementation. Do not implement the task, commit, push, create a pull request, or merge.\n\nReview for missing phases, hidden risks, test strategy, observability, restartability, safety boundaries, and architecture fit with this repository. Preserve useful details and tighten vague steps.\n\nTask:\n{}\n\nMode: {}\nVariant: {}\nAgent profile: {}",
        plan_path.display(),
        run.initial_prompt,
        run.mode.as_str(),
        run.variant,
        run.agent_profile.as_deref().unwrap_or("default")
    )
}

fn auto_implementation_prompt(run: &AutoRun) -> String {
    if run.mode == AutoRunMode::PlanFirst {
        let plan_path = plan_first_plan_path(run);
        format!(
            "Implement the approved plan in `{}` for this Auto Flow task. Stop after the implementation changes are complete; do not commit, push, create a pull request, or merge.\n\nOriginal task:\n{}",
            plan_path.display(),
            run.initial_prompt
        )
    } else {
        format!(
            "Implement the following task in this worktree. Stop after the implementation changes are complete; do not commit, push, create a pull request, or merge.\n\nTask:\n{}",
            run.initial_prompt
        )
    }
}

fn auto_verify_fix_prompt(run: &AutoRun, step: &AutoStepRun) -> String {
    format!(
        "Local verification failed for this Auto Flow run. Fix the failures, then stop without committing.\n\nOriginal task:\n{}\n\nFailure context:\n{}",
        run.initial_prompt,
        step.reason
            .as_deref()
            .unwrap_or("No verification details were recorded.")
    )
}

fn auto_review_fix_prompt(run: &AutoRun, step: &AutoStepRun) -> String {
    format!(
        "Resolve the review feedback for this branch, then stop without committing.\n\nOriginal task:\n{}\n\nReview context:\n{}",
        run.initial_prompt,
        step.reason
            .as_deref()
            .unwrap_or("No review details were recorded.")
    )
}

fn auto_ci_fix_prompt(run: &AutoRun, step: &AutoStepRun) -> String {
    format!(
        "CI failed for this branch. Fix the failure, then stop without committing.\n\nOriginal task:\n{}\n\nCI context:\n{}",
        run.initial_prompt,
        step.reason
            .as_deref()
            .unwrap_or("No CI details were recorded.")
    )
}

fn plan_first_plan_path(run: &AutoRun) -> PathBuf {
    run.plan_path
        .clone()
        .unwrap_or_else(|| run.worktree_path.join("plan.md"))
}

fn auto_plan_path(run: &AutoRun) -> Result<PathBuf, String> {
    match run.implementation_source {
        AutoImplementationSource::Prompt => {
            Err("prompt auto flow does not have a plan path".to_string())
        }
        AutoImplementationSource::ExistingPlan => run
            .plan_path
            .clone()
            .ok_or_else(|| "existing-plan auto flow requires a plan path".to_string()),
        AutoImplementationSource::DraftPlan => Ok(plan_first_plan_path(run)),
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

fn plan_run_mode_label(mode: PlanRunMode) -> &'static str {
    match mode {
        PlanRunMode::Sequential => "sequential",
        PlanRunMode::Parallel => "parallel",
    }
}

fn parse_plan_run_mode(value: &str) -> Result<PlanRunMode, String> {
    match value {
        "sequential" => Ok(PlanRunMode::Sequential),
        "parallel" => Ok(PlanRunMode::Parallel),
        _ => Err(format!("unknown plan run mode: {value}")),
    }
}

fn opencode_run_command(
    executor: &AutoExecutorConfig,
    step: &AutoStepRun,
    prompt: &str,
    attach: bool,
) -> Command {
    let mut command = Command::new(&executor.opencode_program);
    command.arg("run");
    if attach && let Some(server_url) = executor.server_url.as_deref() {
        command.arg("--attach").arg(server_url);
    }
    command
        .arg("--format")
        .arg("json")
        .arg("--dir")
        .arg(&executor.worktree_path)
        .arg("--title")
        .arg(format!(
            "{} {} attempt {}",
            executor.title_prefix,
            step.step_key.as_str(),
            step.attempt
        ))
        .arg(prompt)
        .current_dir(&executor.worktree_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn spawn_opencode(command: &mut Command) -> Result<Child, String> {
    observability::emit(observability::EventInput {
        level: LogLevel::Info,
        target: "auto_flow",
        action: "external_command",
        operation_id: None,
        parent_operation_id: None,
        branch: None,
        session: None,
        message: "spawning Auto Flow command".to_string(),
        data_json: Some(observability::command_data_json(
            command,
            false,
            None,
            Some("spawn"),
            None,
        )),
    });
    match command.spawn() {
        Ok(child) => {
            observability::emit(observability::EventInput {
                level: LogLevel::Info,
                target: "auto_flow",
                action: "external_command",
                operation_id: None,
                parent_operation_id: None,
                branch: None,
                session: None,
                message: "Auto Flow command spawned".to_string(),
                data_json: Some(format!("{{\"pid\":{}}}", child.id())),
            });
            Ok(child)
        }
        Err(error) => {
            observability::emit(observability::EventInput {
                level: LogLevel::Warn,
                target: "auto_flow",
                action: "external_command",
                operation_id: None,
                parent_operation_id: None,
                branch: None,
                session: None,
                message: format!("Auto Flow command spawn failed: {error}"),
                data_json: Some(observability::command_data_json(
                    command,
                    false,
                    None,
                    Some("spawn_failed"),
                    Some(&error.to_string()),
                )),
            });
            Err(format!("opencode: {error}"))
        }
    }
}

fn emit_auto_run_log(run: &AutoRun) {
    observability::emit(observability::EventInput {
        level: LogLevel::Debug,
        target: "auto_flow",
        action: "run_state",
        operation_id: None,
        parent_operation_id: None,
        branch: Some(run.branch.clone()),
        session: None,
        message: format!("Auto Flow run {} is {}", run.id, run.status.as_str()),
        data_json: Some(format!(
            "{{\"run_id\":{},\"status\":{},\"mode\":{},\"pause_requested\":{},\"pr_number\":{},\"current_head_sha\":{}}}",
            json_string(&run.id),
            json_string(run.status.as_str()),
            json_string(run.mode.as_str()),
            run.pause_requested,
            run.pr_number
                .map(|number| number.to_string())
                .unwrap_or_else(|| "null".to_string()),
            run.current_head_sha
                .as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".to_string())
        )),
    });
}

fn emit_auto_step_log(step: &AutoStepRun) {
    observability::emit(observability::EventInput {
        level: LogLevel::Debug,
        target: "auto_flow",
        action: "step_state",
        operation_id: None,
        parent_operation_id: None,
        branch: None,
        session: step.opencode_session_id.clone(),
        message: format!(
            "Auto Flow step {} attempt {} is {}",
            step.step_key.as_str(),
            step.attempt,
            step.status.as_str()
        ),
        data_json: Some(format!(
            "{{\"run_id\":{},\"step_run_id\":{},\"sequence\":{},\"step_key\":{},\"attempt\":{},\"status\":{},\"process_id\":{},\"commit_sha\":{},\"head_sha\":{}}}",
            json_string(&step.run_id),
            step.id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "null".to_string()),
            step.sequence,
            json_string(step.step_key.as_str()),
            step.attempt,
            json_string(step.status.as_str()),
            step.process_id
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "null".to_string()),
            step.commit_sha
                .as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".to_string()),
            step.head_sha
                .as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".to_string())
        )),
    });
}

fn emit_auto_event_log(event: &AutoEvent) {
    observability::emit(observability::EventInput {
        level: LogLevel::Info,
        target: "auto_flow",
        action: event.kind.as_str(),
        operation_id: None,
        parent_operation_id: None,
        branch: None,
        session: None,
        message: format!("Auto Flow event {}", event.kind),
        data_json: Some(format!(
            "{{\"run_id\":{},\"step_run_id\":{},\"kind\":{}}}",
            json_string(&event.run_id),
            event
                .step_run_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "null".to_string()),
            json_string(&event.kind)
        )),
    });
}

fn mark_spawn_failure(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    error: &str,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    step.status = AutoStepStatus::Failed;
    step.finished_unix_ms = Some(unix_ms());
    step.error = Some(error.to_string());
    let step_id = save_step_with_conn(conn, step)?;
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Error,
        error,
        None,
        max_output_lines_per_step,
    )
}

fn finish_step_after_exit(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    exit_code: i32,
    used_attach: bool,
) -> Result<(), String> {
    step.process_id = None;
    step.finished_unix_ms = Some(unix_ms());
    if exit_code == 0 {
        step.status = AutoStepStatus::Done;
        step.error = None;
    } else {
        step.status = AutoStepStatus::Failed;
        let attach_note = if used_attach { " with --attach" } else { "" };
        step.error = Some(format!("opencode run{attach_note} exited with {exit_code}"));
    }
    save_step_with_conn(conn, step)?;
    Ok(())
}

fn collect_child_output(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    child: &mut Child,
    max_output_lines_per_step: usize,
    output: &mut dyn Write,
) -> Result<i32, String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "open opencode stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "open opencode stderr".to_string())?;
    let (tx, rx) = mpsc::channel::<Result<ChildLine, String>>();
    spawn_reader_thread(StreamKind::Stdout, stdout, tx.clone());
    spawn_reader_thread(StreamKind::Stderr, stderr, tx);

    let mut readers_open = 2;
    while readers_open > 0 {
        match rx.recv() {
            Ok(Ok(ChildLine::Line { stream, text })) => {
                ingest_child_line(conn, step, stream, &text, max_output_lines_per_step, output)?;
            }
            Ok(Ok(ChildLine::End)) => readers_open -= 1,
            Ok(Err(error)) => return Err(error),
            Err(_) => break,
        }
    }

    let status = child
        .wait()
        .map_err(|error| format!("wait for opencode: {error}"))?;
    Ok(status.code().unwrap_or(1))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamKind {
    Stdout,
    Stderr,
}

#[derive(Debug)]
enum ChildLine {
    Line { stream: StreamKind, text: String },
    End,
}

fn spawn_reader_thread(
    stream: StreamKind,
    reader: impl std::io::Read + Send + 'static,
    tx: mpsc::Sender<Result<ChildLine, String>>,
) {
    thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            match line {
                Ok(text) => {
                    if tx.send(Ok(ChildLine::Line { stream, text })).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = tx.send(Err(format!("read opencode output: {error}")));
                    return;
                }
            }
        }
        let _ = tx.send(Ok(ChildLine::End));
    });
}

fn ingest_child_line(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    stream: StreamKind,
    raw: &str,
    max_output_lines_per_step: usize,
    output: &mut dyn Write,
) -> Result<(), String> {
    if stream == StreamKind::Stderr {
        let step_id = step
            .id
            .ok_or_else(|| "auto step must be saved before output".to_string())?;
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Error,
            raw,
            None,
            max_output_lines_per_step,
        )?;
        step.error = Some(raw.to_string());
        save_step_with_conn(conn, step)?;
        writeln!(output, "[stderr] {raw}")
            .map_err(|error| format!("write auto output: {error}"))?;
        return Ok(());
    }

    let events = crate::plan_run::parse_plan_agent_events(raw);
    for event in events {
        let text = ingest_single_agent_event(conn, step, event, max_output_lines_per_step)?;
        writeln!(output, "{text}").map_err(|error| format!("write auto output: {error}"))?;
    }
    save_step_with_conn(conn, step)?;
    Ok(())
}

fn ingest_single_agent_event(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    event: PlanAgentEvent,
    max_output_lines_per_step: usize,
) -> Result<String, String> {
    let (kind, text, block_id) = apply_agent_event(step, event);
    let step_id = step
        .id
        .ok_or_else(|| "auto step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        kind,
        &text,
        block_id.as_deref(),
        max_output_lines_per_step,
    )?;
    Ok(text)
}

fn apply_agent_event(
    step: &mut AutoStepRun,
    event: PlanAgentEvent,
) -> (AutoOutputKind, String, Option<String>) {
    match event {
        PlanAgentEvent::SessionIdentified { session_id, title } => {
            step.opencode_session_id = Some(session_id.clone());
            let title = title
                .map(|title| format!(" title: {title}"))
                .unwrap_or_default();
            (
                AutoOutputKind::Status,
                format!("session {session_id}{title}"),
                None,
            )
        }
        PlanAgentEvent::StateChanged { state } => {
            (AutoOutputKind::Status, format!("status: {state}"), None)
        }
        PlanAgentEvent::AssistantText { text } => {
            step.summary = Some(text.clone());
            (AutoOutputKind::Assistant, text, None)
        }
        PlanAgentEvent::ToolStarted {
            id,
            name,
            args_summary,
        } => {
            let mut text = format!("tool {name} running");
            if let Some(args) = args_summary {
                text.push_str(": ");
                text.push_str(&args);
            }
            (AutoOutputKind::Tool, text, id)
        }
        PlanAgentEvent::ToolOutput { id, text } => (AutoOutputKind::ToolOutput, text, id),
        PlanAgentEvent::ToolFinished { id, status } => {
            (AutoOutputKind::Tool, format!("tool finished: {status}"), id)
        }
        PlanAgentEvent::TodoUpdated { todos } => (
            AutoOutputKind::Status,
            format!("todos updated: {}", todos.len()),
            None,
        ),
        PlanAgentEvent::DiffUpdated { summary, patch } => {
            let text = patch
                .map(|patch| format!("{summary}\n{patch}"))
                .unwrap_or(summary);
            (AutoOutputKind::Diff, text, None)
        }
        PlanAgentEvent::Error { message } => {
            step.error = Some(message.clone());
            (AutoOutputKind::Error, message, None)
        }
        PlanAgentEvent::Raw { event_type, json } => (
            AutoOutputKind::RawJson,
            format!("[{event_type}] {json}"),
            None,
        ),
    }
}

fn append_system_output(
    conn: &rusqlite::Connection,
    step_run_id: i64,
    kind: AutoOutputKind,
    text: &str,
    block_id: Option<&str>,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let line_number = next_output_line_number(conn, step_run_id)?;
    append_output_line_limited(
        conn,
        &AutoOutputLine {
            step_run_id,
            line_number,
            time_unix_ms: unix_ms(),
            kind,
            text: text.to_string(),
            block_id: block_id.map(str::to_string),
        },
        max_output_lines_per_step,
    )
}

fn next_output_line_number(conn: &rusqlite::Connection, step_run_id: i64) -> Result<u64, String> {
    let current: Option<i64> = conn
        .query_row(
            "select max(line_number) from auto_output_line where step_run_id = ?1",
            params![step_run_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("read auto output line number: {error}"))?;
    Ok(current.map(i64_to_next_u64).unwrap_or(1))
}

fn trim_output_lines(
    conn: &rusqlite::Connection,
    step_run_id: i64,
    max_lines_per_step: usize,
) -> Result<(), String> {
    if max_lines_per_step == 0 {
        return Ok(());
    }
    let retained_line_count = max_lines_per_step.saturating_sub(1);
    if retained_line_count == 0 {
        conn.execute(
            "delete from auto_output_line where step_run_id = ?1",
            params![step_run_id],
        )
        .map_err(|error| format!("trim auto output lines: {error}"))?;
        return Ok(());
    }
    let deleted = conn
        .execute(
            "delete from auto_output_line
             where step_run_id = ?1
               and line_number not in (
                 select line_number
                 from auto_output_line
                 where step_run_id = ?1
                 order by line_number desc
                 limit ?2
               )",
            params![step_run_id, usize_to_i64(retained_line_count)],
        )
        .map_err(|error| format!("trim auto output lines: {error}"))?;
    if deleted == 0 {
        return Ok(());
    }
    let first_retained: Option<i64> = conn
        .query_row(
            "select min(line_number) from auto_output_line where step_run_id = ?1",
            params![step_run_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("read retained auto output marker position: {error}"))?;
    let Some(first_retained) = first_retained else {
        return Ok(());
    };
    let marker_line = first_retained.saturating_sub(1);
    conn.execute(
        "insert or replace into auto_output_line (
           step_run_id, line_number, time_unix_ms, kind, text, block_id
         ) values (?1, ?2, ?3, 'system', ?4, null)",
        params![
            step_run_id,
            marker_line,
            u64_to_i64(unix_ms()),
            format!("[... omitted {deleted} older output lines ...]"),
        ],
    )
    .map_err(|error| format!("write auto output omission marker: {error}"))?;
    Ok(())
}

#[cfg(unix)]
fn terminate_process(process_id: u32) -> Result<(), String> {
    let result = unsafe { libc::kill(process_id as libc::pid_t, libc::SIGTERM) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "terminate opencode process {process_id}: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(not(unix))]
fn terminate_process(process_id: u32) -> Result<(), String> {
    Command::new("taskkill")
        .args(["/PID", &process_id.to_string(), "/T", "/F"])
        .status()
        .map_err(|error| format!("terminate opencode process {process_id}: {error}"))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(format!("terminate opencode process {process_id}: {status}"))
            }
        })
}

fn summarize_prompt(prompt: &str) -> String {
    let collapsed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= 96 {
        return collapsed;
    }
    let mut summary = collapsed.chars().take(93).collect::<String>();
    summary.push_str("...");
    summary
}

fn stable_string_hash(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn from_string_error(error: String) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(error.into())
}

fn i64_to_usize(value: i64, index: usize) -> usize {
    usize::try_from(value)
        .unwrap_or_else(|_| panic!("SQLite column {index} contained invalid usize: {value}"))
}

fn i64_to_u64(value: i64, index: usize) -> u64 {
    u64::try_from(value)
        .unwrap_or_else(|_| panic!("SQLite column {index} contained invalid u64: {value}"))
}

fn i64_to_next_u64(value: i64) -> u64 {
    value.max(0) as u64 + 1
}

fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn bool_to_i64(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn table_has_column(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
) -> Result<bool, String> {
    let mut statement = conn
        .prepare(&format!("pragma table_info({table})"))
        .map_err(|error| format!("prepare table info: {error}"))?;
    let mut rows = statement
        .query([])
        .map_err(|error| format!("read table info: {error}"))?;
    while let Some(row) = rows
        .next()
        .map_err(|error| format!("read column info: {error}"))?
    {
        let name = row
            .get::<_, String>(1)
            .map_err(|error| format!("read column name: {error}"))?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Self {
            let unique = unix_ms();
            let path = std::env::temp_dir().join(format!(
                "prism-auto-flow-test-{name}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn launch_creates_persistable_auto_run() {
        let repo = PathBuf::from("/repo/prism");
        let launch = AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
            .expect("launch");

        let persisted = launch.create_run();

        assert_eq!(persisted.run.status, AutoRunStatus::Queued);
        assert_eq!(persisted.run.branch, "feat/auto");
        assert_eq!(persisted.run.prompt_summary, "Implement auto");
        assert_eq!(persisted.steps.len(), 1);
        assert_eq!(persisted.steps[0].sequence, 1);
        assert_eq!(persisted.steps[0].step_key, AutoStepKey::Prepare);
    }

    #[test]
    fn plan_first_prompts_create_and_review_plan_file() {
        let repo = PathBuf::from("/repo/prism");
        let persisted = AutoLaunch::with_options(
            &repo,
            &repo.join("feature"),
            AutoLaunchOptions {
                branch: "feat/auto".to_string(),
                mode: AutoRunMode::PlanFirst,
                implementation_source: AutoImplementationSource::DraftPlan,
                plan_path: Some(repo.join("feature/plan.md")),
                plan_run_mode: PlanRunMode::Sequential,
                variant: "intensive".to_string(),
                agent_profile: Some("planner".to_string()),
                initial_prompt: "Implement auto".to_string(),
            },
        )
        .unwrap()
        .create_run();

        let create_prompt = prompt_for_step(
            &persisted.run,
            &AutoStepRun::queued(&persisted.run.id, 2, AutoStepKey::CreatePlan, 1, None),
        );
        let review_prompt = prompt_for_step(
            &persisted.run,
            &AutoStepRun::queued(&persisted.run.id, 3, AutoStepKey::ReviewPlan, 1, None),
        );
        assert!(create_prompt.contains("/repo/prism/feature/plan.md"));
        assert!(create_prompt.contains("Do not implement"));
        assert!(create_prompt.contains("Variant: intensive"));
        assert!(create_prompt.contains("Agent profile: planner"));
        assert!(review_prompt.contains("missing phases"));
        assert!(review_prompt.contains("Edit the plan in place"));
    }

    #[test]
    fn plan_first_queues_prelude_before_run_plan() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted = AutoLaunch::with_options(
            &repo,
            &repo.join("feature"),
            AutoLaunchOptions {
                branch: "feat/auto".to_string(),
                mode: AutoRunMode::PlanFirst,
                implementation_source: AutoImplementationSource::DraftPlan,
                plan_path: Some(repo.join("feature/plan.md")),
                plan_run_mode: PlanRunMode::Sequential,
                variant: "intensive".to_string(),
                agent_profile: None,
                initial_prompt: "Implement auto".to_string(),
            },
        )
        .unwrap()
        .create_run();
        persisted.steps[0].status = AutoStepStatus::Done;
        save_auto_run(&conn, &mut persisted).unwrap();

        assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());
        assert_eq!(persisted.steps[1].step_key, AutoStepKey::CreatePlan);
        persisted.steps[1].status = AutoStepStatus::Done;
        save_auto_run(&conn, &mut persisted).unwrap();

        assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());
        assert_eq!(persisted.steps[2].step_key, AutoStepKey::ReviewPlan);
        persisted.steps[2].status = AutoStepStatus::Done;
        save_auto_run(&conn, &mut persisted).unwrap();

        assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());
        assert_eq!(persisted.steps[3].step_key, AutoStepKey::ApprovePlan);
        assert!(
            !persisted
                .steps
                .iter()
                .any(|step| step.step_key == AutoStepKey::Implement)
        );
        persisted.steps[3].status = AutoStepStatus::Done;
        save_auto_run(&conn, &mut persisted).unwrap();

        assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());
        assert_eq!(persisted.steps[4].step_key, AutoStepKey::RunPlan);
    }

    #[test]
    fn plan_approval_pauses_and_resume_queues_run_plan() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted = AutoLaunch::with_options(
            &repo,
            &repo.join("feature"),
            AutoLaunchOptions {
                branch: "feat/auto".to_string(),
                mode: AutoRunMode::PlanFirst,
                implementation_source: AutoImplementationSource::DraftPlan,
                plan_path: Some(repo.join("feature/plan.md")),
                plan_run_mode: PlanRunMode::Sequential,
                variant: "intensive".to_string(),
                agent_profile: None,
                initial_prompt: "Implement auto".to_string(),
            },
        )
        .unwrap()
        .create_run();
        persisted.steps.clear();
        push_test_step(
            &mut persisted,
            1,
            AutoStepKey::CreatePlan,
            AutoStepStatus::Done,
        );
        push_test_step(
            &mut persisted,
            2,
            AutoStepKey::ReviewPlan,
            AutoStepStatus::Done,
        );
        persisted.steps.push(AutoStepRun::queued(
            &persisted.run.id,
            3,
            AutoStepKey::ApprovePlan,
            1,
            Some("approve".to_string()),
        ));
        save_auto_run(&conn, &mut persisted).unwrap();
        start_non_agent_step(&conn, &mut persisted, 2).unwrap();

        execute_approve_plan_step(&conn, &mut persisted, 2, 100).unwrap();

        assert_eq!(persisted.run.status, AutoRunStatus::Paused);
        assert!(persisted.run.pause_requested);
        assert_eq!(persisted.steps[2].status, AutoStepStatus::Done);

        resume_paused_auto_run(&conn, &mut persisted).unwrap();
        assert!(prepare_auto_run_for_resume(&conn, &mut persisted, 100).unwrap());
        assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());
        assert!(
            persisted
                .steps
                .iter()
                .any(|step| step.step_key == AutoStepKey::RunPlan)
        );
    }

    #[test]
    fn existing_plan_queues_run_plan() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted = AutoLaunch::with_options(
            &repo,
            &repo.join("feature"),
            AutoLaunchOptions {
                branch: "feat/auto".to_string(),
                mode: AutoRunMode::Standard,
                implementation_source: AutoImplementationSource::ExistingPlan,
                plan_path: Some(repo.join("feature/plan.md")),
                plan_run_mode: PlanRunMode::Sequential,
                variant: "default".to_string(),
                agent_profile: None,
                initial_prompt: "Implement existing plan".to_string(),
            },
        )
        .unwrap()
        .create_run();
        persisted.steps[0].status = AutoStepStatus::Done;
        save_auto_run(&conn, &mut persisted).unwrap();

        assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());

        assert_eq!(persisted.steps[1].step_key, AutoStepKey::RunPlan);
    }

    #[test]
    #[cfg(unix)]
    fn run_plan_success_queues_local_verify() {
        let temp = TempDir::new("run-plan-success");
        let work = temp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("plan.md"), "# Phase 1\n\nImplement it.\n").unwrap();
        let repo =
            Repository::with_config_dir_for_test(work.clone(), temp.path().join("prism-config"));
        let mut config = Config::load(&repo);
        let opencode = temp.path().join("opencode");
        write_executable(
            &opencode,
            r#"#!/bin/sh
printf '%s\n' '{"type":"message","text":"phase done"}'
"#,
        );
        config
            .tools
            .insert("opencode".to_string(), opencode.display().to_string());
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        crate::plan_run::migrate_schema(&conn).unwrap();
        let mut persisted = AutoLaunch::with_options(
            &work,
            &work,
            AutoLaunchOptions {
                branch: "feat/auto".to_string(),
                mode: AutoRunMode::Standard,
                implementation_source: AutoImplementationSource::ExistingPlan,
                plan_path: Some(work.join("plan.md")),
                plan_run_mode: PlanRunMode::Sequential,
                variant: "default".to_string(),
                agent_profile: None,
                initial_prompt: "Implement existing plan".to_string(),
            },
        )
        .unwrap()
        .create_run();
        persisted.steps.clear();
        persisted.steps.push(AutoStepRun::queued(
            &persisted.run.id,
            1,
            AutoStepKey::RunPlan,
            1,
            Some("run plan".to_string()),
        ));
        save_auto_run(&conn, &mut persisted).unwrap();
        start_non_agent_step(&conn, &mut persisted, 0).unwrap();

        execute_run_plan_step(&conn, &repo, &config, &mut persisted, 0, 100).unwrap();
        assert_eq!(persisted.steps[0].status, AutoStepStatus::Done);
        assert!(persisted.steps[0].plan_run_id.is_some());

        assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());
        assert!(
            persisted
                .steps
                .iter()
                .any(|step| step.step_key == AutoStepKey::LocalVerify)
        );
    }

    #[test]
    #[cfg(unix)]
    fn run_plan_failure_marks_auto_step_failed() {
        let temp = TempDir::new("run-plan-failure");
        let work = temp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("plan.md"), "# Phase 1\n\nImplement it.\n").unwrap();
        let repo =
            Repository::with_config_dir_for_test(work.clone(), temp.path().join("prism-config"));
        let mut config = Config::load(&repo);
        let opencode = temp.path().join("opencode");
        write_executable(
            &opencode,
            r#"#!/bin/sh
printf '%s\n' '{"type":"message","text":"phase failed"}'
exit 7
"#,
        );
        config
            .tools
            .insert("opencode".to_string(), opencode.display().to_string());
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        crate::plan_run::migrate_schema(&conn).unwrap();
        let mut persisted = AutoLaunch::with_options(
            &work,
            &work,
            AutoLaunchOptions {
                branch: "feat/auto".to_string(),
                mode: AutoRunMode::Standard,
                implementation_source: AutoImplementationSource::ExistingPlan,
                plan_path: Some(work.join("plan.md")),
                plan_run_mode: PlanRunMode::Sequential,
                variant: "default".to_string(),
                agent_profile: None,
                initial_prompt: "Implement existing plan".to_string(),
            },
        )
        .unwrap()
        .create_run();
        persisted.steps.clear();
        persisted.steps.push(AutoStepRun::queued(
            &persisted.run.id,
            1,
            AutoStepKey::RunPlan,
            1,
            Some("run plan".to_string()),
        ));
        save_auto_run(&conn, &mut persisted).unwrap();
        start_non_agent_step(&conn, &mut persisted, 0).unwrap();

        let error = execute_run_plan_step(&conn, &repo, &config, &mut persisted, 0, 100)
            .expect_err("run-plan should fail when linked phase fails");

        assert!(error.contains("inspect linked plan dashboard"));
        assert_eq!(persisted.steps[0].status, AutoStepStatus::Failed);
        assert_eq!(
            persisted.steps[0].summary.as_deref(),
            Some("plan run failed")
        );
        assert!(
            persisted.steps[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ended with status failed"))
        );
        let plan_run_id = persisted.steps[0].plan_run_id.as_deref().unwrap();
        let linked_plan = load_plan_run(&conn, plan_run_id).unwrap().unwrap();
        assert_eq!(linked_plan.run.status, PlanRunStatus::Failed);
    }

    #[test]
    fn resume_reconciles_interrupted_linked_plan_before_auto_stale_failure() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        crate::plan_run::migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted = linked_run_plan_auto_run(&conn, &repo);
        let plan_run_id = persisted.steps[0].plan_run_id.clone().unwrap();
        let mut plan_run = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
        plan_run.run.status = PlanRunStatus::Running;
        plan_run.steps[0].status = crate::plan_run::PlanStepStatus::Running;
        crate::plan_run::save_plan_run(&conn, &plan_run).unwrap();

        assert!(prepare_auto_run_for_resume(&conn, &mut persisted, 100).unwrap());

        assert_eq!(persisted.steps[0].status, AutoStepStatus::Queued);
        assert_eq!(persisted.run.status, AutoRunStatus::Queued);
        let loaded_plan = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
        assert_eq!(
            loaded_plan.steps[0].status,
            crate::plan_run::PlanStepStatus::Queued
        );
    }

    #[test]
    fn resume_marks_run_plan_done_when_linked_plan_finished() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        crate::plan_run::migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted = linked_run_plan_auto_run(&conn, &repo);
        let plan_run_id = persisted.steps[0].plan_run_id.clone().unwrap();
        let mut plan_run = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
        plan_run.run.status = PlanRunStatus::Done;
        plan_run.steps[0].status = crate::plan_run::PlanStepStatus::Done;
        crate::plan_run::save_plan_run(&conn, &plan_run).unwrap();

        assert!(prepare_auto_run_for_resume(&conn, &mut persisted, 100).unwrap());

        assert_eq!(persisted.steps[0].status, AutoStepStatus::Done);
        assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());
        assert_eq!(persisted.steps[1].step_key, AutoStepKey::LocalVerify);
    }

    #[test]
    fn retry_failed_run_plan_requeues_linked_failed_phase() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        crate::plan_run::migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted = linked_run_plan_auto_run(&conn, &repo);
        let plan_run_id = persisted.steps[0].plan_run_id.clone().unwrap();
        persisted.steps[0].status = AutoStepStatus::Failed;
        save_auto_run(&conn, &mut persisted).unwrap();
        let mut plan_run = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
        plan_run.run.status = PlanRunStatus::Failed;
        plan_run.steps[0].status = crate::plan_run::PlanStepStatus::Failed;
        crate::plan_run::save_plan_run(&conn, &plan_run).unwrap();

        retry_failed_auto_step(&conn, &mut persisted).unwrap();

        assert_eq!(persisted.steps.len(), 1);
        assert_eq!(persisted.steps[0].status, AutoStepStatus::Queued);
        let loaded_plan = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
        assert_eq!(
            loaded_plan.steps[0].status,
            crate::plan_run::PlanStepStatus::Queued
        );
    }

    #[test]
    fn retry_from_run_plan_resets_later_auto_steps() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        crate::plan_run::migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted = linked_run_plan_auto_run(&conn, &repo);
        persisted.steps[0].status = AutoStepStatus::Done;
        save_auto_run(&conn, &mut persisted).unwrap();
        append_step_run(
            &conn,
            &mut persisted,
            AutoStepKey::LocalVerify,
            Some("verify".to_string()),
        )
        .unwrap();
        persisted.steps[1].status = AutoStepStatus::Done;
        save_auto_run(&conn, &mut persisted).unwrap();
        let selected = persisted.steps[0].id.unwrap();

        retry_auto_from_step(&conn, &mut persisted, selected).unwrap();

        assert_eq!(persisted.steps[0].status, AutoStepStatus::Queued);
        assert_eq!(persisted.steps[1].status, AutoStepStatus::Queued);
    }

    #[test]
    fn pause_auto_run_requests_linked_plan_pause() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        crate::plan_run::migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted = linked_run_plan_auto_run(&conn, &repo);
        let plan_run_id = persisted.steps[0].plan_run_id.clone().unwrap();

        request_auto_run_pause(&conn, &mut persisted).unwrap();

        let loaded_plan = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
        assert!(loaded_plan.run.pause_requested);
    }

    #[test]
    fn aggregate_status_handles_waiting_and_failures() {
        assert_eq!(
            aggregate_step_status([AutoStepStatus::Done, AutoStepStatus::Waiting]),
            AutoRunStatus::Running
        );
        assert_eq!(
            aggregate_step_status([AutoStepStatus::Done, AutoStepStatus::Queued]),
            AutoRunStatus::Queued
        );
        assert_eq!(
            aggregate_step_status([AutoStepStatus::Running, AutoStepStatus::Failed]),
            AutoRunStatus::Failed
        );
        assert_eq!(
            aggregate_step_status([AutoStepStatus::Done, AutoStepStatus::Skipped]),
            AutoRunStatus::Done
        );
    }

    #[test]
    fn schema_round_trips_run_steps_and_output() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted =
            AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
                .unwrap()
                .create_run();
        persisted.run.status = AutoRunStatus::Running;
        persisted.run.pr_number = Some(42);
        persisted.steps[0].status = AutoStepStatus::Done;
        persisted.steps[0].summary = Some("prepared".to_string());
        persisted.steps.push(AutoStepRun::running(
            &persisted.run.id,
            2,
            AutoStepKey::Implement,
            1,
        ));
        persisted.steps[1].plan_run_id = Some("plan-linked".to_string());
        persisted.run.selected_step_run_id = Some(2);

        save_auto_run(&conn, &mut persisted).unwrap();
        let implement_id = persisted.steps[1].id.expect("step id");
        append_output_line(
            &conn,
            &AutoOutputLine {
                step_run_id: implement_id,
                line_number: 1,
                time_unix_ms: 100,
                kind: AutoOutputKind::Assistant,
                text: "working".to_string(),
                block_id: None,
            },
        )
        .unwrap();

        let loaded = load_auto_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("run");

        assert_eq!(loaded.run, persisted.run);
        assert_eq!(loaded.steps, persisted.steps);
        assert_eq!(loaded.status_counts().done, 1);
        assert_eq!(loaded.status_counts().running, 1);
        assert_eq!(
            load_output_lines(&conn, implement_id).unwrap()[0].text,
            "working"
        );
    }

    #[test]
    fn schema_round_trips_prompt_existing_plan_and_draft_plan_sources() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");

        let mut prompt = AutoLaunch::with_options(
            &repo,
            &repo.join("prompt"),
            AutoLaunchOptions {
                branch: "feat/prompt".to_string(),
                mode: AutoRunMode::Standard,
                implementation_source: AutoImplementationSource::Prompt,
                plan_path: None,
                plan_run_mode: PlanRunMode::Sequential,
                variant: "default".to_string(),
                agent_profile: None,
                initial_prompt: "Implement prompt task".to_string(),
            },
        )
        .unwrap()
        .create_run();
        let mut existing_plan = AutoLaunch::with_options(
            &repo,
            &repo.join("existing"),
            AutoLaunchOptions {
                branch: "feat/existing".to_string(),
                mode: AutoRunMode::Standard,
                implementation_source: AutoImplementationSource::ExistingPlan,
                plan_path: Some(repo.join("existing/plan.md")),
                plan_run_mode: PlanRunMode::Parallel,
                variant: "default".to_string(),
                agent_profile: None,
                initial_prompt: "Implement existing plan".to_string(),
            },
        )
        .unwrap()
        .create_run();
        let mut draft_plan = AutoLaunch::with_options(
            &repo,
            &repo.join("draft"),
            AutoLaunchOptions {
                branch: "feat/draft".to_string(),
                mode: AutoRunMode::PlanFirst,
                implementation_source: AutoImplementationSource::DraftPlan,
                plan_path: Some(repo.join("draft/plan.md")),
                plan_run_mode: PlanRunMode::Sequential,
                variant: "intensive".to_string(),
                agent_profile: None,
                initial_prompt: "Draft then implement plan".to_string(),
            },
        )
        .unwrap()
        .create_run();

        save_auto_run(&conn, &mut prompt).unwrap();
        save_auto_run(&conn, &mut existing_plan).unwrap();
        save_auto_run(&conn, &mut draft_plan).unwrap();

        let prompt = load_auto_run(&conn, &prompt.run.id).unwrap().unwrap();
        let existing_plan = load_auto_run(&conn, &existing_plan.run.id)
            .unwrap()
            .unwrap();
        let draft_plan = load_auto_run(&conn, &draft_plan.run.id).unwrap().unwrap();

        assert_eq!(
            prompt.run.implementation_source,
            AutoImplementationSource::Prompt
        );
        assert_eq!(prompt.run.plan_path, None);
        assert_eq!(
            existing_plan.run.implementation_source,
            AutoImplementationSource::ExistingPlan
        );
        assert_eq!(
            existing_plan.run.plan_path,
            Some(repo.join("existing/plan.md"))
        );
        assert_eq!(existing_plan.run.plan_run_mode, PlanRunMode::Parallel);
        assert_eq!(
            draft_plan.run.implementation_source,
            AutoImplementationSource::DraftPlan
        );
        assert_eq!(draft_plan.run.plan_path, Some(repo.join("draft/plan.md")));
    }

    #[test]
    fn repeated_attempts_retain_distinct_output() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted =
            AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
                .unwrap()
                .create_run();
        save_auto_run(&conn, &mut persisted).unwrap();

        let first_id = append_step_run(
            &conn,
            &mut persisted,
            AutoStepKey::FixReview,
            Some("first review fix".to_string()),
        )
        .unwrap();
        persisted.steps[1].status = AutoStepStatus::Failed;
        save_auto_run(&conn, &mut persisted).unwrap();
        append_output_line(
            &conn,
            &AutoOutputLine {
                step_run_id: first_id,
                line_number: 1,
                time_unix_ms: 101,
                kind: AutoOutputKind::Error,
                text: "first failed".to_string(),
                block_id: None,
            },
        )
        .unwrap();

        let second_id = append_step_run(
            &conn,
            &mut persisted,
            AutoStepKey::FixReview,
            Some("second review fix".to_string()),
        )
        .unwrap();
        append_output_line(
            &conn,
            &AutoOutputLine {
                step_run_id: second_id,
                line_number: 1,
                time_unix_ms: 102,
                kind: AutoOutputKind::Assistant,
                text: "second running".to_string(),
                block_id: None,
            },
        )
        .unwrap();

        let loaded = load_auto_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("run");
        let fix_attempts = loaded
            .steps
            .iter()
            .filter(|step| step.step_key == AutoStepKey::FixReview)
            .collect::<Vec<_>>();

        assert_eq!(fix_attempts.len(), 2);
        assert_eq!(fix_attempts[0].attempt, 1);
        assert_eq!(fix_attempts[1].attempt, 2);
        assert_eq!(
            load_output_lines(&conn, first_id).unwrap()[0].text,
            "first failed"
        );
        assert_eq!(
            load_output_lines(&conn, second_id).unwrap()[0].text,
            "second running"
        );
    }

    #[test]
    fn pause_resume_fail_and_archive_round_trip() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted =
            AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
                .unwrap()
                .create_run();
        save_auto_run(&conn, &mut persisted).unwrap();

        request_auto_run_pause(&conn, &mut persisted).unwrap();
        let loaded = load_auto_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("paused");
        assert_eq!(loaded.run.status, AutoRunStatus::Paused);

        resume_paused_auto_run(&conn, &mut persisted).unwrap();
        assert_eq!(persisted.run.status, AutoRunStatus::Queued);

        fail_auto_run(&conn, &mut persisted, "verification failed").unwrap();
        archive_auto_run(&conn, &mut persisted).unwrap();
        let loaded = load_auto_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("archived");
        assert_eq!(loaded.run.status, AutoRunStatus::Failed);
        assert!(loaded.run.archived_unix_ms.is_some());
    }

    #[test]
    fn stale_reconciliation_marks_active_steps_failed() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted =
            AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
                .unwrap()
                .create_run();
        persisted.run.status = AutoRunStatus::Running;
        persisted.steps[0].status = AutoStepStatus::Running;
        save_auto_run(&conn, &mut persisted).unwrap();

        let changed = reconcile_stale_auto_run(&conn, &mut persisted).unwrap();

        assert!(changed);
        let loaded = load_auto_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("run");
        assert_eq!(loaded.run.status, AutoRunStatus::Failed);
        assert_eq!(loaded.steps[0].status, AutoStepStatus::Failed);
        assert!(
            loaded.steps[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Prism restarted"))
        );
        let output = load_output_lines(&conn, loaded.steps[0].id.unwrap()).unwrap();
        assert!(output.iter().any(|line| {
            line.kind == AutoOutputKind::Error && line.text.contains("Prism restarted")
        }));
    }

    #[test]
    fn recent_active_runs_excludes_archived_and_done_runs() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");

        let mut active = AutoLaunch::new(&repo, &repo.join("feature-a"), "feat/a", "Implement a")
            .unwrap()
            .create_run();
        let mut done = AutoLaunch::new(&repo, &repo.join("feature-b"), "feat/b", "Implement b")
            .unwrap()
            .create_run();
        done.run.status = AutoRunStatus::Done;
        let mut archived = AutoLaunch::new(&repo, &repo.join("feature-c"), "feat/c", "Implement c")
            .unwrap()
            .create_run();
        archived.run.status = AutoRunStatus::Failed;
        save_auto_run(&conn, &mut active).unwrap();
        save_auto_run(&conn, &mut done).unwrap();
        save_auto_run(&conn, &mut archived).unwrap();
        archive_auto_run(&conn, &mut archived).unwrap();

        let recent = load_recent_active_runs_for_repo(&conn, &repo, 10).unwrap();

        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].run.id, active.run.id);
    }

    #[test]
    #[cfg(unix)]
    fn executor_runs_fake_opencode_and_persists_events() {
        let temp = TempDir::new("executor-success");
        let origin = temp.path().join("origin.git");
        let work = temp.path().join("work");
        setup_git_worktree(&origin, &work);
        let repo =
            Repository::with_config_dir_for_test(work.clone(), temp.path().join("prism-config"));
        let mut config = Config::load(&repo);
        config.default_base = None;
        seed_pr_cache(&repo, "feat/auto", "abc123");
        let opencode = temp.path().join("opencode");
        write_executable(
            &opencode,
            r#"#!/bin/sh
printf '%s\n' '{"type":"session","session_id":"ses_auto","title":"Auto Test"}'
printf '%s\n' '{"type":"message","text":"working on it"}'
printf '%s\n' '{"type":"tool.execute.before","id":"tool_1","name":"bash","command":"cargo test"}'
printf '%s\n' '{"type":"tool.execute.after","id":"tool_1","status":"success","output":"ok"}'
"#,
        );
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let mut persisted = AutoLaunch::new(&work, &work, "feat/auto", "Implement auto")
            .unwrap()
            .create_run();
        save_auto_run(&conn, &mut persisted).unwrap();
        let executor =
            AutoExecutorConfig::new(opencode.display().to_string(), None, work.clone(), "Auto");

        execute_auto_initial_step(
            &conn,
            &repo,
            &config,
            &mut persisted,
            &executor,
            &mut Vec::new(),
        )
        .unwrap();

        let loaded = load_auto_run(&conn, &persisted.run.id).unwrap().unwrap();
        assert_eq!(loaded.run.status, AutoRunStatus::Done);
        assert_eq!(loaded.run.pr_number, Some(42));
        assert_eq!(
            loaded.run.pr_url.as_deref(),
            Some("https://example.com/pr/42")
        );
        assert_eq!(loaded.steps[0].status, AutoStepStatus::Done);
        let implement = loaded
            .steps
            .iter()
            .find(|step| step.step_key == AutoStepKey::Implement)
            .unwrap();
        assert_eq!(implement.status, AutoStepStatus::Done);
        assert_eq!(implement.opencode_session_id.as_deref(), Some("ses_auto"));
        assert_eq!(implement.summary.as_deref(), Some("working on it"));
        let lines = load_output_lines(&conn, implement.id.unwrap()).unwrap();
        assert!(
            lines.iter().any(|line| {
                line.kind == AutoOutputKind::Tool && line.text.contains("cargo test")
            })
        );
        assert!(
            lines
                .iter()
                .any(|line| { line.kind == AutoOutputKind::ToolOutput && line.text == "ok" })
        );
    }

    #[test]
    #[cfg(unix)]
    fn executor_marks_failed_opencode_exit() {
        let temp = TempDir::new("executor-failed");
        let repo = Repository {
            root: temp.path().to_path_buf(),
        };
        let config = Config::load(&repo);
        let opencode = temp.path().join("opencode");
        write_executable(
            &opencode,
            r#"#!/bin/sh
printf '%s\n' '{"type":"message","text":"starting"}'
printf '%s\n' 'boom' >&2
exit 7
"#,
        );
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let mut persisted =
            AutoLaunch::new(temp.path(), temp.path(), "feat/auto", "Implement auto")
                .unwrap()
                .create_run();
        save_auto_run(&conn, &mut persisted).unwrap();
        let executor =
            AutoExecutorConfig::new(opencode.display().to_string(), None, temp.path(), "Auto");

        let error = execute_auto_initial_step(
            &conn,
            &repo,
            &config,
            &mut persisted,
            &executor,
            &mut Vec::new(),
        )
        .unwrap_err();

        assert!(error.contains("exited with 7"));
        let loaded = load_auto_run(&conn, &persisted.run.id).unwrap().unwrap();
        assert_eq!(loaded.run.status, AutoRunStatus::Failed);
        let implement = loaded
            .steps
            .iter()
            .find(|step| step.step_key == AutoStepKey::Implement)
            .unwrap();
        assert_eq!(implement.status, AutoStepStatus::Failed);
        assert!(
            implement
                .error
                .as_deref()
                .unwrap_or("")
                .contains("exited with 7")
        );
        let lines = load_output_lines(&conn, implement.id.unwrap()).unwrap();
        assert!(lines.iter().any(|line| line.text == "boom"));
    }

    #[test]
    fn output_retention_keeps_marker_and_recent_lines() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted =
            AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
                .unwrap()
                .create_run();
        save_auto_run(&conn, &mut persisted).unwrap();
        let step_id = persisted.steps[0].id.unwrap();

        for line_number in 1..=5 {
            append_output_line_limited(
                &conn,
                &AutoOutputLine {
                    step_run_id: step_id,
                    line_number,
                    time_unix_ms: line_number,
                    kind: AutoOutputKind::Assistant,
                    text: format!("line {line_number}"),
                    block_id: None,
                },
                3,
            )
            .unwrap();
        }

        let lines = load_output_lines(&conn, step_id).unwrap();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].text.contains("omitted"));
        assert_eq!(lines[1].text, "line 4");
        assert_eq!(lines[2].text, "line 5");
    }

    #[test]
    fn review_poll_detects_new_actionable_pr_comments() {
        let temp = TempDir::new("review-poll-actionable");
        let repo = Repository {
            root: temp.path().to_path_buf(),
        };
        let summary = test_pr_summary("feat/auto", "abc123", "2026-01-01T00:00:00Z");
        let config = Config::load(&repo);
        let details = crate::github::PrDetails {
            comments: vec![crate::github::PrComment {
                id: "comment-1".to_string(),
                author: "github-copilot".to_string(),
                body: "Please simplify this branch.".to_string(),
                created_at: "2026-01-01T00:01:00Z".to_string(),
            }],
            ..crate::github::PrDetails::default()
        };
        let mut persisted =
            AutoLaunch::new(temp.path(), temp.path(), "feat/auto", "Implement auto")
                .unwrap()
                .create_run();
        persisted.run.review_baseline_json = Some(
            serde_json::to_string(&ReviewBaseline {
                head_sha: "abc123".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .unwrap(),
        );

        let outcome =
            evaluate_review_feedback(&config, &mut persisted, &summary, Some(&details)).unwrap();

        assert!(outcome.fix_prompt.is_some());
        let prompt = outcome.fix_prompt.unwrap();
        assert!(prompt.contains("PR comments:"));
        assert!(prompt.contains("Please simplify this branch."));
        assert!(!outcome.complete);
    }

    #[test]
    fn review_poll_skips_feedback_at_or_before_baseline() {
        let temp = TempDir::new("review-poll-old");
        let repo = Repository {
            root: temp.path().to_path_buf(),
        };
        let summary = test_pr_summary("feat/auto", "abc123", "2026-01-01T00:05:00Z");
        let config = Config::load(&repo);
        let details = crate::github::PrDetails {
            comments: vec![crate::github::PrComment {
                id: "comment-1".to_string(),
                author: "github-copilot".to_string(),
                body: "Already handled.".to_string(),
                created_at: "2026-01-01T00:05:00Z".to_string(),
            }],
            ..crate::github::PrDetails::default()
        };
        let mut persisted =
            AutoLaunch::new(temp.path(), temp.path(), "feat/auto", "Implement auto")
                .unwrap()
                .create_run();
        persisted.run.review_baseline_json = Some(
            serde_json::to_string(&ReviewBaseline {
                head_sha: "abc123".to_string(),
                updated_at: "2026-01-01T00:05:00Z".to_string(),
            })
            .unwrap(),
        );

        let outcome =
            evaluate_review_feedback(&config, &mut persisted, &summary, Some(&details)).unwrap();

        assert!(outcome.fix_prompt.is_none());
        assert!(outcome.complete);
        assert!(outcome.summary.contains("no actionable review feedback"));
    }

    #[test]
    fn ci_status_waits_while_checks_are_pending() {
        let temp = TempDir::new("ci-pending");
        let repo = Repository {
            root: temp.path().to_path_buf(),
        };
        let config = Config::load(&repo);
        let mut summary = test_pr_summary("feat/auto", "abc123", "2026-01-01T00:00:00Z");
        summary.check_status = "running".to_string();

        let outcome = evaluate_ci_status(&config, "feat/auto", &summary, None).unwrap();

        assert_eq!(outcome.state, PrCheckState::Pending);
        assert!(outcome.summary.contains("still running"));
    }

    #[test]
    fn ci_status_builds_failure_prompt_with_logs() {
        let temp = TempDir::new("ci-failed");
        let repo = Repository {
            root: temp.path().to_path_buf(),
        };
        let config = Config::load(&repo);
        let mut summary = test_pr_summary("feat/auto", "abc123", "2026-01-01T00:00:00Z");
        summary.check_status = "failed".to_string();
        let details = PrDetails {
            failing_checks: vec!["test".to_string()],
            ci_failures: vec![crate::github::CiFailure {
                workflow: "CI".to_string(),
                name: "test".to_string(),
                conclusion: "failure".to_string(),
                url: "https://example.com/actions/runs/1".to_string(),
                run_id: "1".to_string(),
                log_tail: "assertion failed".to_string(),
            }],
            ..PrDetails::default()
        };

        let outcome = evaluate_ci_status(&config, "feat/auto", &summary, Some(&details)).unwrap();

        assert_eq!(outcome.state, PrCheckState::Failed);
        assert!(outcome.summary.contains("CI failed"));
        assert!(outcome.prompt.contains("Head SHA: abc123"));
        assert!(outcome.prompt.contains("assertion failed"));
    }

    #[test]
    fn review_completion_queues_ci_wait_before_done() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted =
            AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
                .unwrap()
                .create_run();
        persisted.steps[0].status = AutoStepStatus::Done;
        for (sequence, step_key, status) in [
            (2, AutoStepKey::Implement, AutoStepStatus::Done),
            (3, AutoStepKey::LocalVerify, AutoStepStatus::Done),
            (4, AutoStepKey::CommitImpl, AutoStepStatus::Done),
            (5, AutoStepKey::PushPr, AutoStepStatus::Done),
            (6, AutoStepKey::WaitReview, AutoStepStatus::Skipped),
        ] {
            persisted.steps.push(AutoStepRun {
                status,
                step_key,
                sequence,
                attempt: 1,
                run_id: persisted.run.id.clone(),
                id: None,
                reason: None,
                started_unix_ms: None,
                finished_unix_ms: None,
                opencode_server_url: None,
                opencode_session_id: None,
                process_id: None,
                plan_run_id: None,
                commit_sha: None,
                head_sha: None,
                summary: Some("done".to_string()),
                error: None,
            });
        }
        save_auto_run(&conn, &mut persisted).unwrap();

        assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());

        assert!(
            persisted
                .steps
                .iter()
                .any(|step| step.step_key == AutoStepKey::WaitCi)
        );
        assert_ne!(persisted.run.status, AutoRunStatus::Done);
    }

    #[test]
    fn ci_completion_queues_merge_before_done() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted =
            AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
                .unwrap()
                .create_run();
        persisted.steps.clear();
        push_test_step(&mut persisted, 1, AutoStepKey::WaitCi, AutoStepStatus::Done);
        save_auto_run(&conn, &mut persisted).unwrap();

        assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());

        assert!(
            persisted
                .steps
                .iter()
                .any(|step| step.step_key == AutoStepKey::Merge)
        );
        assert_ne!(persisted.run.status, AutoRunStatus::Done);
    }

    #[test]
    fn merge_success_queues_cleanup_separately() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted =
            AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
                .unwrap()
                .create_run();
        persisted.steps.clear();
        push_test_step(&mut persisted, 1, AutoStepKey::WaitCi, AutoStepStatus::Done);
        push_test_step(&mut persisted, 2, AutoStepKey::Merge, AutoStepStatus::Done);
        save_auto_run(&conn, &mut persisted).unwrap();

        assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());

        assert!(
            persisted
                .steps
                .iter()
                .any(|step| step.step_key == AutoStepKey::Cleanup)
        );
        assert_ne!(persisted.run.status, AutoRunStatus::Done);
    }

    #[test]
    fn manual_merge_skip_completes_run_without_cleanup() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted =
            AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
                .unwrap()
                .create_run();
        persisted.steps.clear();
        push_test_step(&mut persisted, 1, AutoStepKey::WaitCi, AutoStepStatus::Done);
        push_test_step(
            &mut persisted,
            2,
            AutoStepKey::Merge,
            AutoStepStatus::Skipped,
        );
        save_auto_run(&conn, &mut persisted).unwrap();

        assert!(!ensure_next_auto_step(&conn, &mut persisted).unwrap());

        assert_eq!(persisted.run.status, AutoRunStatus::Done);
        assert!(
            !persisted
                .steps
                .iter()
                .any(|step| step.step_key == AutoStepKey::Cleanup)
        );
    }

    #[test]
    fn merge_gate_blocks_dirty_draft_failed_ci_stale_head_and_review_feedback() {
        let temp = TempDir::new("merge-gate-blockers");
        let repo = Repository {
            root: temp.path().to_path_buf(),
        };
        let config = Config::load(&repo);
        let mut summary = test_pr_summary("feat/auto", "remote-head", "2026-01-01T00:00:00Z");
        summary.check_status = "failed".to_string();
        summary.draft = true;
        let details = PrDetails {
            comments: vec![crate::github::PrComment {
                id: "comment-1".to_string(),
                author: "github-copilot".to_string(),
                body: "Please fix this before merging.".to_string(),
                created_at: "2026-01-01T00:01:00Z".to_string(),
            }],
            ..PrDetails::default()
        };
        let mut persisted =
            AutoLaunch::new(temp.path(), temp.path(), "feat/auto", "Implement auto")
                .unwrap()
                .create_run();
        persisted.run.review_baseline_json = Some(
            serde_json::to_string(&ReviewBaseline {
                head_sha: "remote-head".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .unwrap(),
        );
        let verify = verify_result(false);

        let outcome = evaluate_merge_gate(
            &config,
            &persisted,
            &summary,
            Some(&details),
            "local-head",
            true,
            &verify,
        );

        assert!(!outcome.allowed);
        assert!(outcome.summary.contains("worktree is dirty"));
        assert!(outcome.summary.contains("PR is draft"));
        assert!(outcome.summary.contains("CI state is failed"));
        assert!(outcome.summary.contains("does not match local head"));
        assert!(
            outcome
                .summary
                .contains("actionable review feedback remains")
        );
        assert!(outcome.summary.contains("final local verification failed"));
    }

    #[cfg(unix)]
    #[test]
    fn merge_step_uses_gh_merge_and_waits_for_merged_status() {
        let temp = TempDir::new("merge-step-success");
        let origin = temp.path().join("origin.git");
        let work = temp.path().join("work");
        setup_git_worktree(&origin, &work);
        let repo =
            Repository::with_config_dir_for_test(work.clone(), temp.path().join("prism-config"));
        let mut config = Config::load(&repo);
        config.auto.merge = true;
        config.auto.review_wait_enabled = false;
        let gh_log = temp.path().join("gh.log");
        let head = crate::git::current_head_sha(&work, &config).unwrap();
        let gh = temp.path().join("gh");
        write_executable(
            &gh,
            &format!(
                r#"#!/bin/sh
printf 'args=%s\n' "$*" >> '{}'
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "feat/auto" ]; then
  cat <<'JSON'
{{"number":42,"title":"Auto","body":"","url":"https://example.com/pr/42","state":"OPEN","reviewDecision":"APPROVED","reviewRequests":[],"headRefName":"feat/auto","baseRefName":"main","headRefOid":"{}","updatedAt":"2026-01-01T00:00:00Z","statusCheckRollup":{{"contexts":{{"nodes":[{{"__typename":"StatusContext","context":"ci","state":"SUCCESS"}}]}}}},"mergedAt":null,"isDraft":false}}
JSON
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "42" ]; then
  printf '%s\n' '{{"state":"MERGED","mergedAt":"2026-01-01T00:02:00Z"}}'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "merge" ]; then
  exit 0
fi
exit 1
"#,
                gh_log.display(),
                head
            ),
        );
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let mut persisted = AutoLaunch::new(&work, &work, "feat/auto", "Implement auto")
            .unwrap()
            .create_run();
        persisted.steps.clear();
        persisted.steps.push(AutoStepRun::queued(
            &persisted.run.id,
            1,
            AutoStepKey::Merge,
            1,
            Some("merge".to_string()),
        ));
        save_auto_run(&conn, &mut persisted).unwrap();
        start_non_agent_step(&conn, &mut persisted, 0).unwrap();

        execute_merge_step(&conn, &repo, &config, &mut persisted, 0, 100).unwrap();

        let loaded = load_auto_run(&conn, &persisted.run.id).unwrap().unwrap();
        assert_eq!(loaded.steps[0].status, AutoStepStatus::Done);
        let commands = fs::read_to_string(gh_log).unwrap();
        assert!(commands.contains("args=pr merge 42 --squash"));
        assert!(commands.contains("args=pr view 42 --json state,mergedAt"));
    }

    fn push_test_step(
        persisted: &mut PersistedAutoRun,
        sequence: usize,
        step_key: AutoStepKey,
        status: AutoStepStatus,
    ) {
        persisted.steps.push(AutoStepRun {
            id: None,
            run_id: persisted.run.id.clone(),
            sequence,
            step_key,
            reason: None,
            status,
            attempt: 1,
            started_unix_ms: None,
            finished_unix_ms: None,
            opencode_server_url: None,
            opencode_session_id: None,
            process_id: None,
            plan_run_id: None,
            commit_sha: None,
            head_sha: None,
            summary: Some("done".to_string()),
            error: None,
        });
    }

    fn linked_run_plan_auto_run(conn: &rusqlite::Connection, repo: &Path) -> PersistedAutoRun {
        let mut persisted = AutoLaunch::with_options(
            repo,
            repo,
            AutoLaunchOptions {
                branch: "feat/auto".to_string(),
                mode: AutoRunMode::Standard,
                implementation_source: AutoImplementationSource::ExistingPlan,
                plan_path: Some(repo.join("plan.md")),
                plan_run_mode: PlanRunMode::Sequential,
                variant: "default".to_string(),
                agent_profile: None,
                initial_prompt: "Implement existing plan".to_string(),
            },
        )
        .unwrap()
        .create_run();
        let plan_launch = crate::plan_run::PlanLaunch::new(
            repo,
            repo,
            &repo.join("plan.md"),
            "phase",
            1,
            1,
            PlanRunMode::Sequential,
        )
        .unwrap();
        let plan_run = plan_launch.create_run();
        crate::plan_run::save_plan_run(conn, &plan_run).unwrap();
        persisted.steps.clear();
        persisted.steps.push(AutoStepRun::running(
            &persisted.run.id,
            1,
            AutoStepKey::RunPlan,
            1,
        ));
        persisted.steps[0].plan_run_id = Some(plan_run.run.id);
        persisted.run.status = AutoRunStatus::Running;
        save_auto_run(conn, &mut persisted).unwrap();
        persisted
    }

    fn verify_result(passed: bool) -> VerifyResult {
        VerifyResult {
            passed,
            checks: vec![crate::verify::VerifyCheckResult {
                kind: crate::verify::VerifyCheckKind::Configured,
                label: "test".to_string(),
                passed,
                message: "test".to_string(),
            }],
        }
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, text: &str) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, text).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    fn setup_git_worktree(origin: &Path, work: &Path) {
        run(Command::new("git").args(["init", "--bare"]).arg(origin));
        run(Command::new("git").arg("--git-dir").arg(origin).args([
            "symbolic-ref",
            "HEAD",
            "refs/heads/main",
        ]));
        run(Command::new("git").arg("clone").arg(origin).arg(work));
        run_git(work, &["config", "user.email", "test@example.com"]);
        run_git(work, &["config", "user.name", "Test User"]);
        fs::write(work.join("tracked.txt"), "base\n").unwrap();
        run_git(work, &["add", "tracked.txt"]);
        run_git(work, &["commit", "-m", "initial"]);
        run_git(work, &["push", "-u", "origin", "main"]);
        run_git(work, &["switch", "-c", "feat/auto"]);
    }

    #[cfg(unix)]
    fn seed_pr_cache(repo: &Repository, branch: &str, head_sha: &str) {
        let cache = crate::github::PrCache {
            summary: Some(crate::github::PrSummary {
                number: 42,
                title: "Auto".to_string(),
                body: String::new(),
                url: "https://example.com/pr/42".to_string(),
                state: "OPEN".to_string(),
                review_decision: String::new(),
                requested_reviewers: Vec::new(),
                head_ref: branch.to_string(),
                base_ref: "main".to_string(),
                head_sha: head_sha.to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                check_status: "passed".to_string(),
                comment_count: 0,
                merged: false,
                draft: false,
            }),
            ..crate::github::PrCache::default()
        };
        crate::github::save_pr_cache(repo, branch, &cache).unwrap();
    }

    fn test_pr_summary(branch: &str, head_sha: &str, updated_at: &str) -> crate::github::PrSummary {
        crate::github::PrSummary {
            number: 42,
            title: "Auto".to_string(),
            body: String::new(),
            url: "https://example.com/pr/42".to_string(),
            state: "OPEN".to_string(),
            review_decision: String::new(),
            requested_reviewers: vec!["github-copilot".to_string()],
            head_ref: branch.to_string(),
            base_ref: "main".to_string(),
            head_sha: head_sha.to_string(),
            updated_at: updated_at.to_string(),
            check_status: "unknown".to_string(),
            comment_count: 1,
            merged: false,
            draft: false,
        }
    }

    #[cfg(unix)]
    fn run_git(path: &Path, args: &[&str]) {
        run(Command::new("git").arg("-C").arg(path).args(args));
    }

    #[cfg(unix)]
    fn run(command: &mut Command) {
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "command failed: {:?}\nstdout: {}\nstderr: {}",
            command,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
