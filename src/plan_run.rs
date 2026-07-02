use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::opencode::{OpencodeState, OpencodeStatus};
use crate::plan::{build_task, display_plan_path};
use crate::repo::Repository;
use crate::util::stable_hash;

pub const DEFAULT_OUTPUT_LINES_PER_STEP: usize = 2_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanRun {
    pub id: String,
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
    pub opencode_state: Option<OpencodeState>,
    pub opencode_server_url: Option<String>,
    pub opencode_session_id: Option<String>,
    pub agent_variant: Option<String>,
    pub process_id: Option<u32>,
    pub started_unix_ms: Option<u64>,
    pub finished_unix_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub latest_message: Option<String>,
    pub active_tool: Option<String>,
    pub todos: Vec<PlanTodo>,
    pub summary: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanTodo {
    pub title: String,
    pub status: String,
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordedProcessState {
    Missing,
    Live(u32),
    Dead(u32),
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
pub struct PlanLaunch {
    pub repo_root: String,
    pub scope_path: PathBuf,
    pub plan_path: PathBuf,
    pub plan_display: String,
    pub step_name: String,
    pub start_step: usize,
    pub total_steps: usize,
    pub mode: PlanRunMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedPlanRun {
    pub run: PlanRun,
    pub steps: Vec<PlanStepRun>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanExecutorConfig {
    pub opencode_program: String,
    pub server_url: Option<String>,
    pub scope_path: PathBuf,
    pub title_prefix: String,
    pub max_output_lines_per_step: usize,
    pub plugin_config_dir: Option<PathBuf>,
    pub plugin_event_log_path: Option<PathBuf>,
    pub agent_variant: Option<String>,
}

pub const DEFAULT_PLAN_AGENT_VARIANT: &str = "medium";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanPluginConfig {
    pub config_dir: PathBuf,
    pub plugin_path: PathBuf,
    pub event_log_path: PathBuf,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanAgentEvent {
    SessionIdentified {
        session_id: String,
        title: Option<String>,
    },
    StateChanged {
        state: String,
    },
    AssistantText {
        text: String,
    },
    ToolStarted {
        id: Option<String>,
        name: String,
        args_summary: Option<String>,
    },
    ToolOutput {
        id: Option<String>,
        text: String,
    },
    ToolFinished {
        id: Option<String>,
        status: String,
    },
    TodoUpdated {
        todos: Vec<PlanTodo>,
    },
    DiffUpdated {
        summary: String,
        patch: Option<String>,
    },
    Error {
        message: String,
    },
    Raw {
        event_type: String,
        json: String,
    },
}

impl PlanRunMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Parallel => "parallel",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "sequential" => Ok(Self::Sequential),
            "parallel" => Ok(Self::Parallel),
            _ => Err(format!("unknown plan run mode: {value}")),
        }
    }
}

impl PlanRunStatus {
    fn as_str(self) -> &'static str {
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

    fn parse(value: &str) -> Result<Self, String> {
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
    fn as_str(self) -> &'static str {
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

    fn parse(value: &str) -> Result<Self, String> {
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
    fn as_str(self) -> &'static str {
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

    fn parse(value: &str) -> Result<Self, String> {
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

impl PlanTodo {
    pub fn new(title: impl Into<String>, status: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            status: status.into(),
        }
    }
}

impl PlanLaunch {
    pub fn new(
        repo_root: &Path,
        scope_path: &Path,
        plan_path: &Path,
        step_name: impl Into<String>,
        start_step: usize,
        total_steps: usize,
        mode: PlanRunMode,
    ) -> Result<Self, String> {
        if start_step == 0 {
            return Err("start step must be greater than zero".to_string());
        }
        if total_steps == 0 {
            return Err("total steps must be greater than zero".to_string());
        }
        if start_step > total_steps {
            return Err("start step cannot be greater than total steps".to_string());
        }
        Ok(Self {
            repo_root: repo_root.display().to_string(),
            scope_path: scope_path.to_path_buf(),
            plan_path: plan_path.to_path_buf(),
            plan_display: display_plan_path(scope_path, plan_path),
            step_name: step_name.into(),
            start_step,
            total_steps,
            mode,
        })
    }

    pub fn create_run(&self) -> PersistedPlanRun {
        let now = unix_ms();
        let id = self.default_run_id(now);
        let run = PlanRun {
            id: id.clone(),
            repo_root: self.repo_root.clone(),
            scope_path: self.scope_path.clone(),
            plan_path: self.plan_path.clone(),
            plan_display: self.plan_display.clone(),
            step_name: self.step_name.clone(),
            start_step: self.start_step,
            total_steps: self.total_steps,
            mode: self.mode,
            status: PlanRunStatus::Queued,
            pause_requested: false,
            selected_step: self.start_step,
            created_unix_ms: now,
            updated_unix_ms: now,
            archived_unix_ms: None,
        };
        let steps = (self.start_step..=self.total_steps)
            .map(|step| {
                PlanStepRun::queued(
                    &id,
                    step,
                    build_task(&self.plan_display, &self.step_name, step),
                )
            })
            .collect();
        PersistedPlanRun { run, steps }
    }

    fn default_run_id(&self, now: u64) -> String {
        format!(
            "plan-{:016x}-{}",
            stable_hash(&self.scope_path) ^ stable_hash(&self.plan_path),
            now
        )
    }
}

impl PlanStepRun {
    pub fn queued(run_id: &str, step: usize, prompt: String) -> Self {
        Self {
            run_id: run_id.to_string(),
            step,
            prompt,
            status: PlanStepStatus::Queued,
            opencode_state: None,
            opencode_server_url: None,
            opencode_session_id: None,
            agent_variant: None,
            process_id: None,
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

impl PlanExecutorConfig {
    pub fn new(
        opencode_program: impl Into<String>,
        server_url: Option<String>,
        scope_path: impl Into<PathBuf>,
        title_prefix: impl Into<String>,
    ) -> Self {
        Self {
            opencode_program: opencode_program.into(),
            server_url,
            scope_path: scope_path.into(),
            title_prefix: title_prefix.into(),
            max_output_lines_per_step: DEFAULT_OUTPUT_LINES_PER_STEP,
            plugin_config_dir: None,
            plugin_event_log_path: None,
            agent_variant: Some(DEFAULT_PLAN_AGENT_VARIANT.to_string()),
        }
    }

    pub fn with_plugin_config(mut self, plugin: PlanPluginConfig) -> Self {
        self.plugin_config_dir = Some(plugin.config_dir);
        self.plugin_event_log_path = Some(plugin.event_log_path);
        self
    }
}

pub fn prepare_plan_plugin_config(repo_prism_dir: &Path) -> Result<PlanPluginConfig, String> {
    let config_dir = repo_prism_dir.join("opencode-plan-plugin");
    let plugin_path = config_dir.join("prism-plan-plugin.js");
    let event_log_path = config_dir.join("events.jsonl");
    fs::create_dir_all(&config_dir)
        .map_err(|error| format!("create OpenCode plan plugin directory: {error}"))?;
    fs::write(
        config_dir.join("opencode.json"),
        opencode_plan_plugin_config_json(),
    )
    .map_err(|error| format!("write OpenCode plan plugin config: {error}"))?;
    fs::write(&plugin_path, opencode_plan_plugin_js())
        .map_err(|error| format!("write OpenCode plan plugin: {error}"))?;
    Ok(PlanPluginConfig {
        config_dir,
        plugin_path,
        event_log_path,
    })
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

pub fn migrate_schema(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        create table if not exists plan_run (
          id text primary key,
          repo_root text not null,
          scope_path text not null,
          plan_path text not null,
          plan_display text not null,
          step_name text not null,
          start_step integer not null,
          total_steps integer not null,
          mode text not null,
          status text not null,
          pause_requested integer not null default 0,
          selected_step integer not null,
          created_unix_ms integer not null,
          updated_unix_ms integer not null,
          archived_unix_ms integer
        );

        create table if not exists plan_step_run (
          run_id text not null references plan_run(id) on delete cascade,
          step integer not null,
          prompt text not null,
          status text not null,
          opencode_state text,
          opencode_server_url text,
          opencode_session_id text,
          agent_variant text,
          process_id integer,
          started_unix_ms integer,
          finished_unix_ms integer,
          exit_code integer,
          latest_message text,
          active_tool text,
          todos_json text not null default '[]',
          summary text,
          error text,
          primary key (run_id, step)
        );

        create table if not exists plan_output_line (
          run_id text not null,
          step integer not null,
          line_number integer not null,
          time_unix_ms integer not null,
          kind text not null,
          text text not null,
          block_id text,
          primary key (run_id, step, line_number),
          foreign key (run_id, step) references plan_step_run(run_id, step) on delete cascade
        );

        create index if not exists plan_run_repo_idx
          on plan_run(repo_root, updated_unix_ms);
        create index if not exists plan_run_scope_idx
          on plan_run(scope_path, updated_unix_ms);
        create index if not exists plan_run_status_idx
          on plan_run(status, updated_unix_ms);
        create index if not exists plan_output_line_step_idx
          on plan_output_line(run_id, step, line_number);
        ",
    )
    .map_err(|error| format!("create plan run schema: {error}"))?;
    add_column_if_missing(
        conn,
        "plan_run",
        "archived_unix_ms",
        "alter table plan_run add column archived_unix_ms integer",
    )?;
    add_column_if_missing(
        conn,
        "plan_run",
        "pause_requested",
        "alter table plan_run add column pause_requested integer not null default 0",
    )?;
    add_column_if_missing(
        conn,
        "plan_step_run",
        "opencode_state",
        "alter table plan_step_run add column opencode_state text",
    )?;
    add_column_if_missing(
        conn,
        "plan_step_run",
        "agent_variant",
        "alter table plan_step_run add column agent_variant text",
    )?;
    Ok(())
}

pub fn save_plan_run(
    conn: &rusqlite::Connection,
    persisted: &PersistedPlanRun,
) -> Result<(), String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| format!("begin plan run transaction: {error}"))?;
    save_run_with_conn(&tx, &persisted.run)?;
    for step in &persisted.steps {
        save_step_with_conn(&tx, step)?;
    }
    tx.commit()
        .map_err(|error| format!("commit plan run transaction: {error}"))?;
    Ok(())
}

pub fn load_plan_run(
    conn: &rusqlite::Connection,
    run_id: &str,
) -> Result<Option<PersistedPlanRun>, String> {
    let run = load_run_with_conn(conn, run_id)?;
    let Some(run) = run else {
        return Ok(None);
    };
    let steps = load_steps_with_conn(conn, run_id)?;
    Ok(Some(PersistedPlanRun { run, steps }))
}

pub fn load_recent_plan_runs_for_repo(
    conn: &rusqlite::Connection,
    repo_root: &Path,
    limit: usize,
) -> Result<Vec<PersistedPlanRun>, String> {
    let mut statement = conn
        .prepare(
            "select id
             from plan_run
             where repo_root = ?1
               and archived_unix_ms is null
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
        .map_err(|error| format!("prepare recent plan run load: {error}"))?;
    let ids = statement
        .query_map(
            params![repo_root.display().to_string(), usize_to_i64(limit)],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| format!("load recent plan run ids: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read recent plan run ids: {error}"))?;
    ids.into_iter()
        .filter_map(|id| load_plan_run(conn, &id).transpose())
        .collect()
}

pub fn load_resumable_plan_run(
    conn: &rusqlite::Connection,
    launch: &PlanLaunch,
) -> Result<Option<PersistedPlanRun>, String> {
    let run_id = conn
        .query_row(
            "select id
             from plan_run
             where repo_root = ?1
               and scope_path = ?2
               and plan_path = ?3
               and step_name = ?4
               and start_step = ?5
               and total_steps = ?6
               and mode = ?7
               and archived_unix_ms is null
               and status in ('queued', 'running', 'paused')
             order by updated_unix_ms desc
             limit 1",
            params![
                launch.repo_root.as_str(),
                launch.scope_path.display().to_string(),
                launch.plan_path.display().to_string(),
                launch.step_name.as_str(),
                usize_to_i64(launch.start_step),
                usize_to_i64(launch.total_steps),
                launch.mode.as_str(),
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| format!("load resumable plan run id: {error}"))?;
    run_id
        .map(|run_id| load_plan_run(conn, &run_id))
        .transpose()
        .map(Option::flatten)
}

pub fn prepare_plan_run_for_resume(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
    max_output_lines_per_step: usize,
) -> Result<bool, String> {
    let mut changed = false;
    let mut has_live_child = false;
    for step in &mut persisted.steps {
        if !matches!(
            step.status,
            PlanStepStatus::Starting | PlanStepStatus::Running
        ) {
            continue;
        }
        if let Some(process_id) = step.process_id
            && process_is_running(process_id)
        {
            has_live_child = true;
            continue;
        }
        let message = format!(
            "phase {} was interrupted before completion and was queued for resume",
            step.step
        );
        reset_step_for_retry(step);
        append_system_output(
            conn,
            step,
            PlanOutputKind::System,
            &message,
            max_output_lines_per_step,
        )?;
        save_step_with_conn(conn, step)?;
        changed = true;
    }
    if has_live_child {
        return Ok(false);
    }
    if persisted.run.pause_requested || persisted.run.status == PlanRunStatus::Paused || changed {
        persisted.run.pause_requested = false;
        persisted.run.status = persisted.aggregate_status();
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
    }
    Ok(true)
}

pub fn append_output_line(
    conn: &rusqlite::Connection,
    line: &PlanOutputLine,
    max_lines_per_step: usize,
) -> Result<(), String> {
    conn.execute(
        "insert or replace into plan_output_line (
           run_id, step, line_number, time_unix_ms, kind, text, block_id
         ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            line.run_id.as_str(),
            usize_to_i64(line.step),
            u64_to_i64(line.line_number),
            u64_to_i64(line.time_unix_ms),
            line.kind.as_str(),
            line.text.as_str(),
            line.block_id.as_deref(),
        ],
    )
    .map_err(|error| format!("write plan output line: {error}"))?;
    trim_output_lines(conn, &line.run_id, line.step, max_lines_per_step)
}

pub fn save_plan_step(conn: &rusqlite::Connection, step: &PlanStepRun) -> Result<(), String> {
    save_step_with_conn(conn, step)
}

pub fn execute_plan_sequential(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
    executor: &PlanExecutorConfig,
    output: &mut dyn Write,
) -> Result<(), String> {
    persisted.run.status = PlanRunStatus::Running;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)?;

    let mut failure: Option<String> = None;
    let mut paused = false;
    for index in 0..persisted.steps.len() {
        if failure.is_some() {
            break;
        }
        if persisted.steps[index].status != PlanStepStatus::Queued {
            continue;
        }
        if reload_pause_request(conn, persisted)? {
            paused = true;
            break;
        }
        let result = execute_one_step(conn, persisted, index, executor, output);
        if let Err(error) = result {
            failure = Some(error);
        }
    }

    if paused {
        persisted.run.status = PlanRunStatus::Paused;
    } else {
        persisted.run.status = persisted.aggregate_status();
    }
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)?;

    if let Some(error) = failure {
        Err(error)
    } else {
        Ok(())
    }
}

pub fn execute_plan_parallel(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
    executor: &PlanExecutorConfig,
    output: &mut dyn Write,
) -> Result<(), String> {
    persisted.run.status = PlanRunStatus::Running;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)?;

    let (tx, rx) = mpsc::channel::<Result<ParallelChildEvent, String>>();
    let mut running = 0usize;
    let mut spawn_errors = Vec::new();

    for index in 0..persisted.steps.len() {
        if persisted.steps[index].status != PlanStepStatus::Queued {
            continue;
        }
        let step_number = persisted.steps[index].step;
        let prompt = persisted.steps[index].prompt.clone();
        {
            let step = &mut persisted.steps[index];
            step.status = PlanStepStatus::Starting;
            step.started_unix_ms = Some(unix_ms());
            step.opencode_server_url = executor.server_url.clone();
            step.agent_variant = executor.agent_variant.clone();
            step.error = None;
            save_step_with_conn(conn, step)?;
        }
        persisted.run.selected_step = step_number;
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        writeln!(output, "\n==> {prompt}\n")
            .map_err(|error| format!("write plan output: {error}"))?;

        let mut command = opencode_run_command(executor, step_number, &prompt, true);
        let spawn_result = spawn_opencode(&mut command);
        let (child, used_attach) = match spawn_result {
            Ok(child) => (child, true),
            Err(error) if executor.server_url.is_some() => {
                append_system_output(
                    conn,
                    &persisted.steps[index],
                    PlanOutputKind::Error,
                    &format!("attach launch failed, retrying without --attach: {error}"),
                    executor.max_output_lines_per_step,
                )?;
                let mut fallback = opencode_run_command(executor, step_number, &prompt, false);
                match spawn_opencode(&mut fallback) {
                    Ok(child) => (child, false),
                    Err(error) => {
                        mark_spawn_failure(
                            conn,
                            &mut persisted.steps[index],
                            &error,
                            executor.max_output_lines_per_step,
                        )?;
                        spawn_errors.push(error);
                        continue;
                    }
                }
            }
            Err(error) => {
                mark_spawn_failure(
                    conn,
                    &mut persisted.steps[index],
                    &error,
                    executor.max_output_lines_per_step,
                )?;
                spawn_errors.push(error);
                continue;
            }
        };

        persisted.steps[index].status = PlanStepStatus::Running;
        persisted.steps[index].process_id = Some(child.id());
        identify_attached_plan_session(executor, &mut persisted.steps[index]);
        save_step_with_conn(conn, &persisted.steps[index])?;
        spawn_parallel_child(index, child, used_attach, tx.clone())?;
        running += 1;
    }
    drop(tx);

    let mut finished = 0usize;
    while finished < running {
        match rx.recv() {
            Ok(Ok(ParallelChildEvent::Line {
                step_index,
                stream,
                text,
            })) => {
                if let Some(step) = persisted.steps.get_mut(step_index) {
                    ingest_child_line(
                        conn,
                        step,
                        stream,
                        &text,
                        executor.max_output_lines_per_step,
                        output,
                    )?;
                }
            }
            Ok(Ok(ParallelChildEvent::Exit {
                step_index,
                exit_code,
                used_attach,
            })) => {
                if let Some(step) = persisted.steps.get_mut(step_index) {
                    finish_step_after_exit(conn, step, exit_code, used_attach)?;
                    persisted.run.selected_step = step.step;
                    persisted.run.status = persisted.aggregate_status();
                    persisted.run.updated_unix_ms = unix_ms();
                    save_run_with_conn(conn, &persisted.run)?;
                }
                finished += 1;
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => break,
        }
    }

    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)?;

    if persisted
        .steps
        .iter()
        .any(|step| step.status == PlanStepStatus::Failed)
    {
        let failures = persisted
            .steps
            .iter()
            .filter(|step| step.status == PlanStepStatus::Failed)
            .map(|step| {
                format!(
                    "step {}: {}",
                    step.step,
                    step.error.as_deref().unwrap_or("failed")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        Err(format!("parallel plan failed: {failures}"))
    } else if !spawn_errors.is_empty() {
        Err(format!("parallel plan failed: {}", spawn_errors.join("; ")))
    } else {
        Ok(())
    }
}

pub fn abort_plan_step(conn: &rusqlite::Connection, step: &mut PlanStepRun) -> Result<(), String> {
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
    step.status = PlanStepStatus::Aborted;
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

pub fn abort_plan_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
) -> Result<(), String> {
    let mut errors = Vec::new();
    for step in &mut persisted.steps {
        if matches!(
            step.status,
            PlanStepStatus::Starting | PlanStepStatus::Running
        ) && let Err(error) = abort_plan_step(conn, step)
        {
            errors.push(format!("step {}: {error}", step.step));
        }
    }
    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)?;
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub fn reconcile_stale_plan_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
    max_output_lines_per_step: usize,
) -> Result<bool, String> {
    let mut changed = false;
    let mut changed_run_status = false;
    let repo_root = persisted.run.repo_root.clone();
    let run_id = persisted.run.id.clone();
    for step in &mut persisted.steps {
        if !matches!(
            step.status,
            PlanStepStatus::Starting | PlanStepStatus::Running
        ) {
            continue;
        }
        match recorded_process_state(step.process_id) {
            RecordedProcessState::Live(process_id) => {
                if reconcile_plan_step_from_server(conn, step, max_output_lines_per_step)
                    .unwrap_or(false)
                {
                    changed = true;
                }
                let message = format!(
                    "Prism restarted while phase {} was running in process {process_id}; stdout cannot be reattached, so Prism is showing persisted state until new OpenCode status is available.",
                    step.step
                );
                if append_unique_system_output(
                    conn,
                    step,
                    PlanOutputKind::System,
                    &message,
                    max_output_lines_per_step,
                )? {
                    changed = true;
                }
                append_stale_reconciliation_log(
                    &repo_root,
                    &run_id,
                    step,
                    "kept-running-live-process",
                );
            }
            RecordedProcessState::Dead(process_id) => {
                let message = format!(
                    "Prism restarted while phase {} was running, and recorded process {process_id} is no longer running.",
                    step.step
                );
                mark_stale_step_failed(conn, step, &message, max_output_lines_per_step)?;
                changed = true;
                changed_run_status = true;
                append_stale_reconciliation_log(&repo_root, &run_id, step, "failed-dead-process");
            }
            RecordedProcessState::Missing => {
                let message = format!(
                    "Prism restarted while phase {} was marked running, but no child process id was recorded.",
                    step.step
                );
                mark_stale_step_failed(conn, step, &message, max_output_lines_per_step)?;
                changed = true;
                changed_run_status = true;
                append_stale_reconciliation_log(
                    &repo_root,
                    &run_id,
                    step,
                    "failed-missing-process",
                );
            }
        }
    }
    if changed_run_status
        && matches!(
            persisted.run.status,
            PlanRunStatus::Queued | PlanRunStatus::Running | PlanRunStatus::Paused
        )
    {
        persisted.run.status = persisted.aggregate_status();
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        changed = true;
    }
    Ok(changed)
}

fn mark_stale_step_failed(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
    message: &str,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    step.status = PlanStepStatus::Failed;
    step.process_id = None;
    step.finished_unix_ms = Some(unix_ms());
    step.error = Some(message.to_string());
    append_unique_system_output(
        conn,
        step,
        PlanOutputKind::Error,
        message,
        max_output_lines_per_step,
    )?;
    save_step_with_conn(conn, step)
}

fn recorded_process_state(process_id: Option<u32>) -> RecordedProcessState {
    match process_id {
        Some(process_id) if process_is_running(process_id) => {
            RecordedProcessState::Live(process_id)
        }
        Some(process_id) => RecordedProcessState::Dead(process_id),
        None => RecordedProcessState::Missing,
    }
}

fn append_unique_system_output(
    conn: &rusqlite::Connection,
    step: &PlanStepRun,
    kind: PlanOutputKind,
    text: &str,
    max_output_lines_per_step: usize,
) -> Result<bool, String> {
    if output_line_exists(conn, &step.run_id, step.step, kind, text)? {
        return Ok(false);
    }
    append_system_output(conn, step, kind, text, max_output_lines_per_step)?;
    Ok(true)
}

fn output_line_exists(
    conn: &rusqlite::Connection,
    run_id: &str,
    step: usize,
    kind: PlanOutputKind,
    text: &str,
) -> Result<bool, String> {
    let exists: i64 = conn
        .query_row(
            "select exists(
               select 1 from plan_output_line
               where run_id = ?1 and step = ?2 and kind = ?3 and text = ?4
             )",
            params![run_id, usize_to_i64(step), kind.as_str(), text],
            |row| row.get(0),
        )
        .map_err(|error| format!("check plan output line existence: {error}"))?;
    Ok(exists != 0)
}

fn append_stale_reconciliation_log(
    repo_root: &str,
    run_id: &str,
    step: &PlanStepRun,
    transition: &str,
) {
    let repo = Repository {
        root: PathBuf::from(repo_root),
    };
    let server_url = step.opencode_server_url.as_deref().unwrap_or("none");
    let session_id = step.opencode_session_id.as_deref().unwrap_or("none");
    let process_id = step
        .process_id
        .map(|process_id| process_id.to_string())
        .unwrap_or_else(|| "none".to_string());
    let _ = crate::observability::append_runtime_message(
        &repo,
        &format!(
            "plan stale reconciliation run_id={run_id} step={} process_id={process_id} server_url={server_url} session_id={session_id} transition={transition}",
            step.step
        ),
    );
}

pub fn retry_failed_steps(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
) -> Result<(), String> {
    let mut first = None;
    for step in &mut persisted.steps {
        if matches!(
            step.status,
            PlanStepStatus::Failed | PlanStepStatus::Aborted
        ) {
            reset_step_for_retry(step);
            first.get_or_insert(step.step);
            save_step_with_conn(conn, step)?;
        }
    }
    if first.is_none() {
        return Err("plan run has no failed phases to retry".to_string());
    }
    persisted.run.selected_step = first.unwrap_or(persisted.run.selected_step);
    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub fn retry_from_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
    selected_step: usize,
) -> Result<(), String> {
    let mut found = false;
    for step in &mut persisted.steps {
        if step.step < selected_step {
            continue;
        }
        found = true;
        reset_step_for_retry(step);
        save_step_with_conn(conn, step)?;
    }
    if !found {
        return Err(format!("plan phase not found: {selected_step}"));
    }
    persisted.run.selected_step = selected_step;
    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub fn skip_plan_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
    selected_step: usize,
) -> Result<(), String> {
    let step = persisted
        .steps
        .iter_mut()
        .find(|step| step.step == selected_step)
        .ok_or_else(|| format!("plan phase not found: {selected_step}"))?;
    if matches!(
        step.status,
        PlanStepStatus::Starting | PlanStepStatus::Running
    ) {
        return Err(format!("plan phase {selected_step} is running"));
    }
    step.status = PlanStepStatus::Skipped;
    step.process_id = None;
    step.finished_unix_ms = Some(unix_ms());
    step.error = None;
    step.active_tool = None;
    append_system_output(
        conn,
        step,
        PlanOutputKind::System,
        "phase skipped",
        DEFAULT_OUTPUT_LINES_PER_STEP,
    )?;
    save_step_with_conn(conn, step)?;
    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub fn request_plan_run_pause(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
) -> Result<(), String> {
    if matches!(
        persisted.run.status,
        PlanRunStatus::Done | PlanRunStatus::Failed | PlanRunStatus::Aborted
    ) {
        return Err("cannot pause a completed plan run".to_string());
    }
    persisted.run.pause_requested = true;
    if !persisted.steps.iter().any(|step| {
        matches!(
            step.status,
            PlanStepStatus::Starting | PlanStepStatus::Running
        )
    }) {
        persisted.run.status = PlanRunStatus::Paused;
    }
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub fn resume_paused_plan_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
) -> Result<(), String> {
    if !persisted.run.pause_requested && persisted.run.status != PlanRunStatus::Paused {
        return Err("plan run is not paused".to_string());
    }
    persisted.run.pause_requested = false;
    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub fn archive_plan_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
) -> Result<(), String> {
    if matches!(
        persisted.run.status,
        PlanRunStatus::Queued | PlanRunStatus::Running | PlanRunStatus::Paused
    ) {
        return Err("cannot dismiss a queued or running plan run".to_string());
    }
    let now = unix_ms();
    persisted.run.archived_unix_ms = Some(now);
    persisted.run.updated_unix_ms = now;
    save_run_with_conn(conn, &persisted.run)
}

pub fn cleanup_stale_archived_plan_runs(
    conn: &rusqlite::Connection,
    retention_ms: u64,
) -> Result<usize, String> {
    let cutoff = unix_ms().saturating_sub(retention_ms);
    conn.execute(
        "delete from plan_run
         where archived_unix_ms is not null and archived_unix_ms <= ?1",
        params![u64_to_i64(cutoff)],
    )
    .map_err(|error| format!("cleanup archived plan runs: {error}"))
}

pub fn parse_plan_agent_events(raw: &str) -> Vec<PlanAgentEvent> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return vec![PlanAgentEvent::AssistantText {
            text: trimmed.to_string(),
        }];
    };
    let event_type = string_field_deep(&value, &["type", "event", "name"])
        .unwrap_or_else(|| "event".to_string());
    let lower_type = event_type.to_ascii_lowercase();
    let mut events = Vec::new();

    if lower_type == "server.connected" {
        events.push(PlanAgentEvent::StateChanged {
            state: "connected".to_string(),
        });
    }

    if let Some(session_id) = string_field_deep(&value, &["session_id", "sessionID", "sessionId"])
        .or_else(|| nested_string(&value, &["session", "id"]))
    {
        events.push(PlanAgentEvent::SessionIdentified {
            session_id,
            title: string_field_deep(&value, &["title"]),
        });
    }

    if let Some(state) = string_field_deep(&value, &["status", "state"])
        && (lower_type.contains("status") || lower_type.contains("state"))
    {
        events.push(PlanAgentEvent::StateChanged { state });
    } else if lower_type == "session.idle" {
        events.push(PlanAgentEvent::StateChanged {
            state: "idle".to_string(),
        });
    } else if lower_type == "session.error" {
        events.push(PlanAgentEvent::StateChanged {
            state: "error".to_string(),
        });
    }

    if let Some(todos) = todos_from_value(&value)
        && !todos.is_empty()
    {
        events.push(PlanAgentEvent::TodoUpdated { todos });
    }

    if lower_type.contains("error")
        && let Some(message) = string_field_deep(&value, &["error", "message"])
    {
        events.push(PlanAgentEvent::Error { message });
    }

    if (lower_type.contains("diff") || string_field_deep(&value, &["patch", "path"]).is_some())
        && let Some(summary) =
            string_field_deep(&value, &["summary", "path"]).or_else(|| Some("diff updated".into()))
    {
        events.push(PlanAgentEvent::DiffUpdated {
            summary,
            patch: string_field_deep(&value, &["patch"]),
        });
    }

    if lower_type.contains("tool") {
        let id = string_field_deep(&value, &["id", "tool_call_id", "call_id"]);
        let name = string_field_deep(&value, &["tool", "name"]).unwrap_or_else(|| "tool".into());
        let status = string_field_deep(&value, &["status", "state"]);
        let args_summary = string_field_deep(&value, &["command", "description", "input"]);
        let output = string_field_deep(&value, &["output", "stdout", "stderr"]);
        if let Some(text) = output {
            events.push(PlanAgentEvent::ToolOutput {
                id: id.clone(),
                text,
            });
        } else if lower_type.contains(".after")
            || lower_type.contains("after")
            || matches!(
                status.as_deref(),
                Some("done" | "failed" | "error" | "completed" | "complete" | "success")
            )
        {
            events.push(PlanAgentEvent::ToolFinished {
                id: id.clone(),
                status: status.unwrap_or_else(|| "done".into()),
            });
        } else {
            events.push(PlanAgentEvent::ToolStarted {
                id: id.clone(),
                name,
                args_summary,
            });
        }
    }

    if !lower_type.contains("tool")
        && !lower_type.contains("status")
        && !lower_type.contains("diff")
        && !lower_type.contains("todo")
        && let Some(text) = string_field_deep(&value, &["text", "content", "message", "summary"])
    {
        events.push(PlanAgentEvent::AssistantText { text });
    }

    if events.is_empty() || should_keep_raw(&event_type, &value) {
        events.push(PlanAgentEvent::Raw {
            event_type,
            json: trimmed.to_string(),
        });
    }
    events
}

pub fn ingest_plan_sse_payload(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
    raw: &str,
    max_output_lines_per_step: usize,
) -> Result<bool, String> {
    if serde_json::from_str::<Value>(raw.trim()).is_err() {
        return Ok(false);
    }
    let events = parse_plan_agent_events(raw);
    if events.is_empty() || !events_match_step_session(step, &events) {
        return Ok(false);
    }
    ingest_plan_agent_events(conn, step, events, max_output_lines_per_step)?;
    Ok(true)
}

pub fn reconcile_plan_step_from_server(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
    max_output_lines_per_step: usize,
) -> Result<bool, String> {
    let (Some(server_url), Some(session_id)) = (
        step.opencode_server_url.as_deref(),
        step.opencode_session_id.as_deref(),
    ) else {
        return Ok(false);
    };
    let status = crate::opencode::poll_session_status(server_url, session_id)?;
    reconcile_plan_step_from_opencode_status(conn, step, &status, max_output_lines_per_step)
}

pub fn reconcile_plan_step_from_opencode_status(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
    status: &OpencodeStatus,
    max_output_lines_per_step: usize,
) -> Result<bool, String> {
    let before = step.clone();
    if let Some(status_session_id) = status.session_id.as_deref() {
        if let Some(step_session_id) = step.opencode_session_id.as_deref()
            && step_session_id != status_session_id
        {
            return Ok(false);
        }
        if step.opencode_session_id.is_none() {
            step.opencode_session_id = Some(status_session_id.to_string());
        }
    }
    if step.opencode_server_url.is_none() {
        step.opencode_server_url = status.server_url.clone();
    }
    step.opencode_state = Some(status.state);

    let mut events = Vec::new();
    if let Some(session_id) = status.session_id.clone() {
        events.push(PlanAgentEvent::SessionIdentified {
            session_id,
            title: status.title.clone(),
        });
    }
    events.push(PlanAgentEvent::StateChanged {
        state: status.state.label().to_string(),
    });
    if let Some(text) = status.latest_message.as_ref()
        && step.latest_message.as_deref() != Some(text.as_str())
    {
        events.push(PlanAgentEvent::AssistantText { text: text.clone() });
    }
    if let Some(tool) = status.active_tool.as_ref()
        && step.active_tool.as_deref() != Some(tool.as_str())
    {
        events.push(PlanAgentEvent::ToolStarted {
            id: None,
            name: tool.clone(),
            args_summary: None,
        });
    } else if status.active_tool.is_none() && step.active_tool.is_some() {
        events.push(PlanAgentEvent::ToolFinished {
            id: None,
            status: "idle".to_string(),
        });
    }
    let todos = status
        .todos
        .iter()
        .map(|todo| PlanTodo::new(&todo.text, &todo.status))
        .collect::<Vec<_>>();
    if step.todos != todos {
        events.push(PlanAgentEvent::TodoUpdated { todos });
    }

    ingest_plan_agent_events(conn, step, events, max_output_lines_per_step)?;
    Ok(*step != before)
}

fn execute_one_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
    step_index: usize,
    executor: &PlanExecutorConfig,
    output: &mut dyn Write,
) -> Result<(), String> {
    {
        let step = &mut persisted.steps[step_index];
        step.status = PlanStepStatus::Starting;
        step.started_unix_ms = Some(unix_ms());
        step.opencode_server_url = executor.server_url.clone();
        step.agent_variant = executor.agent_variant.clone();
        step.error = None;
        persisted.run.selected_step = step.step;
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        save_step_with_conn(conn, step)?;
    }

    let step_number = persisted.steps[step_index].step;
    let prompt = persisted.steps[step_index].prompt.clone();
    writeln!(output, "\n==> {prompt}\n").map_err(|error| format!("write plan output: {error}"))?;

    let mut command = opencode_run_command(executor, step_number, &prompt, true);
    let spawn_result = spawn_opencode(&mut command);
    let (mut child, used_attach) = match spawn_result {
        Ok(child) => (child, true),
        Err(error) if executor.server_url.is_some() => {
            append_system_output(
                conn,
                &persisted.steps[step_index],
                PlanOutputKind::Error,
                &format!("attach launch failed, retrying without --attach: {error}"),
                executor.max_output_lines_per_step,
            )?;
            let mut fallback = opencode_run_command(executor, step_number, &prompt, false);
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
        step.status = PlanStepStatus::Running;
        step.process_id = Some(child.id());
        identify_attached_plan_session(executor, step);
        save_step_with_conn(conn, step)?;
    }

    let exit_code = collect_child_output(
        conn,
        &mut persisted.steps[step_index],
        &mut child,
        executor.max_output_lines_per_step,
        output,
    )?;

    let step = &mut persisted.steps[step_index];
    finish_step_after_exit(conn, step, exit_code, used_attach)?;
    if exit_code == 0 {
        Ok(())
    } else {
        Err(format!(
            "plan step {} failed: {}",
            step.step,
            step.error.as_deref().unwrap_or("opencode run failed")
        ))
    }
}

fn reload_pause_request(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
) -> Result<bool, String> {
    let Some(run) = load_run_with_conn(conn, &persisted.run.id)? else {
        return Ok(false);
    };
    persisted.run.pause_requested = run.pause_requested;
    if run.pause_requested || run.status == PlanRunStatus::Paused {
        persisted.run.status = PlanRunStatus::Paused;
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        return Ok(true);
    }
    Ok(false)
}

fn finish_step_after_exit(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
    exit_code: i32,
    used_attach: bool,
) -> Result<(), String> {
    step.process_id = None;
    step.finished_unix_ms = Some(unix_ms());
    step.exit_code = Some(exit_code);
    if exit_code == 0 {
        step.status = PlanStepStatus::Done;
        step.active_tool = None;
    } else {
        step.status = PlanStepStatus::Failed;
        let attach_note = if used_attach { " with --attach" } else { "" };
        step.error = Some(format!("opencode run{attach_note} exited with {exit_code}"));
    }
    save_step_with_conn(conn, step)
}

fn mark_spawn_failure(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
    error: &str,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    step.status = PlanStepStatus::Failed;
    step.finished_unix_ms = Some(unix_ms());
    step.error = Some(error.to_string());
    append_system_output(
        conn,
        step,
        PlanOutputKind::Error,
        error,
        max_output_lines_per_step,
    )?;
    save_step_with_conn(conn, step)
}

fn opencode_run_command(
    executor: &PlanExecutorConfig,
    step: usize,
    prompt: &str,
    attach: bool,
) -> Command {
    let mut command = Command::new(&executor.opencode_program);
    command.arg("run");
    if attach && let Some(server_url) = executor.server_url.as_deref() {
        command.arg("--attach").arg(server_url);
    }
    if let Some(variant) = executor.agent_variant.as_deref() {
        command.arg("--variant").arg(variant);
    }
    command
        .arg("--format")
        .arg("json")
        .arg("--dir")
        .arg(&executor.scope_path)
        .arg("--title")
        .arg(format!("{} phase {}", executor.title_prefix, step))
        .arg(prompt)
        .current_dir(&executor.scope_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(config_dir) = executor.plugin_config_dir.as_deref() {
        command.env("OPENCODE_CONFIG_DIR", config_dir);
    }
    if let Some(event_log_path) = executor.plugin_event_log_path.as_deref() {
        command.env("PRISM_PLAN_HOOK_LOG", event_log_path);
    }
    command
}

fn identify_attached_plan_session(executor: &PlanExecutorConfig, step: &mut PlanStepRun) {
    let Some(server_url) = executor.server_url.as_deref() else {
        return;
    };
    if step.opencode_session_id.is_some() {
        return;
    }
    let title = format!("{} phase {}", executor.title_prefix, step.step);
    if let Ok(sessions) = crate::opencode::list_sessions(server_url)
        && let Some(session) = sessions
            .iter()
            .filter(|session| session.title.as_deref() == Some(title.as_str()))
            .max_by(|left, right| left.time_updated.cmp(&right.time_updated))
    {
        step.opencode_server_url = Some(server_url.to_string());
        step.opencode_session_id = Some(session.id.clone());
    }
}

fn opencode_plan_plugin_config_json() -> &'static str {
    r#"{
  "$schema": "https://opencode.ai/config.json",
  "plugin": ["./prism-plan-plugin.js"]
}
"#
}

fn opencode_plan_plugin_js() -> &'static str {
    r#"import fs from "node:fs";

const hookLogPath = process.env.PRISM_PLAN_HOOK_LOG;

function summarize(value) {
  if (value === undefined || value === null) return value;
  if (typeof value === "string") return value.length > 500 ? `${value.slice(0, 500)}...` : value;
  if (Array.isArray(value)) return value.slice(0, 20).map(summarize);
  if (typeof value !== "object") return value;
  const out = {};
  for (const [key, child] of Object.entries(value)) {
    if (/token|secret|password|authorization|cookie/i.test(key)) {
      out[key] = "[redacted]";
    } else if (/command|args|input|patch|diff|content|text/i.test(key)) {
      out[key] = summarize(child);
    } else if (["id", "sessionID", "sessionId", "status", "title", "name", "tool"].includes(key)) {
      out[key] = summarize(child);
    }
  }
  return out;
}

function writeHook(type, payload) {
  if (!hookLogPath) return;
  const event = {
    type,
    time_unix_ms: Date.now(),
    properties: summarize(payload),
  };
  fs.appendFileSync(hookLogPath, `${JSON.stringify(event)}\n`, { mode: 0o600 });
}

export default async function PrismPlanPlugin() {
  return {
    event(input) {
      writeHook(input?.event?.type || input?.type || "event", input?.event || input);
    },
    "tool.execute.before"(input) {
      writeHook("tool.execute.before", input);
    },
    "tool.execute.after"(input) {
      writeHook("tool.execute.after", input);
    },
    "session.diff"(input) {
      writeHook("session.diff", input);
    },
    "session.compacted"(input) {
      writeHook("session.compacted", input);
    },
  };
}
"#
}

fn spawn_opencode(command: &mut Command) -> Result<Child, String> {
    command
        .spawn()
        .map_err(|error| format!("opencode: {error}"))
}

#[cfg(unix)]
fn process_is_running(process_id: u32) -> bool {
    let result = unsafe { libc::kill(process_id as libc::pid_t, 0) };
    result == 0
}

#[cfg(not(unix))]
fn process_is_running(process_id: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {process_id}")])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).contains(&process_id.to_string()))
        .unwrap_or(false)
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

fn reset_step_for_retry(step: &mut PlanStepRun) {
    step.status = PlanStepStatus::Queued;
    step.opencode_state = None;
    step.opencode_session_id = None;
    step.agent_variant = None;
    step.process_id = None;
    step.started_unix_ms = None;
    step.finished_unix_ms = None;
    step.exit_code = None;
    step.latest_message = None;
    step.active_tool = None;
    step.todos.clear();
    step.summary = None;
    step.error = None;
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

fn collect_child_output(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
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

#[derive(Debug)]
enum ParallelChildEvent {
    Line {
        step_index: usize,
        stream: StreamKind,
        text: String,
    },
    Exit {
        step_index: usize,
        exit_code: i32,
        used_attach: bool,
    },
}

fn spawn_parallel_child(
    step_index: usize,
    mut child: Child,
    used_attach: bool,
    tx: mpsc::Sender<Result<ParallelChildEvent, String>>,
) -> Result<(), String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "open opencode stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "open opencode stderr".to_string())?;
    spawn_parallel_reader(step_index, StreamKind::Stdout, stdout, tx.clone());
    spawn_parallel_reader(step_index, StreamKind::Stderr, stderr, tx.clone());
    thread::spawn(move || {
        let result = child
            .wait()
            .map_err(|error| format!("wait for opencode: {error}"))
            .map(|status| ParallelChildEvent::Exit {
                step_index,
                exit_code: status.code().unwrap_or(1),
                used_attach,
            });
        let _ = tx.send(result);
    });
    Ok(())
}

fn spawn_parallel_reader(
    step_index: usize,
    stream: StreamKind,
    reader: impl std::io::Read + Send + 'static,
    tx: mpsc::Sender<Result<ParallelChildEvent, String>>,
) {
    thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            match line {
                Ok(text) => {
                    let event = ParallelChildEvent::Line {
                        step_index,
                        stream,
                        text,
                    };
                    if tx.send(Ok(event)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = tx.send(Err(format!("read opencode output: {error}")));
                    return;
                }
            }
        }
    });
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
    step: &mut PlanStepRun,
    stream: StreamKind,
    raw: &str,
    max_output_lines_per_step: usize,
    output: &mut dyn Write,
) -> Result<(), String> {
    if stream == StreamKind::Stderr {
        append_system_output(
            conn,
            step,
            PlanOutputKind::Error,
            raw,
            max_output_lines_per_step,
        )?;
        step.error = Some(raw.to_string());
        save_step_with_conn(conn, step)?;
        writeln!(output, "[stderr] {raw}")
            .map_err(|error| format!("write plan output: {error}"))?;
        return Ok(());
    }

    let events = parse_plan_agent_events(raw);
    for event in events {
        let text = ingest_single_plan_agent_event(conn, step, event, max_output_lines_per_step)?;
        writeln!(output, "{text}").map_err(|error| format!("write plan output: {error}"))?;
    }
    save_step_with_conn(conn, step)?;
    Ok(())
}

fn ingest_plan_agent_events(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
    events: Vec<PlanAgentEvent>,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    for event in events {
        ingest_single_plan_agent_event(conn, step, event, max_output_lines_per_step)?;
    }
    save_step_with_conn(conn, step)
}

fn ingest_single_plan_agent_event(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
    event: PlanAgentEvent,
    max_output_lines_per_step: usize,
) -> Result<String, String> {
    let (kind, text, block_id) = apply_agent_event(step, event);
    append_system_output_with_block(
        conn,
        step,
        kind,
        &text,
        block_id.as_deref(),
        max_output_lines_per_step,
    )?;
    Ok(text)
}

fn events_match_step_session(step: &PlanStepRun, events: &[PlanAgentEvent]) -> bool {
    let event_session_ids = events
        .iter()
        .filter_map(|event| match event {
            PlanAgentEvent::SessionIdentified { session_id, .. } => Some(session_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if let Some(step_session_id) = step.opencode_session_id.as_deref() {
        return event_session_ids
            .iter()
            .all(|event_session_id| *event_session_id == step_session_id);
    }
    event_session_ids.len() == 1
}

fn apply_agent_event(
    step: &mut PlanStepRun,
    event: PlanAgentEvent,
) -> (PlanOutputKind, String, Option<String>) {
    match event {
        PlanAgentEvent::SessionIdentified { session_id, title } => {
            step.opencode_session_id = Some(session_id.clone());
            let title = title
                .map(|title| format!(" title: {title}"))
                .unwrap_or_default();
            (
                PlanOutputKind::Status,
                format!("session {session_id}{title}"),
                None,
            )
        }
        PlanAgentEvent::StateChanged { state } => {
            step.opencode_state = OpencodeState::parse(&state);
            if state == OpencodeState::Idle.label() {
                step.active_tool = None;
            }
            (PlanOutputKind::Status, format!("status: {state}"), None)
        }
        PlanAgentEvent::AssistantText { text } => {
            step.latest_message = Some(text.clone());
            (PlanOutputKind::Assistant, text, None)
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
            step.active_tool = Some(text.clone());
            (PlanOutputKind::Tool, text, id)
        }
        PlanAgentEvent::ToolOutput { id, text } => (PlanOutputKind::ToolOutput, text, id),
        PlanAgentEvent::ToolFinished { id, status } => {
            step.active_tool = None;
            (PlanOutputKind::Tool, format!("tool finished: {status}"), id)
        }
        PlanAgentEvent::TodoUpdated { todos } => {
            let text = format!("todos updated: {}", todos.len());
            step.todos = todos;
            (PlanOutputKind::Todo, text, None)
        }
        PlanAgentEvent::DiffUpdated { summary, patch } => {
            let text = patch
                .map(|patch| format!("{summary}\n{patch}"))
                .unwrap_or(summary);
            (PlanOutputKind::Diff, text, None)
        }
        PlanAgentEvent::Error { message } => {
            step.error = Some(message.clone());
            (PlanOutputKind::Error, message, None)
        }
        PlanAgentEvent::Raw { event_type, json } => (
            PlanOutputKind::RawJson,
            format!("[{event_type}] {json}"),
            None,
        ),
    }
}

fn append_system_output(
    conn: &rusqlite::Connection,
    step: &PlanStepRun,
    kind: PlanOutputKind,
    text: &str,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    append_system_output_with_block(conn, step, kind, text, None, max_output_lines_per_step)
}

fn append_system_output_with_block(
    conn: &rusqlite::Connection,
    step: &PlanStepRun,
    kind: PlanOutputKind,
    text: &str,
    block_id: Option<&str>,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let line_number = next_output_line_number(conn, &step.run_id, step.step)?;
    append_output_line(
        conn,
        &PlanOutputLine {
            run_id: step.run_id.clone(),
            step: step.step,
            line_number,
            time_unix_ms: unix_ms(),
            kind,
            text: text.to_string(),
            block_id: block_id.map(str::to_string),
        },
        max_output_lines_per_step,
    )
}

fn next_output_line_number(
    conn: &rusqlite::Connection,
    run_id: &str,
    step: usize,
) -> Result<u64, String> {
    let current: Option<i64> = conn
        .query_row(
            "select max(line_number) from plan_output_line where run_id = ?1 and step = ?2",
            params![run_id, usize_to_i64(step)],
            |row| row.get(0),
        )
        .map_err(|error| format!("read next plan output line number: {error}"))?;
    Ok(current.unwrap_or(0).max(0) as u64 + 1)
}

fn todos_from_value(value: &Value) -> Option<Vec<PlanTodo>> {
    find_key_deep(value, "todos")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let title = string_field_deep(item, &["title", "text", "content"])?;
                    let status = string_field_deep(item, &["status", "state"])
                        .unwrap_or_else(|| "pending".into());
                    Some(PlanTodo::new(title, status))
                })
                .collect::<Vec<_>>()
        })
}

fn should_keep_raw(event_type: &str, value: &Value) -> bool {
    let lower_type = event_type.to_ascii_lowercase();
    lower_type.contains("tool")
        || lower_type.contains("diff")
        || lower_type.contains("error")
        || find_key_deep(value, "input").is_some()
        || find_key_deep(value, "arguments").is_some()
}

fn string_field_deep(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| find_key_deep(value, key).and_then(value_to_string))
}

fn nested_string(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    value_to_string(current)
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn find_key_deep<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|v| find_key_deep(v, key))),
        Value::Array(items) => items.iter().find_map(|item| find_key_deep(item, key)),
        _ => None,
    }
}

