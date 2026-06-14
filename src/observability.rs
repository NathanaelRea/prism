use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, params};

use crate::json::json_escape;
use crate::repo::Repository;
use crate::util::{single_line, truncate};

const RUNTIME_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
const RUNTIME_LOG_RETAINED_FILES: usize = 3;

static OBSERVER: OnceLock<Mutex<ObserverState>> = OnceLock::new();
static PANIC_HOOK_INSTALLED: OnceLock<()> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Error => 1,
            Self::Warn => 2,
            Self::Info => 3,
            Self::Debug => 4,
            Self::Trace => 5,
        }
    }

    fn allows(self, event_level: Self) -> bool {
        event_level.rank() <= self.rank()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ObserverOptions {
    pub log_level: LogLevel,
    pub print_logs: bool,
}

#[derive(Clone, Debug)]
struct Event {
    time_unix_ms: i64,
    level: LogLevel,
    target: String,
    action: String,
    operation_id: Option<String>,
    parent_operation_id: Option<String>,
    repo: Option<String>,
    branch: Option<String>,
    session: Option<String>,
    message: String,
    data_json: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PhaseRecord {
    pub phase: String,
    pub time_started_unix_ms: i64,
    pub time_finished_unix_ms: Option<i64>,
    pub status: String,
    pub error: Option<String>,
    pub elapsed_ms: Option<i64>,
}

#[derive(Clone, Debug)]
struct StoredPhaseRecord {
    record: PhaseRecord,
    persisted: bool,
}

#[derive(Debug)]
struct ObserverState {
    file_level: LogLevel,
    stderr_level: Option<LogLevel>,
    repo_root: Option<PathBuf>,
    prism_dir: Option<PathBuf>,
    db_ready: bool,
    buffered: Vec<Event>,
    next_operation_id: u64,
    startup_run_id: Option<String>,
    phases: Vec<StoredPhaseRecord>,
}

#[derive(Clone, Debug)]
pub struct Operation {
    id: String,
}

impl Operation {
    pub fn finish(
        &self,
        level: LogLevel,
        target: &str,
        action: &str,
        message: impl Into<String>,
        data_json: Option<String>,
    ) {
        emit(EventInput {
            level,
            target,
            action,
            operation_id: Some(self.id.clone()),
            parent_operation_id: None,
            branch: None,
            session: None,
            message: message.into(),
            data_json,
        });
    }
}

pub struct EventInput<'a> {
    pub level: LogLevel,
    pub target: &'a str,
    pub action: &'a str,
    pub operation_id: Option<String>,
    pub parent_operation_id: Option<String>,
    pub branch: Option<String>,
    pub session: Option<String>,
    pub message: String,
    pub data_json: Option<String>,
}

pub fn init(options: ObserverOptions) {
    let mutex = OBSERVER.get_or_init(|| Mutex::new(ObserverState::new(options)));
    if let Ok(mut state) = mutex.lock() {
        state.file_level = options.log_level;
        state.stderr_level = options.print_logs.then_some(options.log_level);
    }
}

pub fn install_panic_hook() {
    PANIC_HOOK_INSTALLED.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            record_panic(info.to_string());
            previous(info);
        }));
    });
}

pub fn attach_repo(repo: &Repository) {
    with_state(|state| {
        state.repo_root = Some(repo.root.clone());
        state.prism_dir = Some(repo.prism_dir());
        let buffered = std::mem::take(&mut state.buffered);
        for mut event in buffered {
            if event.repo.is_none() {
                event.repo = state.repo_string();
            }
            state.write_persistent_event(&event);
        }
    });
}

pub fn db_path(repo: &Repository) -> PathBuf {
    repo.prism_dir().join("prism.db")
}

pub fn runtime_log_path(repo: &Repository) -> PathBuf {
    repo.prism_dir().join("runtime.log")
}

pub fn emit(input: EventInput<'_>) {
    let event = Event {
        time_unix_ms: now_ms(),
        level: input.level,
        target: input.target.to_string(),
        action: input.action.to_string(),
        operation_id: input.operation_id,
        parent_operation_id: input.parent_operation_id,
        repo: None,
        branch: input.branch,
        session: input.session,
        message: input.message,
        data_json: input.data_json,
    };

    with_state(|state| state.record_event(event));
}

