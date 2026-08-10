#[cfg(test)]
use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::json::json_escape;
use crate::repo::Repository;
use crate::util::{single_line, truncate};

const RUNTIME_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
const RUNTIME_LOG_RETAINED_FILES: usize = 3;
const DEFERRED_DB_EVENT_CAPACITY: usize = 128;

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
    buffered: Vec<Event>,
    next_operation_id: u64,
    startup_run_id: Option<String>,
    phases: Vec<StoredPhaseRecord>,
    deferred_db_events: Vec<Event>,
    deferred_db_overflow_total: u64,
    deferred_db_overflow_pending: u64,
    database_writes_disabled: bool,
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct CapturedEvent {
    pub target: String,
    pub action: String,
    pub data_json: Option<String>,
}

#[cfg(test)]
thread_local! {
    static CAPTURED_EVENTS: RefCell<Vec<CapturedEvent>> = const { RefCell::new(Vec::new()) };
    static DATABASE_ACCESS_FORBIDDEN: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn deny_database_access_on_current_thread<T>(operation: impl FnOnce() -> T) -> T {
    struct Reset(bool);

    impl Drop for Reset {
        fn drop(&mut self) {
            DATABASE_ACCESS_FORBIDDEN.with(|forbidden| forbidden.set(self.0));
        }
    }

    let previous = DATABASE_ACCESS_FORBIDDEN.with(|forbidden| forbidden.replace(true));
    let _reset = Reset(previous);
    operation()
}

#[cfg(test)]
fn assert_database_access_allowed() {
    DATABASE_ACCESS_FORBIDDEN.with(|forbidden| {
        assert!(
            !forbidden.get(),
            "database access is forbidden on this thread"
        );
    });
}

#[cfg(not(test))]
fn assert_database_access_allowed() {}

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

pub fn attach_repo(repo: &Repository) -> Result<(), String> {
    with_state(|state| {
        state.repo_root = Some(repo.root.clone());
        state.prism_dir = Some(repo.prism_dir());
        state.database_writes_disabled = true;
    });
    let outcome = crate::run_marker::begin(repo, env!("CARGO_PKG_VERSION"))?;
    with_state(|state| {
        state.startup_run_id = Some(outcome.run_id.clone());
        state.database_writes_disabled = false;
        let buffered = std::mem::take(&mut state.buffered);
        for mut event in buffered {
            if event.repo.is_none() {
                event.repo = state.repo_string();
            }
            state.write_persistent_event(&event);
        }
        state.persist_unpersisted_phases();
    });
    start_startup_run(&outcome.run_id, env!("CARGO_PKG_VERSION"));
    for stale_run_id in outcome.stale_run_ids {
        emit(EventInput {
            level: LogLevel::Warn,
            target: "startup",
            action: "previous_incomplete",
            operation_id: None,
            parent_operation_id: None,
            branch: None,
            session: None,
            message: format!(
                "stale prior run {stale_run_id} passed read-only quick_check and foreign_key_check"
            ),
            data_json: Some(json_object(vec![json_string_field(
                "run_id",
                &stale_run_id,
            )])),
        });
    }
    Ok(())
}

pub(crate) fn attach_run_repo(repo: &Repository) -> Result<(), String> {
    let outcome = crate::run_marker::begin(repo, env!("CARGO_PKG_VERSION"))?;
    for stale_run_id in outcome.stale_run_ids {
        let _ = append_runtime_message(
            repo,
            &format!(
                "warn startup.previous_incomplete stale prior run {stale_run_id} passed read-only quick_check and foreign_key_check"
            ),
        );
    }
    Ok(())
}

pub fn db_path(repo: &Repository) -> PathBuf {
    repo.prism_dir().join("prism.db")
}

pub fn runtime_log_path(repo: &Repository) -> PathBuf {
    repo.prism_dir().join("runtime.log")
}

pub fn emit(input: EventInput<'_>) {
    let event = event_from_input(input);
    capture_event(&event);
    with_state(|state| state.record_event(event));
}

/// Records runtime evidence and queues the SQLite copy without opening the database.
/// Reliability-sensitive callers use this path to avoid a synchronous writer wait.
pub(crate) fn emit_deferred(input: EventInput<'_>) {
    let event = event_from_input(input);
    capture_event(&event);
    with_state(|state| state.record_deferred_event(event));
}

pub(crate) fn flush_deferred_events() {
    // Callers invoke this only after leaving their writer operation and dropping
    // its connection. ObserverState methods must never recurse through this lock.
    with_state(ObserverState::flush_deferred_db_events);
}

fn event_from_input(input: EventInput<'_>) -> Event {
    Event {
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
    }
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

fn start_startup_run(id: &str, version: &str) {
    emit(EventInput {
        level: LogLevel::Info,
        target: "startup",
        action: "run_begin",
        operation_id: Some(id.to_string()),
        parent_operation_id: None,
        branch: None,
        session: None,
        message: "startup run began".to_string(),
        data_json: Some(json_object(vec![json_string_field("version", version)])),
    });
}

pub fn finish_process_runs(status: &str, error: Option<&str>) {
    let has_attached_run = with_state(|state| state.startup_run_id.is_some()).unwrap_or(false);
    if has_attached_run {
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
                Some(error) => format!("process run finished with {status}: {error}"),
                None => format!("process run finished with {status}"),
            },
            data_json: Some(json_object(vec![json_string_field("status", status)])),
        });
    }
    for finish_error in crate::run_marker::finish_all(status, error) {
        with_state(|state| {
            let warning = format!("run marker completion failed: {finish_error}");
            let _ = state.append_text_warning(&warning);
            state.write_stderr_warning(&warning);
        });
    }
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
    let mut fields = command_data_fields(command, include_argv);
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