pub fn load_output_lines(
    conn: &rusqlite::Connection,
    run_id: &str,
    step: usize,
) -> Result<Vec<PlanOutputLine>, String> {
    let mut statement = conn
        .prepare(
            "select run_id, step, line_number, time_unix_ms, kind, text, block_id
             from plan_output_line
             where run_id = ?1 and step = ?2
             order by line_number",
        )
        .map_err(|error| format!("prepare plan output load: {error}"))?;
    let rows = statement
        .query_map(params![run_id, usize_to_i64(step)], |row| {
            let kind: String = row.get(4)?;
            Ok(PlanOutputLine {
                run_id: row.get(0)?,
                step: i64_to_usize(row.get(1)?, 1),
                line_number: i64_to_u64(row.get(2)?, 2),
                time_unix_ms: i64_to_u64(row.get(3)?, 3),
                kind: PlanOutputKind::parse(&kind).map_err(from_string_error)?,
                text: row.get(5)?,
                block_id: row.get(6)?,
            })
        })
        .map_err(|error| format!("load plan output lines: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read plan output lines: {error}"))
}

fn save_run_with_conn(conn: &rusqlite::Connection, run: &PlanRun) -> Result<(), String> {
    conn.execute(
        "insert into plan_run (
           id, repo_root, scope_path, plan_path, plan_display, step_name, start_step,
           total_steps, mode, status, pause_requested, selected_step, created_unix_ms,
           updated_unix_ms, archived_unix_ms
         ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         on conflict(id) do update set
           repo_root = excluded.repo_root,
           scope_path = excluded.scope_path,
           plan_path = excluded.plan_path,
           plan_display = excluded.plan_display,
           step_name = excluded.step_name,
           start_step = excluded.start_step,
           total_steps = excluded.total_steps,
           mode = excluded.mode,
           status = excluded.status,
           pause_requested = excluded.pause_requested,
           selected_step = excluded.selected_step,
           updated_unix_ms = excluded.updated_unix_ms,
           archived_unix_ms = excluded.archived_unix_ms",
        params![
            run.id.as_str(),
            run.repo_root.as_str(),
            run.scope_path.display().to_string(),
            run.plan_path.display().to_string(),
            run.plan_display.as_str(),
            run.step_name.as_str(),
            usize_to_i64(run.start_step),
            usize_to_i64(run.total_steps),
            run.mode.as_str(),
            run.status.as_str(),
            bool_to_i64(run.pause_requested),
            usize_to_i64(run.selected_step),
            u64_to_i64(run.created_unix_ms),
            u64_to_i64(run.updated_unix_ms),
            run.archived_unix_ms.map(u64_to_i64),
        ],
    )
    .map_err(|error| format!("write plan run: {error}"))?;
    Ok(())
}