pub fn begin_operation(
    level: LogLevel,
    target: &str,
    action: &str,
    message: impl Into<String>,
    data_json: Option<String>,
) -> Operation {
    let id = with_state(|state| state.next_operation_id())
        .unwrap_or_else(|| format!("{}-{}", std::process::id(), now_ms().max(0)));
    emit(EventInput {
        level,
        target,
        action,
        operation_id: Some(id.clone()),
        parent_operation_id: None,
        branch: None,
        session: None,
        message: message.into(),
        data_json,
    });
    Operation { id }
}

pub fn phase<T, F>(phase: &str, run: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String>,
{
    let started_ms = now_ms();
    let started = Instant::now();
    let operation = begin_operation(
        LogLevel::Info,
        "startup",
        "begin",
        format!("begin {phase}"),
        Some(json_object(vec![json_string_field("phase", phase)])),
    );
    let result = run();
    let elapsed_ms = started.elapsed().as_millis() as i64;
    let finished_ms = now_ms();
    match &result {
        Ok(_) => {
            operation.finish(
                LogLevel::Info,
                "startup",
                "end",
                format!("finished {phase}"),
                Some(json_object(vec![
                    json_string_field("phase", phase),
                    json_number_field("elapsed_ms", elapsed_ms),
                    json_string_field("status", "ok"),
                ])),
            );
            record_phase(PhaseRecord {
                phase: phase.to_string(),
                time_started_unix_ms: started_ms,
                time_finished_unix_ms: Some(finished_ms),
                status: "ok".to_string(),
                error: None,
                elapsed_ms: Some(elapsed_ms),
            });
        }
        Err(error) => {
            operation.finish(
                LogLevel::Error,
                "startup",
                "end",
                format!("failed {phase}: {}", truncate(&single_line(error), 300)),
                Some(json_object(vec![
                    json_string_field("phase", phase),
                    json_number_field("elapsed_ms", elapsed_ms),
                    json_string_field("status", "error"),
                    json_string_field("error", &truncate(&single_line(error), 500)),
                ])),
            );
            record_phase(PhaseRecord {
                phase: phase.to_string(),
                time_started_unix_ms: started_ms,
                time_finished_unix_ms: Some(finished_ms),
                status: "error".to_string(),
                error: Some(truncate(&single_line(error), 500)),
                elapsed_ms: Some(elapsed_ms),
            });
        }
    }
    result
}

pub fn start_startup_run(version: &str) -> String {
    let id = format!("startup-{}-{}", std::process::id(), now_ms().max(0));
    with_state(|state| {
        if let Some(previous) = state.previous_incomplete_startup() {
            state.record_event(Event {
                time_unix_ms: now_ms(),
                level: LogLevel::Warn,
                target: "startup".to_string(),
                action: "previous_incomplete".to_string(),
                operation_id: None,
                parent_operation_id: None,
                repo: state.repo_string(),
                branch: None,
                session: None,
                message: format!("previous startup did not finish: {previous}"),
                data_json: Some(json_object(vec![json_string_field("run_id", &previous)])),
            });
        }
        state.startup_run_id = Some(id.clone());
        state.insert_startup_run(&id, version);
        state.persist_unpersisted_phases();
    });
    emit(EventInput {
        level: LogLevel::Info,
        target: "startup",
        action: "run_begin",
        operation_id: Some(id.clone()),
        parent_operation_id: None,
        branch: None,
        session: None,
        message: "startup run began".to_string(),
        data_json: Some(json_object(vec![json_string_field("version", version)])),
    });
    id
}

pub fn finish_startup_run(status: &str, error: Option<&str>) {
    with_state(|state| {
        let Some(run_id) = state.startup_run_id.clone() else {
            return;
        };
        state.update_startup_run(&run_id, status, error);
    });
    emit(EventInput {
        level: if status == "ok" {
            LogLevel::Info
        } else {
            LogLevel::Error
        },
        target: "startup",
        action: "run_end",
        operation_id: None,
        parent_operation_id: None,
        branch: None,
        session: None,
        message: match error {
            Some(error) => format!("startup run finished with {status}: {error}"),
            None => format!("startup run finished with {status}"),
        },
        data_json: Some(json_object(vec![json_string_field("status", status)])),
    });
}

pub fn startup_phases() -> Vec<PhaseRecord> {
    with_state(|state| {
        state
            .phases
            .iter()
            .map(|phase| phase.record.clone())
            .collect()
    })
    .unwrap_or_default()
}

pub fn enabled(level: LogLevel) -> bool {
    with_state(|state| {
        state.file_level.allows(level)
            || state
                .stderr_level
                .is_some_and(|stderr| stderr.allows(level))
    })
    .unwrap_or(false)
}

pub fn command_data_json(
    command: &Command,
    include_argv: bool,
    elapsed_ms: Option<i64>,
    status: Option<&str>,
    stderr: Option<&str>,
) -> String {
    let mut fields = vec![
        json_string_field("program", &os_to_string(command.get_program())),
        json_number_field("arg_count", command.get_args().count() as i64 + 1),
    ];
    if include_argv {
        let argv = sanitized_argv(command);
        fields.push(format!(
            "\"argv\":[{}]",
            argv.iter()
                .map(|arg| format!("\"{}\"", json_escape(arg)))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if let Some(cwd) = command.get_current_dir() {
        fields.push(json_string_field("cwd", &cwd.display().to_string()));
    }
    if let Some(elapsed_ms) = elapsed_ms {
        fields.push(json_number_field("elapsed_ms", elapsed_ms));
    }
    if let Some(status) = status {
        fields.push(json_string_field("status", status));
    }
    if let Some(stderr) = stderr {
        fields.push(json_string_field("stderr", &redact_freeform(stderr, 500)));
    }
    json_object(fields)
}

pub fn command_display(command: &Command) -> String {
    sanitized_argv(command).join(" ")
}

pub fn agent_spawn_data_json(argv: &[String], workdir: &Path) -> String {
    let program = argv.first().cloned().unwrap_or_default();
    json_object(vec![
        json_string_field("program", &sanitize_arg(&program, false)),
        json_number_field("arg_count", argv.len() as i64),
        json_string_field("cwd", &workdir.display().to_string()),
    ])
}

pub fn sanitize_command_text(command: &str) -> String {
    let mut redact_next = false;
    command
        .split_whitespace()
        .map(|part| {
            let sanitized = sanitize_arg(part, redact_next);
            redact_next = is_secret_flag(part);
            sanitized
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn tail_runtime_log(repo: &Repository, lines: usize) -> Result<Vec<String>, String> {
    let path = runtime_log_path(repo);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text =
        fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let mut tail = text
        .lines()
        .rev()
        .take(lines)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    tail.reverse();
    Ok(tail)
}

pub fn append_runtime_message(repo: &Repository, message: &str) -> Result<(), String> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    append_text_line(
        &runtime_log_path(repo),
        &format!("[{seconds}] {}", single_line(message)),
    )
}

pub fn run_readonly_query(repo: &Repository, query: &str) -> Result<(), String> {
    let path = db_path(repo);
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("open {} read-only: {error}", path.display()))?;
    conn.pragma_update(None, "query_only", true)
        .map_err(|error| format!("enable SQLite query_only: {error}"))?;
    let mut statement = conn
        .prepare(query)
        .map_err(|error| format!("prepare query: {error}"))?;
    let column_count = statement.column_count();
    let mut rows = statement
        .query([])
        .map_err(|error| format!("run query: {error}"))?;
    while let Some(row) = rows.next().map_err(|error| format!("read row: {error}"))? {
        let mut values = Vec::new();
        for index in 0..column_count {
            let value = row
                .get_ref(index)
                .map_err(|error| format!("read column {index}: {error}"))?;
            values.push(sqlite_value_to_string(value));
        }
        println!("{}", values.join("\t"));
    }
    Ok(())
}

pub fn with_writable_db<T>(
    repo: &Repository,
    run: impl FnOnce(&Connection) -> Result<T, String>,
) -> Result<T, String> {
    let path = db_path(repo);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create db dir: {error}"))?;
    }
    let conn =
        Connection::open(&path).map_err(|error| format!("open {}: {error}", path.display()))?;
    create_schema(&conn)?;
    run(&conn)
}

fn record_panic(message: String) {
    let event = Event {
        time_unix_ms: now_ms(),
        level: LogLevel::Error,
        target: "process".to_string(),
        action: "panic".to_string(),
        operation_id: None,
        parent_operation_id: None,
        repo: None,
        branch: None,
        session: None,
        message: truncate(&single_line(&message), 500),
        data_json: None,
    };
    if let Some(mutex) = OBSERVER.get()
        && let Ok(mut state) = mutex.try_lock()
    {
        state.record_event(event);
    }
}

fn record_phase(record: PhaseRecord) {
    with_state(|state| {
        state.phases.push(StoredPhaseRecord {
            record,
            persisted: false,
        });
        state.persist_unpersisted_phases();
    });
}

impl ObserverState {
    fn new(options: ObserverOptions) -> Self {
        Self {
            file_level: options.log_level,
            stderr_level: options.print_logs.then_some(options.log_level),
            repo_root: None,
            prism_dir: None,
            db_ready: false,
            buffered: Vec::new(),
            next_operation_id: 0,
            startup_run_id: None,
            phases: Vec::new(),
        }
    }

    fn next_operation_id(&mut self) -> String {
        self.next_operation_id += 1;
        format!("op-{}-{}", std::process::id(), self.next_operation_id)
    }

    fn record_event(&mut self, mut event: Event) {
        if !self.file_level.allows(event.level)
            && !self
                .stderr_level
                .is_some_and(|stderr_level| stderr_level.allows(event.level))
        {
            return;
        }
        if event.repo.is_none() {
            event.repo = self.repo_string();
        }
        if self.prism_dir.is_none() {
            self.write_stderr_if_enabled(&event);
            self.buffered.push(event);
            return;
        }
        self.write_event(&event);
    }

    fn write_event(&mut self, event: &Event) {
        self.write_stderr_if_enabled(event);
        self.write_persistent_event(event);
    }

    fn write_persistent_event(&mut self, event: &Event) {
        if self.file_level.allows(event.level) {
            if let Err(error) = self.write_text_event(event) {
                eprintln!("prism observability: {error}");
            }
            if let Err(error) = self.write_db_event(event) {
                let warning = format!("observability db write failed: {error}");
                let _ = self.append_text_warning(&warning);
                self.write_stderr_warning(&warning);
            }
        }
    }

    fn write_stderr_if_enabled(&self, event: &Event) {
        if self
            .stderr_level
            .is_some_and(|stderr_level| stderr_level.allows(event.level))
        {
            eprintln!("{}", format_text_event(event));
        }
    }

    fn write_stderr_warning(&self, message: &str) {
        if self.stderr_level.is_some() {
            eprintln!("prism observability: {message}");
        }
    }

    fn write_text_event(&self, event: &Event) -> Result<(), String> {
        let Some(prism_dir) = &self.prism_dir else {
            return Ok(());
        };
        let path = prism_dir.join("runtime.log");
        append_text_line(&path, &format_text_event(event))
    }

    fn append_text_warning(&self, message: &str) -> Result<(), String> {
        let Some(prism_dir) = &self.prism_dir else {
            return Ok(());
        };
        let path = prism_dir.join("runtime.log");
        append_text_line(
            &path,
            &format!("[{}] warn observability.db {message}", now_ms()),
        )
    }

    fn write_db_event(&mut self, event: &Event) -> Result<(), String> {
        let Some(prism_dir) = &self.prism_dir else {
            return Ok(());
        };
        let path = prism_dir.join("prism.db");
        let conn = self.open_writable_db(&path)?;
        conn.execute(
            "insert into event (
                time_unix_ms, level, target, action, operation_id, parent_operation_id,
                repo, branch, session, message, data_json
            ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                event.time_unix_ms,
                event.level.label(),
                event.target.as_str(),
                event.action.as_str(),
                event.operation_id.as_deref(),
                event.parent_operation_id.as_deref(),
                event.repo.as_deref(),
                event.branch.as_deref(),
                event.session.as_deref(),
                event.message.as_str(),
                event.data_json.as_deref(),
            ],
        )
        .map_err(|error| format!("insert event: {error}"))?;
        Ok(())
    }

    fn open_writable_db(&mut self, path: &Path) -> Result<Connection, String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("create db dir: {error}"))?;
        }
        let conn =
            Connection::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
        if !self.db_ready {
            create_schema(&conn)?;
            self.db_ready = true;
        }
        Ok(conn)
    }

    fn insert_startup_run(&mut self, run_id: &str, version: &str) {
        let Some(prism_dir) = &self.prism_dir else {
            return;
        };
        let path = prism_dir.join("prism.db");
        let result = (|| -> Result<(), String> {
            let conn = self.open_writable_db(&path)?;
            let repo = self.repo_string();
            conn.execute(
                "insert into startup_run (
                    id, time_started_unix_ms, time_finished_unix_ms, status, repo, version, error
                ) values (?1, ?2, null, 'running', ?3, ?4, null)",
                params![run_id, now_ms(), repo.as_deref(), version,],
            )
            .map_err(|error| format!("insert startup_run: {error}"))?;
            Ok(())
        })();
        if let Err(error) = result {
            let warning = format!("startup run insert failed: {error}");
            let _ = self.append_text_warning(&warning);
            self.write_stderr_warning(&warning);
        }
    }

    fn update_startup_run(&mut self, run_id: &str, status: &str, error: Option<&str>) {
        let Some(prism_dir) = &self.prism_dir else {
            return;
        };
        let path = prism_dir.join("prism.db");
        let result = (|| -> Result<(), String> {
            let conn = self.open_writable_db(&path)?;
            conn.execute(
                "update startup_run
                 set time_finished_unix_ms = ?1, status = ?2, error = ?3
                 where id = ?4",
                params![now_ms(), status, error, run_id],
            )
            .map_err(|error| format!("update startup_run: {error}"))?;
            Ok(())
        })();
        if let Err(error) = result {
            let warning = format!("startup run update failed: {error}");
            let _ = self.append_text_warning(&warning);
            self.write_stderr_warning(&warning);
        }
    }

    fn previous_incomplete_startup(&mut self) -> Option<String> {
        let prism_dir = self.prism_dir.clone()?;
        let path = prism_dir.join("prism.db");
        if !path.exists() {
            return None;
        }
        let conn = self.open_writable_db(&path).ok()?;
        conn.query_row(
            "select id from startup_run
             where status = 'running'
             order by time_started_unix_ms desc
             limit 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
    }

    fn persist_unpersisted_phases(&mut self) {
        let Some(run_id) = self.startup_run_id.clone() else {
            return;
        };
        let Some(prism_dir) = &self.prism_dir else {
            return;
        };
        let path = prism_dir.join("prism.db");
        let conn = match self.open_writable_db(&path) {
            Ok(conn) => conn,
            Err(error) => {
                let warning = format!("startup phase persist failed: {error}");
                let _ = self.append_text_warning(&warning);
                self.write_stderr_warning(&warning);
                return;
            }
        };
        for phase in &mut self.phases {
            if phase.persisted {
                continue;
            }
            let result = conn.execute(
                "insert into startup_phase (
                    run_id, phase, time_started_unix_ms, time_finished_unix_ms, status, error
                ) values (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    run_id.as_str(),
                    phase.record.phase.as_str(),
                    phase.record.time_started_unix_ms,
                    phase.record.time_finished_unix_ms,
                    phase.record.status.as_str(),
                    phase.record.error.as_deref(),
                ],
            );
            match result {
                Ok(_) => phase.persisted = true,
                Err(error) => {
                    let warning = format!("startup phase insert failed: {error}");
                    let _ = self.append_text_warning(&warning);
                    self.write_stderr_warning(&warning);
                    return;
                }
            }
        }
    }

    fn repo_string(&self) -> Option<String> {
        self.repo_root
            .as_ref()
            .map(|path| path.display().to_string())
    }
}