pub struct ProcessExecutionObservation<'a> {
    pub policy: &'a str,
    pub elapsed_ms: i64,
    pub deadline_ms: i64,
    pub child_pid: u32,
    pub process_group: Option<u32>,
    pub status: &'a str,
    pub completion: &'a str,
    pub termination_stage: &'a str,
    pub stdout_bytes: u64,
    pub stdout_truncated: bool,
    pub stderr_bytes: u64,
    pub stderr_truncated: bool,
    pub error: Option<&'a str>,
}

pub fn process_start_data_json(
    command: &Command,
    include_argv: bool,
    policy: &str,
    deadline_ms: i64,
) -> String {
    let mut fields = command_data_fields(command, include_argv);
    fields.push(json_string_field("policy", policy));
    fields.push(json_number_field("deadline_ms", deadline_ms));
    json_object(fields)
}

pub fn process_data_json(
    command: &Command,
    include_argv: bool,
    observation: ProcessExecutionObservation<'_>,
) -> String {
    let mut fields = command_data_fields(command, include_argv);
    fields.extend([
        json_string_field("policy", observation.policy),
        json_number_field("elapsed_ms", observation.elapsed_ms),
        json_number_field("deadline_ms", observation.deadline_ms),
        json_number_field("child_pid", i64::from(observation.child_pid)),
        json_string_field("status", observation.status),
        json_string_field("completion", observation.completion),
        json_string_field("termination_stage", observation.termination_stage),
        json_number_field(
            "stdout_bytes",
            observation.stdout_bytes.min(i64::MAX as u64) as i64,
        ),
        format!("\"stdout_truncated\":{}", observation.stdout_truncated),
        json_number_field(
            "stderr_bytes",
            observation.stderr_bytes.min(i64::MAX as u64) as i64,
        ),
        format!("\"stderr_truncated\":{}", observation.stderr_truncated),
    ]);
    if let Some(process_group) = observation.process_group {
        fields.push(json_number_field("process_group", i64::from(process_group)));
    }
    if let Some(error) = observation.error {
        fields.push(json_string_field("error", &redact_freeform(error, 500)));
    }
    json_object(fields)
}

pub(crate) fn process_error_data_json(
    command: &Command,
    include_argv: bool,
    policy: &str,
    elapsed_ms: i64,
    deadline_ms: i64,
    category: &str,
    error: &str,
) -> String {
    let mut fields = command_data_fields(command, include_argv);
    fields.extend([
        json_string_field("policy", policy),
        json_number_field("elapsed_ms", elapsed_ms),
        json_number_field("deadline_ms", deadline_ms),
        json_string_field("outcome", "supervision_error"),
        json_string_field("error_category", category),
        json_string_field("termination_stage", "none"),
        json_number_field("stdout_bytes", 0),
        json_number_field("stderr_bytes", 0),
        json_string_field("error", &redact_freeform(error, 500)),
    ]);
    json_object(fields)
}