fn save_step_with_conn(conn: &rusqlite::Connection, step: &PlanStepRun) -> Result<(), String> {
    let todos_json = serde_json::to_string(
        &step
            .todos
            .iter()
            .map(|todo| {
                let mut map = BTreeMap::new();
                map.insert("title", todo.title.as_str());
                map.insert("status", todo.status.as_str());
                map
            })
            .collect::<Vec<_>>(),
    )
    .map_err(|error| format!("serialize plan todos: {error}"))?;
    conn.execute(
        "insert into plan_step_run (
           run_id, step, prompt, status, opencode_state, opencode_server_url, opencode_session_id,
           agent_variant, process_id, started_unix_ms, finished_unix_ms, exit_code, latest_message,
           active_tool, todos_json, summary, error
          ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
          on conflict(run_id, step) do update set
            prompt = excluded.prompt,
            status = excluded.status,
             opencode_state = excluded.opencode_state,
             opencode_server_url = excluded.opencode_server_url,
             opencode_session_id = excluded.opencode_session_id,
             agent_variant = excluded.agent_variant,
             process_id = excluded.process_id,
             started_unix_ms = excluded.started_unix_ms,
             finished_unix_ms = excluded.finished_unix_ms,
             exit_code = excluded.exit_code,
             latest_message = excluded.latest_message,
             active_tool = excluded.active_tool,
             todos_json = excluded.todos_json,
             summary = excluded.summary,
             error = excluded.error",
        params![
            step.run_id.as_str(),
            usize_to_i64(step.step),
            step.prompt.as_str(),
            step.status.as_str(),
            step.opencode_state.map(OpencodeState::label),
            step.opencode_server_url.as_deref(),
            step.opencode_session_id.as_deref(),
            step.agent_variant.as_deref(),
            step.process_id.map(i64::from),
            step.started_unix_ms.map(u64_to_i64),
            step.finished_unix_ms.map(u64_to_i64),
            step.exit_code,
            step.latest_message.as_deref(),
            step.active_tool.as_deref(),
            todos_json,
            step.summary.as_deref(),
            step.error.as_deref(),
        ],
    )
    .map_err(|error| format!("write plan step run: {error}"))?;
    Ok(())
}