fn with_state<T>(run: impl FnOnce(&mut ObserverState) -> T) -> Option<T> {
    let mutex = OBSERVER.get()?;
    let mut state = mutex.lock().ok()?;
    Some(run(&mut state))
}

fn create_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        create table if not exists metadata (
          key text primary key,
          value text not null
        );

        create table if not exists event (
          id integer primary key autoincrement,
          time_unix_ms integer not null,
          level text not null,
          target text not null,
          action text not null,
          operation_id text,
          parent_operation_id text,
          repo text,
          branch text,
          session text,
          message text not null,
          data_json text
        );

        create index if not exists event_time_idx on event(time_unix_ms);
        create index if not exists event_target_idx on event(target);
        create index if not exists event_action_idx on event(action);
        create index if not exists event_branch_idx on event(branch);
        create index if not exists event_operation_idx on event(operation_id);

        create table if not exists startup_run (
          id text primary key,
          time_started_unix_ms integer not null,
          time_finished_unix_ms integer,
          status text not null,
          repo text,
          version text not null,
          error text
        );

        create table if not exists startup_phase (
          id integer primary key autoincrement,
          run_id text not null references startup_run(id) on delete cascade,
          phase text not null,
          time_started_unix_ms integer not null,
          time_finished_unix_ms integer,
          status text not null,
          error text
        );

        create table if not exists task_metadata (
          branch text primary key,
          prompt_summary text not null,
          initial_prompt text not null,
          worktree text not null,
          updated_unix_ms integer not null
        );

        create table if not exists hidden_session (
          branch text primary key,
          hidden_unix_ms integer not null
        );

        create table if not exists agent_state (
          branch text primary key,
          state text not null,
          updated_unix_ms integer not null
        );

        create table if not exists pr_cache (
          branch text primary key,
          number integer not null,
          title text not null,
          body text not null default '',
          url text not null,
          state text not null,
          review_decision text not null,
          head_ref text not null,
          base_ref text not null,
          head_sha text not null,
          updated_at text not null,
          check_status text not null,
          comment_count integer not null default 0,
          merged integer not null,
          draft integer not null,
          last_refreshed text not null,
          refreshed_unix_ms integer not null
        );
        ",
    )
    .map_err(|error| format!("create schema: {error}"))?;
    if !table_has_column(conn, "pr_cache", "body")? {
        conn.execute(
            "alter table pr_cache add column body text not null default ''",
            [],
        )
        .map_err(|error| format!("migrate pr_cache body column: {error}"))?;
    }
    if !table_has_column(conn, "pr_cache", "comment_count")? {
        conn.execute(
            "alter table pr_cache add column comment_count integer not null default 0",
            [],
        )
        .map_err(|error| format!("migrate pr_cache comment_count column: {error}"))?;
    }
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(|error| format!("enable foreign keys: {error}"))?;
    Ok(())
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, String> {
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

fn append_text_line(path: &Path, line: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create log dir: {error}"))?;
    }
    rotate_runtime_log(path)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    writeln!(file, "{line}").map_err(|error| format!("write {}: {error}", path.display()))
}