pub(crate) struct JobObservation<'a> {
    pub job_id: u64,
    pub kind: &'a str,
    pub key: &'a str,
    pub generation: u64,
    pub outcome: &'a str,
    pub elapsed_ms: i64,
    pub deadline_ms: Option<i64>,
    pub error: Option<&'a str>,
}

pub(crate) fn job_data_json(observation: JobObservation<'_>) -> String {
    let mut fields = vec![
        json_u64_field("job_id", observation.job_id),
        json_string_field("kind", observation.kind),
        json_string_field("key", &redact_freeform(observation.key, 500)),
        json_u64_field("generation", observation.generation),
        json_string_field("outcome", observation.outcome),
        json_number_field("elapsed_ms", observation.elapsed_ms),
    ];
    if let Some(deadline_ms) = observation.deadline_ms {
        fields.push(json_number_field("deadline_ms", deadline_ms));
    }
    if let Some(error) = observation.error {
        fields.push(json_string_field("error", &redact_freeform(error, 500)));
    }
    json_object(fields)
}

pub(crate) fn storage_error_data_json(
    kind: &str,
    primary_code: Option<i32>,
    extended_code: Option<i32>,
    busy_ms: Option<i64>,
) -> String {
    let mut fields = vec![json_string_field("kind", kind)];
    if let Some(code) = primary_code {
        fields.push(json_number_field("primary_code", i64::from(code)));
    }
    if let Some(code) = extended_code {
        fields.push(json_number_field("extended_code", i64::from(code)));
    }
    if let Some(busy_ms) = busy_ms {
        fields.push(json_number_field("busy_ms", busy_ms));
    }
    json_object(fields)
}

pub(crate) fn wal_growth_data_json(
    main_bytes: u64,
    wal_bytes: u64,
    shm_bytes: u64,
    warning_bytes: u64,
    warning_bucket: u64,
) -> String {
    json_object(vec![
        json_u64_field("main_bytes", main_bytes),
        json_u64_field("wal_bytes", wal_bytes),
        json_u64_field("shm_bytes", shm_bytes),
        json_u64_field("warning_bytes", warning_bytes),
        json_u64_field("warning_bucket", warning_bucket),
    ])
}

pub(crate) fn persistence_data_json(
    category: &str,
    stage: &str,
    committed: bool,
    durability: &str,
    error: Option<&str>,
) -> String {
    let mut fields = vec![
        json_string_field("category", category),
        json_string_field("stage", stage),
        json_bool_field("committed", committed),
        json_string_field("durability", durability),
    ];
    if let Some(error) = error {
        fields.push(json_string_field("error", &redact_freeform(error, 500)));
    }
    json_object(fields)
}

pub(crate) fn shutdown_data_json(
    reason: &str,
    active_jobs: usize,
    unfinished_jobs: usize,
    elapsed_ms: i64,
) -> String {
    json_object(vec![
        json_string_field("reason", reason),
        json_usize_field("active_jobs", active_jobs),
        json_usize_field("unfinished_jobs", unfinished_jobs),
        json_number_field("elapsed_ms", elapsed_ms),
    ])
}

fn command_data_fields(command: &Command, include_argv: bool) -> Vec<String> {
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
    fields
}

pub fn command_display(command: &Command) -> String {
    sanitized_argv(command).join(" ")
}

#[allow(dead_code)]
pub fn agent_spawn_data_json(argv: &[String], workdir: &Path) -> String {
    let program = argv.first().cloned().unwrap_or_default();
    json_object(vec![
        json_string_field("program", &sanitize_arg(&program, false)),
        json_number_field("arg_count", argv.len() as i64),
        json_string_field("cwd", &workdir.display().to_string()),
    ])
}

pub fn sanitize_command_text(command: &str) -> String {
    sanitize_token_sequence(command)
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
    for values in crate::persistence::database::run_operator_query(&path, query)? {
        println!("{}", values.join("\t"));
    }
    Ok(())
}

#[track_caller]
pub fn with_writable_db<T>(
    repo: &Repository,
    run: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    let caller = std::panic::Location::caller();
    writable_db(repo).run_observed(&format!("{}:{}", caller.file(), caller.line()), run)
}

#[track_caller]
#[cfg(test)]
pub fn with_nonblocking_read_db<T>(
    repo: &Repository,
    run: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    let caller = std::panic::Location::caller();
    with_nonblocking_read_db_observed(repo, &format!("{}:{}", caller.file(), caller.line()), run)
}

pub fn with_nonblocking_read_db_named<T>(
    repo: &Repository,
    operation: &'static str,
    run: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    with_nonblocking_read_db_observed(repo, operation, run)
}