fn load_run_with_conn(
    conn: &rusqlite::Connection,
    run_id: &str,
) -> Result<Option<PlanRun>, String> {
    conn.query_row(
        "select id, repo_root, scope_path, plan_path, plan_display, step_name,
                start_step, total_steps, mode, status, pause_requested, selected_step,
                created_unix_ms, updated_unix_ms, archived_unix_ms
         from plan_run
         where id = ?1",
        params![run_id],
        |row| {
            let mode: String = row.get(8)?;
            let status: String = row.get(9)?;
            Ok(PlanRun {
                id: row.get(0)?,
                repo_root: row.get(1)?,
                scope_path: PathBuf::from(row.get::<_, String>(2)?),
                plan_path: PathBuf::from(row.get::<_, String>(3)?),
                plan_display: row.get(4)?,
                step_name: row.get(5)?,
                start_step: i64_to_usize(row.get(6)?, 6),
                total_steps: i64_to_usize(row.get(7)?, 7),
                mode: PlanRunMode::parse(&mode).map_err(from_string_error)?,
                status: PlanRunStatus::parse(&status).map_err(from_string_error)?,
                pause_requested: row.get::<_, i64>(10)? != 0,
                selected_step: i64_to_usize(row.get(11)?, 11),
                created_unix_ms: i64_to_u64(row.get(12)?, 12),
                updated_unix_ms: i64_to_u64(row.get(13)?, 13),
                archived_unix_ms: row
                    .get::<_, Option<i64>>(14)?
                    .map(|value| value.max(0) as u64),
            })
        },
    )
    .optional()
    .map_err(|error| format!("load plan run: {error}"))
}