fn rotate_runtime_log(path: &Path) -> Result<(), String> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(());
    };
    if metadata.len() < RUNTIME_LOG_MAX_BYTES {
        return Ok(());
    }
    for index in (1..=RUNTIME_LOG_RETAINED_FILES).rev() {
        let rotated = rotated_log_path(path, index);
        if index == RUNTIME_LOG_RETAINED_FILES {
            if rotated.exists() {
                fs::remove_file(&rotated)
                    .map_err(|error| format!("remove {}: {error}", rotated.display()))?;
            }
            continue;
        }
        let next = rotated_log_path(path, index + 1);
        if rotated.exists() {
            fs::rename(&rotated, &next).map_err(|error| {
                format!(
                    "rotate {} to {}: {error}",
                    rotated.display(),
                    next.display()
                )
            })?;
        }
    }
    let first = rotated_log_path(path, 1);
    fs::rename(path, &first)
        .map_err(|error| format!("rotate {} to {}: {error}", path.display(), first.display()))
}

fn rotated_log_path(path: &Path, index: usize) -> PathBuf {
    PathBuf::from(format!("{}.{}", path.display(), index))
}

fn format_text_event(event: &Event) -> String {
    let mut parts = vec![
        format!("[{}]", event.time_unix_ms),
        event.level.label().to_string(),
        format!("{}.{}", event.target, event.action),
    ];
    if let Some(operation_id) = &event.operation_id {
        parts.push(format!("op={operation_id}"));
    }
    if let Some(branch) = &event.branch {
        parts.push(format!("branch={}", single_line(branch)));
    }
    if let Some(session) = &event.session {
        parts.push(format!("session={}", single_line(session)));
    }
    parts.push(single_line(&event.message));
    if let Some(data_json) = &event.data_json {
        parts.push(data_json.clone());
    }
    parts.join(" ")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn sanitized_argv(command: &Command) -> Vec<String> {
    let mut argv = Vec::new();
    argv.push(sanitize_arg(&os_to_string(command.get_program()), false));
    let mut redact_next = false;
    for arg in command.get_args() {
        let text = os_to_string(arg);
        argv.push(sanitize_arg(&text, redact_next));
        redact_next = is_secret_flag(&text);
    }
    argv
}

fn sanitize_arg(arg: &str, redact: bool) -> String {
    if redact {
        return "<redacted>".to_string();
    }
    let lower = arg.to_ascii_lowercase();
    if lower.contains("prism-prompts/prompt-") {
        return "<prompt-file>".to_string();
    }
    if arg.chars().any(char::is_whitespace) {
        let sanitized = sanitize_command_text(arg);
        if sanitized != arg {
            return sanitized;
        }
    }
    for flag in secret_flags() {
        if lower == *flag {
            return arg.to_string();
        }
        if let Some((name, _)) = lower.split_once('=')
            && name == *flag
        {
            return format!("{flag}=<redacted>");
        }
    }
    if lower.contains("token=")
        || lower.contains("api_key=")
        || lower.contains("apikey=")
        || lower.contains("password=")
        || lower.contains("secret=")
        || looks_like_secret(arg)
        || arg.contains('\n')
        || arg.chars().count() > 120
    {
        return "<redacted>".to_string();
    }
    single_line(arg)
}

fn is_secret_flag(arg: &str) -> bool {
    let lower = arg.to_ascii_lowercase();
    secret_flags().iter().any(|flag| lower == **flag)
}

fn secret_flags() -> &'static [&'static str] {
    &[
        "--token",
        "--api-key",
        "--apikey",
        "--password",
        "--secret",
        "--auth",
        "--github-token",
        "--prompt",
        "--prompt-file",
    ]
}