fn with_nonblocking_read_db_observed<T>(
    repo: &Repository,
    operation: &str,
    run: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    assert_database_access_allowed();
    let started = Instant::now();
    let open_started = Instant::now();
    let path = db_path(repo);
    let opened = open_observed_readonly_db_path(&path);
    let open_elapsed = open_started.elapsed();
    match opened {
        Ok(()) => {}
        Err(error) => {
            record_db_operation(
                "readonly",
                operation,
                started.elapsed(),
                open_elapsed,
                Some(&error),
            );
            return Err(error);
        }
    }
    let result = run(&path);
    record_db_operation(
        "readonly",
        operation,
        started.elapsed(),
        open_elapsed,
        result.as_ref().err().map(String::as_str),
    );
    result
}

pub fn writable_db(repo: &Repository) -> WritableDb {
    WritableDb {
        path: db_path(repo),
    }
}

#[derive(Clone, Debug)]
pub struct WritableDb {
    path: PathBuf,
}

impl WritableDb {
    #[cfg(test)]
    pub fn run<T>(&self, run: impl FnOnce(&Path) -> Result<T, String>) -> Result<T, String> {
        self.run_observed("test", run)
    }

    fn run_observed<T>(
        &self,
        operation: &str,
        run: impl FnOnce(&Path) -> Result<T, String>,
    ) -> Result<T, String> {
        assert_database_access_allowed();
        let started = Instant::now();
        let open_started = Instant::now();
        let opened = open_observed_writable_db_path(&self.path);
        let open_elapsed = open_started.elapsed();
        let path = match opened {
            Ok(path) => path,
            Err(error) => {
                record_db_operation(
                    "writable",
                    operation,
                    started.elapsed(),
                    open_elapsed,
                    Some(&error),
                );
                return Err(error);
            }
        };
        let result = run(&path);
        record_db_operation(
            "writable",
            operation,
            started.elapsed(),
            open_elapsed,
            result.as_ref().err().map(String::as_str),
        );
        if result.is_ok() {
            crate::storage::monitor_wal_growth(&self.path);
            flush_deferred_events();
        }
        result
    }
}

fn record_db_operation(
    access: &'static str,
    operation: &str,
    total: std::time::Duration,
    open: std::time::Duration,
    error: Option<&str>,
) {
    let query_or_tx = total.saturating_sub(open);
    let mut fields = vec![
        crate::flight_recorder::text("name", operation),
        crate::flight_recorder::text("access", access),
        crate::flight_recorder::unsigned("open_us", open.as_micros()),
        crate::flight_recorder::unsigned("query_or_tx_us", query_or_tx.as_micros()),
        crate::flight_recorder::boolean("success", error.is_none()),
    ];
    if let Some(error) = error {
        let lower = error.to_ascii_lowercase();
        let kind = if lower.contains("busy") {
            "busy"
        } else if lower.contains("locked") {
            "locked"
        } else {
            "other"
        };
        fields.push(crate::flight_recorder::text("error_kind", kind));
        if matches!(kind, "busy" | "locked") {
            fields.push(crate::flight_recorder::unsigned(
                "busy_wait_upper_bound_us",
                query_or_tx.as_micros(),
            ));
        }
    }
    crate::flight_recorder::record("sqlite", "operation", Some(total), fields);
}

fn open_observed_writable_db_path(path: &Path) -> Result<PathBuf, String> {
    crate::storage::prepare_writable(path)
        .map(|()| path.to_path_buf())
        .map_err(|error| {
            crate::storage::record_storage_error(&error);
            error.to_string()
        })
}

fn open_observed_readonly_db_path(path: &Path) -> Result<(), String> {
    crate::storage::verify_readonly(path).map_err(|error| {
        crate::storage::record_storage_error(&error);
        error.to_string()
    })
}