fn load_steps_with_conn(
    conn: &rusqlite::Connection,
    run_id: &str,
) -> Result<Vec<PlanStepRun>, String> {
    let mut statement = conn
        .prepare(
            "select run_id, step, prompt, status, opencode_state, opencode_server_url, opencode_session_id,
                agent_variant, process_id, started_unix_ms, finished_unix_ms, exit_code,
                    latest_message, active_tool, todos_json, summary, error
             from plan_step_run
             where run_id = ?1
             order by step",
        )
        .map_err(|error| format!("prepare plan step load: {error}"))?;
    let rows = statement
        .query_map(params![run_id], |row| {
            let status: String = row.get(3)?;
            let opencode_state: Option<String> = row.get(4)?;
            let todos_json: String = row.get(14)?;
            Ok(PlanStepRun {
                run_id: row.get(0)?,
                step: i64_to_usize(row.get(1)?, 1),
                prompt: row.get(2)?,
                status: PlanStepStatus::parse(&status).map_err(from_string_error)?,
                opencode_state: opencode_state.as_deref().and_then(OpencodeState::parse),
                opencode_server_url: row.get(5)?,
                opencode_session_id: row.get(6)?,
                agent_variant: row.get(7)?,
                process_id: row
                    .get::<_, Option<i64>>(8)?
                    .map(|value| value.max(0) as u32),
                started_unix_ms: row
                    .get::<_, Option<i64>>(9)?
                    .map(|value| value.max(0) as u64),
                finished_unix_ms: row
                    .get::<_, Option<i64>>(10)?
                    .map(|value| value.max(0) as u64),
                exit_code: row.get(11)?,
                latest_message: row.get(12)?,
                active_tool: row.get(13)?,
                todos: parse_todos_json(&todos_json).map_err(from_string_error)?,
                summary: row.get(15)?,
                error: row.get(16)?,
            })
        })
        .map_err(|error| format!("load plan steps: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read plan steps: {error}"))
}