fn redact_freeform(value: &str, max_chars: usize) -> String {
    let redacted = value
        .split_whitespace()
        .map(|word| {
            if looks_like_secret(word) {
                "<redacted>".to_string()
            } else {
                sanitize_arg(word, false)
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    truncate(&single_line(&redacted), max_chars)
}

fn looks_like_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.starts_with("sk-")
        || lower.starts_with("ghp_")
        || lower.starts_with("github_pat_")
        || lower.starts_with("xoxb-")
        || lower.starts_with("xoxp-")
}

fn os_to_string(value: &OsStr) -> String {
    value.to_string_lossy().to_string()
}

fn json_object(fields: Vec<String>) -> String {
    format!("{{{}}}", fields.join(","))
}

fn json_string_field(key: &str, value: &str) -> String {
    format!("\"{}\":\"{}\"", json_escape(key), json_escape(value))
}

fn json_number_field(key: &str, value: i64) -> String {
    format!("\"{}\":{}", json_escape(key), value)
}

fn sqlite_value_to_string(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Null => String::new(),
        ValueRef::Integer(value) => value.to_string(),
        ValueRef::Real(value) => value.to_string(),
        ValueRef::Text(value) => single_line(&String::from_utf8_lossy(value)),
        ValueRef::Blob(value) => format!("<blob {} bytes>", value.len()),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::{sanitize_command_text, sanitized_argv};

    #[test]
    fn sanitizes_secret_command_arguments() {
        let mut command = Command::new("gh");
        command.args(["api", "--token", "ghp_secret", "--api-key=abc", "ok"]);

        let argv = sanitized_argv(&command);

        assert_eq!(
            argv,
            vec![
                "gh",
                "api",
                "--token",
                "<redacted>",
                "--api-key=<redacted>",
                "ok"
            ]
        );
    }

    #[test]
    fn sanitizes_prompt_like_command_arguments() {
        let text = sanitize_command_text("agent --prompt hello --password hunter2");

        assert_eq!(text, "agent --prompt <redacted> --password <redacted>");
    }

    #[test]
    fn sanitizes_secret_flags_inside_shell_fragment_arguments() {
        let mut command = Command::new("tmux");
        command.arg("agent --token ghp_secret");

        let argv = sanitized_argv(&command);

        assert_eq!(argv, vec!["tmux", "agent --token <redacted>"]);
    }
}