#[cfg(test)]
fn open_writable_db_path(path: &Path) -> Result<PathBuf, String> {
    // Observer persistence calls this while holding OBSERVER. Keep it unobserved
    // and never flush deferred events from this path.
    crate::storage::prepare_writable(path)
        .map(|()| path.to_path_buf())
        .map_err(|error| error.to_string())
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
            buffered: Vec::new(),
            next_operation_id: 0,
            startup_run_id: None,
            phases: Vec::new(),
            deferred_db_events: Vec::new(),
            deferred_db_overflow_total: 0,
            deferred_db_overflow_pending: 0,
            database_writes_disabled: false,
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

    fn record_deferred_event(&mut self, mut event: Event) {
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
        self.write_stderr_if_enabled(&event);
        if self.file_level.allows(event.level) {
            if let Err(error) = self.write_text_event(&event) {
                eprintln!("prism observability: {error}");
            }
            if self.deferred_db_events.len() == DEFERRED_DB_EVENT_CAPACITY {
                self.deferred_db_events.remove(0);
                self.deferred_db_overflow_total = self.deferred_db_overflow_total.saturating_add(1);
                self.deferred_db_overflow_pending =
                    self.deferred_db_overflow_pending.saturating_add(1);
                if self.deferred_db_overflow_total.is_power_of_two() {
                    let warning = format!(
                        "deferred event queue overflow: {} event(s) omitted from the SQLite backlog; runtime events remain recorded; capacity={DEFERRED_DB_EVENT_CAPACITY}",
                        self.deferred_db_overflow_total
                    );
                    let _ = self.append_text_warning(&warning);
                    self.write_stderr_warning(&warning);
                }
            }
            self.deferred_db_events.push(event);
        }
    }

    fn flush_deferred_db_events(&mut self) {
        let pending = std::mem::take(&mut self.deferred_db_events);
        let mut remaining = pending.into_iter();
        while let Some(event) = remaining.next() {
            if let Err(error) = self.write_db_event(&event) {
                self.deferred_db_events.push(event);
                self.deferred_db_events.extend(remaining);
                let warning = format!("deferred observability db write failed: {error}");
                let _ = self.append_text_warning(&warning);
                self.write_stderr_warning(&warning);
                break;
            }
        }
        if self.deferred_db_events.is_empty() && self.deferred_db_overflow_pending > 0 {
            let event = self.deferred_overflow_event();
            match self.write_db_event(&event) {
                Ok(()) => self.deferred_db_overflow_pending = 0,
                Err(error) => {
                    let warning = format!("deferred overflow event db write failed: {error}");
                    let _ = self.append_text_warning(&warning);
                    self.write_stderr_warning(&warning);
                }
            }
        }
    }

    fn deferred_overflow_event(&self) -> Event {
        Event {
            time_unix_ms: now_ms(),
            level: LogLevel::Warn,
            target: "observability".to_string(),
            action: "deferred_overflow".to_string(),
            operation_id: None,
            parent_operation_id: None,
            repo: self.repo_string(),
            branch: None,
            session: None,
            message: format!(
                "{} deferred event(s) were omitted from the SQLite backlog; runtime events remain recorded",
                self.deferred_db_overflow_pending
            ),
            data_json: Some(json_object(vec![
                json_u64_field("overflow_count", self.deferred_db_overflow_pending),
                json_u64_field("overflow_total", self.deferred_db_overflow_total),
                json_usize_field("capacity", DEFERRED_DB_EVENT_CAPACITY),
            ])),
        }
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
            if !self.database_writes_disabled
                && let Err(error) = self.write_db_event(event)
            {
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
        crate::persistence::observability::ObservabilityStore::open(&path)
            .and_then(|store| {
                store.insert_event(&crate::persistence::observability::EventRecord {
                    time_unix_ms: event.time_unix_ms,
                    level: event.level.label(),
                    target: event.target.as_str(),
                    action: event.action.as_str(),
                    operation_id: event.operation_id.as_deref(),
                    parent_operation_id: event.parent_operation_id.as_deref(),
                    repo: event.repo.as_deref(),
                    branch: event.branch.as_deref(),
                    session: event.session.as_deref(),
                    message: event.message.as_str(),
                    data_json: event.data_json.as_deref(),
                })
            })
            .map_err(|error| format!("insert event: {error}"))
    }

    fn persist_unpersisted_phases(&mut self) {
        let Some(run_id) = self.startup_run_id.clone() else {
            return;
        };
        let Some(prism_dir) = &self.prism_dir else {
            return;
        };
        let path = prism_dir.join("prism.db");
        let store = match crate::persistence::observability::ObservabilityStore::open(&path) {
            Ok(store) => store,
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
            let result =
                store.insert_phase(&crate::persistence::observability::StartupPhaseRecord {
                    run_id: run_id.as_str(),
                    phase: phase.record.phase.as_str(),
                    time_started_unix_ms: phase.record.time_started_unix_ms,
                    time_finished_unix_ms: phase.record.time_finished_unix_ms,
                    status: phase.record.status.as_str(),
                    error: phase.record.error.as_deref(),
                });
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

fn append_text_line(path: &Path, line: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create log dir: {error}"))?;
        #[cfg(windows)]
        crate::system::windows_security::secure_path(parent, true)
            .map_err(|error| format!("secure log dir: {error}"))?;
    }
    rotate_runtime_log(path)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    #[cfg(windows)]
    crate::system::windows_security::secure_path(path, false)
        .map_err(|error| format!("secure {}: {error}", path.display()))?;
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
        argv.push(sanitize_sequence_part(&text, &mut redact_next));
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
    if lower.contains("http://") || lower.contains("https://") {
        return "<redacted-url>".to_string();
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
        || contains_sensitive_header(&lower)
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
        "--gitlab-token",
        "--forgejo-token",
        "--private-token",
        "--access-token",
        "--oauth-token",
        "--prompt",
        "--prompt-file",
    ]
}

pub(crate) fn redact_freeform(value: &str, max_chars: usize) -> String {
    let redacted = sanitize_token_sequence(value);
    truncate(&single_line(&redacted), max_chars)
}

fn looks_like_secret(value: &str) -> bool {
    let lower = value
        .trim_start_matches(|character: char| !character.is_ascii_alphanumeric())
        .to_ascii_lowercase();
    lower.starts_with("sk-")
        || lower.starts_with("ghp_")
        || lower.starts_with("github_pat_")
        || lower.contains("glpat-")
        || lower.starts_with("xoxb-")
        || lower.starts_with("xoxp-")
}

fn sanitize_token_sequence(value: &str) -> String {
    let mut redact_next = false;
    value
        .split_whitespace()
        .map(|part| sanitize_sequence_part(part, &mut redact_next))
        .collect::<Vec<_>>()
        .join(" ")
}

fn sanitize_sequence_part(part: &str, redact_next: &mut bool) -> String {
    if *redact_next {
        if is_authorization_scheme(part) {
            return sanitize_arg(part, false);
        }
        *redact_next = false;
        return "<redacted>".to_string();
    }
    let sanitized = sanitize_arg(part, false);
    *redact_next = secret_value_follows(part);
    sanitized
}

fn secret_value_follows(value: &str) -> bool {
    if is_secret_flag(value) || is_authorization_scheme(value) {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    for name in ["authorization", "private-token", "private_token"] {
        if let Some(index) = lower.find(name) {
            let remainder = lower[index + name.len()..]
                .trim_matches(|character: char| matches!(character, ':' | '=' | '"' | '\''));
            return remainder.is_empty() || is_authorization_scheme(remainder);
        }
    }
    false
}

fn is_authorization_scheme(value: &str) -> bool {
    matches!(
        value
            .trim_matches(|character: char| !character.is_ascii_alphanumeric())
            .to_ascii_lowercase()
            .as_str(),
        "bearer" | "token" | "basic"
    )
}

fn contains_sensitive_header(lower: &str) -> bool {
    [
        "authorization:",
        "authorization=",
        "private-token:",
        "private-token=",
        "private_token:",
        "private_token=",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
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

fn json_u64_field(key: &str, value: u64) -> String {
    format!("\"{}\":{}", json_escape(key), value)
}

fn json_usize_field(key: &str, value: usize) -> String {
    format!("\"{}\":{}", json_escape(key), value)
}

fn json_bool_field(key: &str, value: bool) -> String {
    format!("\"{}\":{}", json_escape(key), value)
}

#[cfg(test)]
fn capture_event(event: &Event) {
    CAPTURED_EVENTS.with(|events| {
        events.borrow_mut().push(CapturedEvent {
            target: event.target.clone(),
            action: event.action.clone(),
            data_json: event.data_json.clone(),
        });
    });
}

#[cfg(not(test))]
fn capture_event(_event: &Event) {}

#[cfg(test)]
pub(crate) fn take_captured_events() -> Vec<CapturedEvent> {
    CAPTURED_EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    use crate::repo::Repository;

    use super::{
        DEFERRED_DB_EVENT_CAPACITY, Event, LogLevel, ObserverOptions, ObserverState, db_path,
        now_ms, open_writable_db_path, redact_freeform, sanitize_command_text, sanitized_argv,
        writable_db,
    };

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
    fn nonblocking_read_returns_committed_snapshot_promptly_while_writer_is_active() {
        let temp = test_path("nonblocking-read-writer");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        super::with_writable_db(&repo, |path| {
            execute_at(
                path,
                "create table read_probe (value text not null);\
                 insert into read_probe (value) values ('committed');",
            )
            .map_err(|error| error.to_string())
        })
        .unwrap();
        let mut blocker =
            crate::persistence::database::TestConnection::open_writable(&super::db_path(&repo))
                .unwrap();
        execute_on(
            &mut blocker,
            "begin immediate;\
                 update read_probe set value = 'uncommitted';",
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(0);
        let reader_repo = repo.clone();
        let reader = std::thread::spawn(move || {
            let result = super::with_nonblocking_read_db(&reader_repo, |path| {
                scalar_string(path, "select value from read_probe")
            });
            let _ = tx.send(result);
        });

        let result = rx.recv_timeout(Duration::from_secs(1));
        execute_on(&mut blocker, "rollback").unwrap();
        reader.join().unwrap();

        assert_eq!(result.unwrap().unwrap(), "committed");
        fs::remove_dir_all(temp).unwrap();
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

    #[test]
    fn redacts_provider_tokens_headers_and_query_parameters() {
        let secrets = [
            "glpat-direct-secret",
            "gitlab-bearer-secret",
            "gitlab-private-header-secret",
            "forgejo-token-secret",
            "query-token-secret",
        ];
        let text = format!(
            "token={} Authorization: Bearer {} PRIVATE-TOKEN: {} token {} https://gitlab.example/api?access_token={}&page=1",
            secrets[0], secrets[1], secrets[2], secrets[3], secrets[4]
        );

        let redacted = redact_freeform(&text, 1_000);

        for secret in secrets {
            assert!(!redacted.contains(secret), "secret survived: {secret}");
        }
        assert!(!redacted.contains("https://gitlab.example"));
        assert!(redacted.contains("<redacted-url>"));
        assert!(redacted.matches("<redacted>").count() >= 5);
    }

    #[test]
    fn redacts_authorization_forms_in_command_text() {
        let secrets = [
            "inline-bearer-secret",
            "separate-bearer-secret",
            "private-token-secret",
            "glpat-command-secret",
        ];
        let command = format!(
            "curl -H Authorization:Bearer {} -H 'Authorization: Bearer {}' -H PRIVATE-TOKEN: {} --gitlab-token {}",
            secrets[0], secrets[1], secrets[2], secrets[3]
        );

        let redacted = sanitize_command_text(&command);

        for secret in secrets {
            assert!(!redacted.contains(secret), "secret survived: {secret}");
        }
    }

    #[test]
    fn redacts_authorization_header_split_across_argv() {
        let secret = "split-argv-bearer-secret";
        let mut command = Command::new("curl");
        command.args(["-H", "Authorization:", "Bearer", secret, "safe"]);

        let argv = sanitized_argv(&command);

        assert!(!argv.join(" ").contains(secret));
        assert_eq!(argv.last().map(String::as_str), Some("safe"));
    }

    #[test]
    fn writable_db_exposes_repo_db_path_and_initializes_schema() {
        let root = test_path("writable-db-repo");
        let config_dir = test_path("writable-db-config");
        let _ = fs::remove_dir_all(&config_dir);
        let repo = Repository::with_config_dir_for_test(root, config_dir.clone());
        let db = writable_db(&repo);

        let path = db_path(&repo);
        assert_eq!(path, repo.prism_dir().join("prism.db"));

        db.run(|path| {
            crate::persistence::database::upsert_metadata(path, "phase", "six")
                .map_err(|error| format!("insert metadata: {error}"))
        })
        .unwrap();

        assert!(table_exists(&path, "metadata"));
        let _ = fs::remove_dir_all(config_dir);
    }

    #[test]
    fn writable_db_initializes_schema_for_each_path() {
        let base = test_path("writable-db-multi-path");
        let _ = fs::remove_dir_all(&base);
        let first = base.join("one").join("prism.db");
        let second = base.join("two").join("prism.db");

        open_writable_db_path(&first).unwrap();
        open_writable_db_path(&second).unwrap();

        assert!(table_exists(&first, "event"));
        assert!(table_exists(&second, "event"));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn deferred_queue_overflow_is_bounded_and_keeps_runtime_evidence() {
        let dir = test_path("deferred-overflow");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut state = ObserverState::new(ObserverOptions {
            log_level: LogLevel::Debug,
            print_logs: false,
        });
        state.prism_dir = Some(dir.clone());
        let overflow = 5;

        for index in 0..DEFERRED_DB_EVENT_CAPACITY + overflow {
            state.record_deferred_event(Event {
                time_unix_ms: index as i64,
                level: LogLevel::Error,
                target: "deferred_test".to_string(),
                action: "terminal".to_string(),
                operation_id: None,
                parent_operation_id: None,
                repo: None,
                branch: None,
                session: None,
                message: format!("deferred-evidence-{index}"),
                data_json: None,
            });
        }

        assert_eq!(state.deferred_db_events.len(), DEFERRED_DB_EVENT_CAPACITY);
        assert_eq!(state.deferred_db_overflow_total, overflow as u64);
        assert_eq!(state.deferred_db_overflow_pending, overflow as u64);
        assert_eq!(
            state.deferred_db_events.first().unwrap().message,
            format!("deferred-evidence-{overflow}")
        );
        let aggregate = state.deferred_overflow_event();
        let data: serde_json::Value =
            serde_json::from_str(aggregate.data_json.as_deref().unwrap()).unwrap();
        assert_eq!(data["overflow_count"], overflow);
        assert_eq!(data["overflow_total"], overflow);
        assert_eq!(data["capacity"], DEFERRED_DB_EVENT_CAPACITY);

        let runtime = fs::read_to_string(dir.join("runtime.log")).unwrap();
        assert_eq!(
            runtime.matches("deferred-evidence-").count(),
            DEFERRED_DB_EVENT_CAPACITY + overflow
        );
        assert!(runtime.contains("deferred event queue overflow"));
        assert!(runtime.contains("runtime events remain recorded"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn deferred_flush_persists_backlog_and_overflow_aggregate() {
        let dir = test_path("deferred-flush");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("prism.db");
        drop(open_writable_db_path(&db).unwrap());
        let mut state = ObserverState::new(ObserverOptions {
            log_level: LogLevel::Debug,
            print_logs: false,
        });
        state.prism_dir = Some(dir.clone());
        state.record_deferred_event(Event {
            time_unix_ms: 1,
            level: LogLevel::Error,
            target: "deferred_test".to_string(),
            action: "terminal".to_string(),
            operation_id: None,
            parent_operation_id: None,
            repo: None,
            branch: None,
            session: None,
            message: "persist me".to_string(),
            data_json: None,
        });
        state.deferred_db_overflow_total = 3;
        state.deferred_db_overflow_pending = 2;

        state.flush_deferred_db_events();

        assert!(state.deferred_db_events.is_empty());
        assert_eq!(state.deferred_db_overflow_pending, 0);
        assert_eq!(
            scalar_i64(
                &db,
                "select count(*) from event where target = 'deferred_test' and action = 'terminal'",
            )
            .unwrap(),
            1
        );
        let data = scalar_string(
                &db,
                "select data_json from event where target = 'observability' and action = 'deferred_overflow'",
            )
            .unwrap();
        let data: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(data["overflow_count"], 2);
        assert_eq!(data["overflow_total"], 3);
        let _ = fs::remove_dir_all(dir);
    }

    fn table_exists(path: &Path, table: &str) -> bool {
        let mut connection =
            crate::persistence::database::TestConnection::open_readonly(path).unwrap();
        connection
            .scalar_bool(
                "select exists(select 1 from sqlite_master where type = 'table' and name = ?1)",
                table,
            )
            .unwrap()
    }

    fn execute_at(path: &Path, sql: &str) -> Result<(), String> {
        let mut connection = crate::persistence::database::TestConnection::open_writable(path)?;
        execute_on(&mut connection, sql)
    }

    fn execute_on(
        connection: &mut crate::persistence::database::TestConnection,
        sql: &str,
    ) -> Result<(), String> {
        connection.execute_batch(sql)
    }

    fn scalar_i64(path: &Path, query: &str) -> Result<i64, String> {
        let mut connection = crate::persistence::database::TestConnection::open_readonly(path)?;
        connection.scalar_i64(query)
    }

    fn scalar_string(path: &Path, query: &str) -> Result<String, String> {
        let mut connection = crate::persistence::database::TestConnection::open_readonly(path)?;
        connection.scalar_string(query)
    }

    fn test_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "prism-observability-{label}-{}-{}",
            std::process::id(),
            now_ms()
        ))
    }
}