fn trim_output_lines(
    conn: &rusqlite::Connection,
    run_id: &str,
    step: usize,
    max_lines_per_step: usize,
) -> Result<(), String> {
    if max_lines_per_step == 0 {
        return Ok(());
    }
    let retained_line_count = max_lines_per_step.saturating_sub(1);
    if retained_line_count == 0 {
        conn.execute(
            "delete from plan_output_line where run_id = ?1 and step = ?2",
            params![run_id, usize_to_i64(step)],
        )
        .map_err(|error| format!("trim plan output lines: {error}"))?;
        return Ok(());
    }
    let deleted = conn
        .execute(
            "delete from plan_output_line
             where run_id = ?1
               and step = ?2
               and line_number not in (
                 select line_number
                 from plan_output_line
                 where run_id = ?1 and step = ?2
                 order by line_number desc
                 limit ?3
               )",
            params![
                run_id,
                usize_to_i64(step),
                usize_to_i64(retained_line_count),
            ],
        )
        .map_err(|error| format!("trim plan output lines: {error}"))?;
    if deleted == 0 {
        return Ok(());
    }
    let first_retained: Option<i64> = conn
        .query_row(
            "select min(line_number) from plan_output_line where run_id = ?1 and step = ?2",
            params![run_id, usize_to_i64(step)],
            |row| row.get(0),
        )
        .map_err(|error| format!("read retained plan output marker position: {error}"))?;
    let Some(first_retained) = first_retained else {
        return Ok(());
    };
    let marker_line = first_retained.saturating_sub(1);
    conn.execute(
        "insert or replace into plan_output_line (
           run_id, step, line_number, time_unix_ms, kind, text, block_id
         ) values (?1, ?2, ?3, ?4, 'system', ?5, null)",
        params![
            run_id,
            usize_to_i64(step),
            marker_line,
            u64_to_i64(unix_ms()),
            format!("[... omitted {deleted} older output lines ...]"),
        ],
    )
    .map_err(|error| format!("write plan output omission marker: {error}"))?;
    Ok(())
}

fn parse_todos_json(text: &str) -> Result<Vec<PlanTodo>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|error| format!("parse todos json: {error}"))?;
    let Some(items) = value.as_array() else {
        return Ok(Vec::new());
    };
    Ok(items
        .iter()
        .filter_map(|item| {
            Some(PlanTodo {
                title: item.get("title")?.as_str()?.to_string(),
                status: item.get("status")?.as_str()?.to_string(),
            })
        })
        .collect())
}

fn from_string_error(error: String) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(error.into())
}

fn add_column_if_missing(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
    sql: &str,
) -> Result<(), String> {
    let mut statement = conn
        .prepare(&format!("pragma table_info({table})"))
        .map_err(|error| format!("inspect {table} schema: {error}"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("read {table} schema: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read {table} schema column: {error}"))?;
    if !columns.iter().any(|name| name == column) {
        conn.execute(sql, [])
            .map_err(|error| format!("migrate {table}.{column}: {error}"))?;
    }
    Ok(())
}

fn i64_to_usize(value: i64, index: usize) -> usize {
    usize::try_from(value)
        .unwrap_or_else(|_| panic!("SQLite column {index} contained invalid usize: {value}"))
}

fn i64_to_u64(value: i64, index: usize) -> u64 {
    u64::try_from(value)
        .unwrap_or_else(|_| panic!("SQLite column {index} contained invalid u64: {value}"))
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

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn launch_creates_queued_steps_with_prompts() {
        let repo = PathBuf::from("/repo/prism");
        let plan = repo.join("plan-plan.md");
        let launch = PlanLaunch::new(&repo, &repo, &plan, "phase", 2, 4, PlanRunMode::Sequential)
            .expect("launch");

        let persisted = launch.create_run();

        assert_eq!(persisted.run.plan_display, "plan-plan.md");
        assert_eq!(persisted.run.selected_step, 2);
        assert_eq!(persisted.run.status, PlanRunStatus::Queued);
        assert_eq!(
            persisted
                .steps
                .iter()
                .map(|step| step.prompt.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Implement plan-plan.md phase 2",
                "Implement plan-plan.md phase 3",
                "Implement plan-plan.md phase 4",
            ]
        );
    }

    #[test]
    fn opencode_run_command_passes_prompt_as_single_raw_argument() {
        let scope_path = PathBuf::from("/repo/prism");
        let executor = PlanExecutorConfig::new(
            "opencode".to_string(),
            Some("http://127.0.0.1:41234".to_string()),
            scope_path.clone(),
            "plan with spaces.md",
        );
        let prompt = "  Implement plan phase 3\n\"quotes\" and $PATH && true\n--leading-dash";

        let command = opencode_run_command(&executor, 3, prompt, true);
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();

        assert_eq!(args.last().map(String::as_str), Some(prompt));
        assert!(args.windows(2).any(|args| args == ["--variant", "medium"]));
        assert!(!args.iter().any(|arg| arg == &format!("'{prompt}'")));
        assert_eq!(args.iter().filter(|arg| arg.as_str() == prompt).count(), 1);
        assert_eq!(command.get_current_dir(), Some(scope_path.as_path()));
    }

    #[test]
    fn aggregate_status_prioritizes_failure_and_running_state() {
        assert_eq!(
            aggregate_step_status([
                PlanStepStatus::Done,
                PlanStepStatus::Queued,
                PlanStepStatus::Done
            ]),
            PlanRunStatus::Queued
        );
        assert_eq!(
            aggregate_step_status([PlanStepStatus::Done, PlanStepStatus::Running]),
            PlanRunStatus::Running
        );
        assert_eq!(
            aggregate_step_status([PlanStepStatus::Running, PlanStepStatus::Failed]),
            PlanRunStatus::Failed
        );
        assert_eq!(
            aggregate_step_status([PlanStepStatus::Done, PlanStepStatus::Skipped]),
            PlanRunStatus::Done
        );
    }

    #[test]
    fn schema_round_trips_plan_run_steps_and_output() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();

        let repo = PathBuf::from("/repo/prism");
        let plan = repo.join("plans/plan-one.md");
        let mut persisted =
            PlanLaunch::new(&repo, &repo, &plan, "phase", 1, 2, PlanRunMode::Parallel)
                .unwrap()
                .create_run();
        persisted.run.status = PlanRunStatus::Running;
        persisted.steps[0].status = PlanStepStatus::Done;
        persisted.steps[0].latest_message = Some("finished phase 1".to_string());
        persisted.steps[0].todos = vec![PlanTodo::new("write tests", "done")];
        persisted.steps[1].status = PlanStepStatus::Running;
        persisted.steps[1].active_tool = Some("bash running: cargo test".to_string());

        save_plan_run(&conn, &persisted).unwrap();

        let loaded = load_plan_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("run");
        assert_eq!(loaded.run, persisted.run);
        assert_eq!(loaded.steps, persisted.steps);
        assert_eq!(loaded.status_counts().done, 1);
        assert_eq!(loaded.status_counts().running, 1);
        assert_eq!(loaded.aggregate_status(), PlanRunStatus::Running);

        for line_number in 1..=3 {
            append_output_line(
                &conn,
                &PlanOutputLine {
                    run_id: persisted.run.id.clone(),
                    step: 1,
                    line_number,
                    time_unix_ms: 100 + line_number,
                    kind: PlanOutputKind::Assistant,
                    text: format!("line {line_number}"),
                    block_id: None,
                },
                2,
            )
            .unwrap();
        }

        let output = load_output_lines(&conn, &persisted.run.id, 1).unwrap();
        assert_eq!(
            output
                .iter()
                .map(|line| (line.line_number, line.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(2, "[... omitted 2 older output lines ...]"), (3, "line 3")]
        );
    }

    #[test]
    fn plan_plugin_config_is_generated_under_prism_state() {
        let temp = unique_temp_dir("prism-plan-plugin-config");
        let repo_root = temp.join("repo");
        let prism_dir = temp.join("config/prism/repos/repo-1234");
        std::fs::create_dir_all(&repo_root).unwrap();

        let plugin = prepare_plan_plugin_config(&prism_dir).unwrap();

        assert!(plugin.config_dir.starts_with(&prism_dir));
        assert!(!plugin.config_dir.starts_with(&repo_root));
        assert!(plugin.plugin_path.is_file());
        assert!(plugin.config_dir.join("opencode.json").is_file());
        assert_eq!(
            plugin.event_log_path,
            plugin.config_dir.join("events.jsonl")
        );
        let config = std::fs::read_to_string(plugin.config_dir.join("opencode.json")).unwrap();
        assert!(config.contains("prism-plan-plugin.js"));
        let plugin_source = std::fs::read_to_string(plugin.plugin_path).unwrap();
        assert!(plugin_source.contains("tool.execute.before"));
        assert!(plugin_source.contains("session.compacted"));

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn executor_passes_plugin_environment_only_when_enabled() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let temp = unique_temp_dir("prism-plan-plugin-env");
        let observed_config_dir = PathBuf::from("observed-config-dir");
        let observed_hook_log = PathBuf::from("observed-hook-log");
        let opencode = fake_opencode(
            &temp,
            r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s' "${OPENCODE_CONFIG_DIR:-}" > observed-config-dir
printf '%s' "${PRISM_PLAN_HOOK_LOG:-}" > observed-hook-log
echo '{"type":"message","text":"plugin env observed"}'
"#,
        );
        let plugin = prepare_plan_plugin_config(&temp.join("prism-state")).unwrap();
        let mut persisted = PlanLaunch::new(
            &temp,
            &temp,
            &temp.join("plan.md"),
            "phase",
            1,
            1,
            PlanRunMode::Sequential,
        )
        .unwrap()
        .create_run();
        save_plan_run(&conn, &persisted).unwrap();
        let executor = PlanExecutorConfig::new(
            opencode.display().to_string(),
            None,
            temp.clone(),
            "plan.md",
        )
        .with_plugin_config(plugin.clone());
        let mut output = Vec::new();

        execute_plan_sequential(&conn, &mut persisted, &executor, &mut output).unwrap();

        assert_eq!(
            std::fs::read_to_string(temp.join(observed_config_dir)).unwrap(),
            plugin.config_dir.display().to_string()
        );
        assert_eq!(
            std::fs::read_to_string(temp.join(observed_hook_log)).unwrap(),
            plugin.event_log_path.display().to_string()
        );

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn sequential_executor_updates_steps_from_fake_opencode() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let temp = unique_temp_dir("prism-plan-executor-success");
        let opencode = fake_opencode(
            &temp,
            r#"#!/usr/bin/env bash
set -euo pipefail
echo '{"type":"session","session_id":"ses_test","title":"phase"}'
echo '{"type":"message","text":"working"}'
echo '{"type":"tool.call","id":"tool_1","name":"bash","input":{"command":"cargo test"}}'
echo '{"type":"todo.updated","todos":[{"title":"write tests","status":"done"}]}'
"#,
        );
        let mut persisted = PlanLaunch::new(
            &temp,
            &temp,
            &temp.join("plan.md"),
            "phase",
            1,
            2,
            PlanRunMode::Sequential,
        )
        .unwrap()
        .create_run();
        save_plan_run(&conn, &persisted).unwrap();

        let executor = PlanExecutorConfig::new(
            opencode.display().to_string(),
            Some("http://127.0.0.1:41234".to_string()),
            temp.clone(),
            "plan.md",
        );
        let mut output = Vec::new();

        execute_plan_sequential(&conn, &mut persisted, &executor, &mut output).unwrap();

        let loaded = load_plan_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("persisted run");
        assert_eq!(loaded.run.status, PlanRunStatus::Done);
        assert!(
            loaded
                .steps
                .iter()
                .all(|step| step.status == PlanStepStatus::Done)
        );
        assert_eq!(
            loaded.steps[0].opencode_server_url.as_deref(),
            Some("http://127.0.0.1:41234")
        );
        assert_eq!(
            loaded.steps[0].opencode_session_id.as_deref(),
            Some("ses_test")
        );
        assert_eq!(loaded.steps[0].latest_message.as_deref(), Some("working"));
        assert_eq!(
            loaded.steps[0].todos,
            vec![PlanTodo::new("write tests", "done")]
        );
        assert!(
            load_output_lines(&conn, &persisted.run.id, 1)
                .unwrap()
                .iter()
                .any(|line| line.kind == PlanOutputKind::Tool)
        );

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn sequential_executor_stops_on_failed_step() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let temp = unique_temp_dir("prism-plan-executor-failure");
        let opencode = fake_opencode(
            &temp,
            r#"#!/usr/bin/env bash
set -euo pipefail
echo '{"type":"message","text":"started"}'
if [[ "$*" == *"phase 2"* ]]; then
  echo 'phase 2 failed' >&2
  exit 7
fi
"#,
        );
        let mut persisted = PlanLaunch::new(
            &temp,
            &temp,
            &temp.join("plan.md"),
            "phase",
            1,
            3,
            PlanRunMode::Sequential,
        )
        .unwrap()
        .create_run();
        save_plan_run(&conn, &persisted).unwrap();
        let executor = PlanExecutorConfig::new(
            opencode.display().to_string(),
            None,
            temp.clone(),
            "plan.md",
        );
        let mut output = Vec::new();

        let error = execute_plan_sequential(&conn, &mut persisted, &executor, &mut output)
            .expect_err("phase 2 should fail");

        assert!(error.contains("plan step 2 failed"));
        let loaded = load_plan_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("persisted run");
        assert_eq!(loaded.run.status, PlanRunStatus::Failed);
        assert_eq!(loaded.steps[0].status, PlanStepStatus::Done);
        assert_eq!(loaded.steps[1].status, PlanStepStatus::Failed);
        assert_eq!(loaded.steps[1].exit_code, Some(7));
        assert_eq!(loaded.steps[2].status, PlanStepStatus::Queued);
        assert!(
            load_output_lines(&conn, &persisted.run.id, 2)
                .unwrap()
                .iter()
                .any(|line| line.kind == PlanOutputKind::Error && line.text == "phase 2 failed")
        );

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn parallel_executor_runs_all_steps_and_waits_for_failures() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let temp = unique_temp_dir("prism-plan-executor-parallel-failure");
        let marker = temp.join("phase-3-finished");
        let opencode = fake_opencode(
            &temp,
            r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "$*" == *"phase 1"* ]]; then
  echo '{"type":"message","text":"phase 1 done"}'
  exit 0
fi
if [[ "$*" == *"phase 2"* ]]; then
  echo '{"type":"message","text":"phase 2 failed"}'
  sleep 0.1
  exit 9
fi
if [[ "$*" == *"phase 3"* ]]; then
  sleep 0.3
  echo '{"type":"message","text":"phase 3 done"}'
  touch phase-3-finished
  exit 0
fi
"#,
        );
        let mut persisted = PlanLaunch::new(
            &temp,
            &temp,
            &temp.join("plan.md"),
            "phase",
            1,
            3,
            PlanRunMode::Parallel,
        )
        .unwrap()
        .create_run();
        save_plan_run(&conn, &persisted).unwrap();
        let executor = PlanExecutorConfig::new(
            opencode.display().to_string(),
            None,
            temp.clone(),
            "plan.md",
        );
        let mut output = Vec::new();

        let error = execute_plan_parallel(&conn, &mut persisted, &executor, &mut output)
            .expect_err("phase 2 should fail");

        assert!(error.contains("parallel plan failed"));
        assert!(
            marker.exists(),
            "phase 3 should continue after phase 2 fails"
        );
        let loaded = load_plan_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("persisted run");
        assert_eq!(loaded.run.status, PlanRunStatus::Failed);
        assert_eq!(loaded.steps[0].status, PlanStepStatus::Done);
        assert_eq!(loaded.steps[1].status, PlanStepStatus::Failed);
        assert_eq!(loaded.steps[1].exit_code, Some(9));
        assert_eq!(loaded.steps[2].status, PlanStepStatus::Done);
        assert_eq!(
            loaded.steps[2].latest_message.as_deref(),
            Some("phase 3 done")
        );

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn parallel_executor_marks_success_after_all_steps_finish() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let temp = unique_temp_dir("prism-plan-executor-parallel-success");
        let opencode = fake_opencode(
            &temp,
            r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "$*" == *"phase 1"* ]]; then
  sleep 0.2
  echo '{"type":"message","text":"phase 1 done"}'
else
  echo '{"type":"message","text":"phase 2 done"}'
fi
"#,
        );
        let mut persisted = PlanLaunch::new(
            &temp,
            &temp,
            &temp.join("plan.md"),
            "phase",
            1,
            2,
            PlanRunMode::Parallel,
        )
        .unwrap()
        .create_run();
        save_plan_run(&conn, &persisted).unwrap();
        let executor = PlanExecutorConfig::new(
            opencode.display().to_string(),
            None,
            temp.clone(),
            "plan.md",
        );
        let mut output = Vec::new();

        execute_plan_parallel(&conn, &mut persisted, &executor, &mut output).unwrap();

        let loaded = load_plan_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("persisted run");
        assert_eq!(loaded.run.status, PlanRunStatus::Done);
        assert!(
            loaded
                .steps
                .iter()
                .all(|step| step.status == PlanStepStatus::Done)
        );

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn parses_known_plan_agent_events_without_panics() {
        let events = parse_plan_agent_events(
            r#"{"type":"tool.execute.before","session":{"id":"ses_1"},"name":"bash","input":{"command":"cargo test"}}"#,
        );

        assert!(events.iter().any(|event| matches!(
            event,
            PlanAgentEvent::SessionIdentified { session_id, .. } if session_id == "ses_1"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            PlanAgentEvent::ToolStarted { name, args_summary, .. }
                if name == "bash" && args_summary.as_deref() == Some("cargo test")
        )));
    }

    #[test]
    fn sse_payload_ingestion_updates_matching_step_and_ignores_other_sessions() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted = PlanLaunch::new(
            &repo,
            &repo,
            &repo.join("plan.md"),
            "phase",
            1,
            1,
            PlanRunMode::Sequential,
        )
        .unwrap()
        .create_run();
        persisted.steps[0].opencode_session_id = Some("ses_plan".to_string());
        save_plan_run(&conn, &persisted).unwrap();

        let matched = ingest_plan_sse_payload(
            &conn,
            &mut persisted.steps[0],
            r#"{"type":"message.part.updated","properties":{"sessionID":"ses_plan","role":"assistant","text":"live update"}}"#,
            DEFAULT_OUTPUT_LINES_PER_STEP,
        )
        .unwrap();
        let ignored = ingest_plan_sse_payload(
            &conn,
            &mut persisted.steps[0],
            r#"{"type":"message.part.updated","properties":{"sessionID":"ses_other","role":"assistant","text":"wrong run"}}"#,
            DEFAULT_OUTPUT_LINES_PER_STEP,
        )
        .unwrap();

        assert!(matched);
        assert!(!ignored);
        assert_eq!(
            persisted.steps[0].latest_message.as_deref(),
            Some("live update")
        );
        let output = load_output_lines(&conn, &persisted.run.id, 1).unwrap();
        assert!(output.iter().any(|line| line.text == "live update"));
        assert!(!output.iter().any(|line| line.text.contains("wrong run")));
    }

    #[test]
    fn sse_payload_ingestion_tracks_session_and_raw_relevant_unknown_events() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted = PlanLaunch::new(
            &repo,
            &repo,
            &repo.join("plan.md"),
            "phase",
            1,
            1,
            PlanRunMode::Sequential,
        )
        .unwrap()
        .create_run();
        save_plan_run(&conn, &persisted).unwrap();

        let matched = ingest_plan_sse_payload(
            &conn,
            &mut persisted.steps[0],
            r#"{"type":"session.status","properties":{"sessionID":"ses_new","status":"busy","title":"plan phase 1"}}"#,
            DEFAULT_OUTPUT_LINES_PER_STEP,
        )
        .unwrap();
        ingest_plan_sse_payload(
            &conn,
            &mut persisted.steps[0],
            r#"{"type":"tool.execute.after","properties":{"sessionID":"ses_new","id":"tool_1","name":"bash","status":"success","arguments":{"command":"cargo test"}}}"#,
            DEFAULT_OUTPUT_LINES_PER_STEP,
        )
        .unwrap();
        let malformed = ingest_plan_sse_payload(
            &conn,
            &mut persisted.steps[0],
            "not json",
            DEFAULT_OUTPUT_LINES_PER_STEP,
        )
        .unwrap();

        assert!(matched);
        assert!(!malformed);
        assert_eq!(
            persisted.steps[0].opencode_session_id.as_deref(),
            Some("ses_new")
        );
        assert_eq!(persisted.steps[0].active_tool, None);
        let output = load_output_lines(&conn, &persisted.run.id, 1).unwrap();
        assert!(
            output
                .iter()
                .any(|line| line.kind == PlanOutputKind::RawJson
                    && line.text.contains("tool.execute.after"))
        );
    }

    #[test]
    fn poll_reconciliation_recovers_latest_status_message_tool_and_todos() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted = PlanLaunch::new(
            &repo,
            &repo,
            &repo.join("plan.md"),
            "phase",
            1,
            1,
            PlanRunMode::Sequential,
        )
        .unwrap()
        .create_run();
        persisted.steps[0].opencode_server_url = Some("http://127.0.0.1:41234".to_string());
        persisted.steps[0].opencode_session_id = Some("ses_plan".to_string());
        save_plan_run(&conn, &persisted).unwrap();

        let status = OpencodeStatus {
            server_url: Some("http://127.0.0.1:41234".to_string()),
            session_id: Some("ses_plan".to_string()),
            title: Some("plan phase 1".to_string()),
            state: OpencodeState::Busy,
            latest_message: Some("recovered message".to_string()),
            active_tool: Some("bash running".to_string()),
            todos: vec![crate::opencode::OpencodeTodo {
                text: "finish phase".to_string(),
                status: "in_progress".to_string(),
            }],
            last_updated_unix_ms: Some(42),
        };

        reconcile_plan_step_from_opencode_status(
            &conn,
            &mut persisted.steps[0],
            &status,
            DEFAULT_OUTPUT_LINES_PER_STEP,
        )
        .unwrap();

        assert_eq!(
            persisted.steps[0].latest_message.as_deref(),
            Some("recovered message")
        );
        assert_eq!(
            persisted.steps[0].todos,
            vec![PlanTodo::new("finish phase", "in_progress")]
        );
        assert!(
            persisted.steps[0]
                .active_tool
                .as_deref()
                .is_some_and(|tool| tool.contains("bash running"))
        );
        let output = load_output_lines(&conn, &persisted.run.id, 1).unwrap();
        assert!(
            output
                .iter()
                .any(|line| line.kind == PlanOutputKind::Assistant
                    && line.text == "recovered message")
        );
    }

    #[test]
    fn abort_plan_step_marks_step_aborted() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted = PlanLaunch::new(
            &repo,
            &repo,
            &repo.join("plan.md"),
            "phase",
            1,
            1,
            PlanRunMode::Sequential,
        )
        .unwrap()
        .create_run();
        save_plan_run(&conn, &persisted).unwrap();

        abort_plan_step(&conn, &mut persisted.steps[0]).unwrap();

        let loaded = load_plan_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("persisted run");
        assert_eq!(loaded.steps[0].status, PlanStepStatus::Aborted);
        assert_eq!(loaded.steps[0].error.as_deref(), Some("aborted"));
        assert!(loaded.steps[0].finished_unix_ms.is_some());
    }

    #[test]
    fn reconcile_marks_running_steps_failed_after_restart() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted = PlanLaunch::new(
            &repo,
            &repo,
            &repo.join("plan.md"),
            "phase",
            1,
            2,
            PlanRunMode::Sequential,
        )
        .unwrap()
        .create_run();
        persisted.run.status = PlanRunStatus::Running;
        persisted.steps[0].status = PlanStepStatus::Running;
        persisted.steps[0].process_id = None;
        save_plan_run(&conn, &persisted).unwrap();

        let changed =
            reconcile_stale_plan_run(&conn, &mut persisted, DEFAULT_OUTPUT_LINES_PER_STEP).unwrap();

        assert!(changed);
        let loaded = load_plan_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("persisted run");
        assert_eq!(loaded.run.status, PlanRunStatus::Failed);
        assert_eq!(loaded.steps[0].status, PlanStepStatus::Failed);
        assert!(
            loaded.steps[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Prism restarted"))
        );
        assert!(
            load_output_lines(&conn, &persisted.run.id, 1)
                .unwrap()
                .iter()
                .any(|line| line.kind == PlanOutputKind::Error
                    && line.text.contains("Prism restarted"))
        );
    }

    #[test]
    fn reconcile_keeps_running_step_with_live_process() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let temp = unique_temp_dir("prism-plan-live-reconcile");
        let mut persisted = PlanLaunch::new(
            &temp,
            &temp,
            &temp.join("plan.md"),
            "phase",
            1,
            1,
            PlanRunMode::Sequential,
        )
        .unwrap()
        .create_run();
        persisted.run.status = PlanRunStatus::Running;
        persisted.run.selected_step = 1;
        persisted.run.updated_unix_ms = 123;
        persisted.steps[0].status = PlanStepStatus::Running;
        persisted.steps[0].process_id = Some(std::process::id());
        persisted.steps[0].opencode_server_url = Some("http://127.0.0.1:41234".to_string());
        persisted.steps[0].opencode_session_id = Some("ses_live".to_string());
        persisted.steps[0].started_unix_ms = Some(111);
        save_plan_run(&conn, &persisted).unwrap();

        let changed =
            reconcile_stale_plan_run(&conn, &mut persisted, DEFAULT_OUTPUT_LINES_PER_STEP).unwrap();
        let changed_again =
            reconcile_stale_plan_run(&conn, &mut persisted, DEFAULT_OUTPUT_LINES_PER_STEP).unwrap();

        assert!(changed);
        assert!(!changed_again);
        let loaded = load_plan_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("persisted run");
        assert_eq!(loaded.run.status, PlanRunStatus::Running);
        assert_eq!(loaded.run.selected_step, 1);
        assert_eq!(loaded.run.updated_unix_ms, 123);
        assert_eq!(loaded.steps[0].status, PlanStepStatus::Running);
        assert_eq!(loaded.steps[0].process_id, Some(std::process::id()));
        assert_eq!(loaded.steps[0].started_unix_ms, Some(111));
        assert_eq!(loaded.steps[0].finished_unix_ms, None);
        assert_eq!(
            loaded.steps[0].opencode_server_url.as_deref(),
            Some("http://127.0.0.1:41234")
        );
        assert_eq!(
            loaded.steps[0].opencode_session_id.as_deref(),
            Some("ses_live")
        );
        let output = load_output_lines(&conn, &persisted.run.id, 1).unwrap();
        assert_eq!(
            output
                .iter()
                .filter(|line| line.kind == PlanOutputKind::System
                    && line.text.contains("stdout cannot be reattached"))
                .count(),
            1
        );

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn retry_from_step_resets_selected_and_later_steps() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted = PlanLaunch::new(
            &repo,
            &repo,
            &repo.join("plan.md"),
            "phase",
            1,
            3,
            PlanRunMode::Sequential,
        )
        .unwrap()
        .create_run();
        persisted.steps[0].status = PlanStepStatus::Done;
        persisted.steps[1].status = PlanStepStatus::Failed;
        persisted.steps[1].error = Some("failed".to_string());
        persisted.steps[2].status = PlanStepStatus::Skipped;
        save_plan_run(&conn, &persisted).unwrap();

        retry_from_step(&conn, &mut persisted, 2).unwrap();

        let loaded = load_plan_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("persisted run");
        assert_eq!(loaded.steps[0].status, PlanStepStatus::Done);
        assert_eq!(loaded.steps[1].status, PlanStepStatus::Queued);
        assert_eq!(loaded.steps[1].error, None);
        assert_eq!(loaded.steps[2].status, PlanStepStatus::Queued);
        assert_eq!(loaded.run.selected_step, 2);
        assert_eq!(loaded.run.status, PlanRunStatus::Queued);
    }

    #[test]
    fn pause_request_stops_sequential_executor_before_next_queued_step() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let temp = unique_temp_dir("prism-plan-pause-before-next");
        let marker = temp.join("should-not-run");
        let opencode = fake_opencode(
            &temp,
            r#"#!/usr/bin/env bash
set -euo pipefail
touch should-not-run
"#,
        );
        let mut persisted = PlanLaunch::new(
            &temp,
            &temp,
            &temp.join("plan.md"),
            "phase",
            1,
            2,
            PlanRunMode::Sequential,
        )
        .unwrap()
        .create_run();
        persisted.steps[0].status = PlanStepStatus::Done;
        save_plan_run(&conn, &persisted).unwrap();
        request_plan_run_pause(&conn, &mut persisted).unwrap();
        let executor = PlanExecutorConfig::new(
            opencode.display().to_string(),
            None,
            temp.clone(),
            "plan.md",
        );
        let mut output = Vec::new();

        execute_plan_sequential(&conn, &mut persisted, &executor, &mut output).unwrap();

        let loaded = load_plan_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("persisted run");
        assert_eq!(loaded.run.status, PlanRunStatus::Paused);
        assert!(loaded.run.pause_requested);
        assert_eq!(loaded.steps[0].status, PlanStepStatus::Done);
        assert_eq!(loaded.steps[1].status, PlanStepStatus::Queued);
        assert!(!marker.exists());

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn resumable_run_requeues_interrupted_steps_and_preserves_done_steps() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let launch = PlanLaunch::new(
            &repo,
            &repo,
            &repo.join("plan.md"),
            "phase",
            1,
            3,
            PlanRunMode::Sequential,
        )
        .unwrap();
        let mut persisted = launch.create_run();
        persisted.run.status = PlanRunStatus::Running;
        persisted.steps[0].status = PlanStepStatus::Done;
        persisted.steps[1].status = PlanStepStatus::Running;
        persisted.steps[1].process_id = None;
        save_plan_run(&conn, &persisted).unwrap();

        let mut resumed = load_resumable_plan_run(&conn, &launch)
            .unwrap()
            .expect("resumable run");
        let can_execute =
            prepare_plan_run_for_resume(&conn, &mut resumed, DEFAULT_OUTPUT_LINES_PER_STEP)
                .unwrap();

        assert!(can_execute);
        let loaded = load_plan_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("persisted run");
        assert!(!loaded.run.pause_requested);
        assert_eq!(loaded.run.status, PlanRunStatus::Queued);
        assert_eq!(loaded.steps[0].status, PlanStepStatus::Done);
        assert_eq!(loaded.steps[1].status, PlanStepStatus::Queued);
        assert_eq!(loaded.steps[2].status, PlanStepStatus::Queued);
    }

    #[test]
    fn skip_and_archive_plan_run_hide_it_from_recent_runs() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let repo = PathBuf::from("/repo/prism");
        let mut persisted = PlanLaunch::new(
            &repo,
            &repo,
            &repo.join("plan.md"),
            "phase",
            1,
            1,
            PlanRunMode::Sequential,
        )
        .unwrap()
        .create_run();
        save_plan_run(&conn, &persisted).unwrap();

        skip_plan_step(&conn, &mut persisted, 1).unwrap();
        assert_eq!(persisted.run.status, PlanRunStatus::Done);
        archive_plan_run(&conn, &mut persisted).unwrap();

        let recent = load_recent_plan_runs_for_repo(&conn, &repo, 8).unwrap();
        assert!(recent.is_empty());
        let loaded = load_plan_run(&conn, &persisted.run.id)
            .unwrap()
            .expect("archived run remains loadable");
        assert!(loaded.run.archived_unix_ms.is_some());
        let removed = cleanup_stale_archived_plan_runs(&conn, 0).unwrap();
        assert_eq!(removed, 1);
        assert!(load_plan_run(&conn, &persisted.run.id).unwrap().is_none());
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn fake_opencode(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("opencode-shim");
        std::fs::write(&path, body).unwrap();
        make_executable(&path);
        path
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {}
}
