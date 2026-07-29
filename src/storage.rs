use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

pub const CURRENT_SCHEMA_VERSION: u32 = 1;
pub const WRITER_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

static VALIDATED_DATABASES: OnceLock<Mutex<HashSet<DatabaseIdentity>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct DatabaseIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageErrorKind {
    Busy,
    Locked,
    Constraint,
    Corruption,
    Io,
    ReadOnly,
    Full,
    CannotOpen,
    Other,
}

#[derive(Debug)]
enum StorageErrorSource {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
}

#[derive(Debug)]
pub struct StorageError {
    context: String,
    kind: StorageErrorKind,
    primary_code: Option<i32>,
    extended_code: Option<i32>,
    busy_elapsed: Option<Duration>,
    source: Option<Box<StorageErrorSource>>,
    corruption_check: Option<Box<StorageCheckReport>>,
    corruption_check_error: Option<Box<str>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageCheckReport {
    pub quick_check: Vec<String>,
    pub foreign_key_check: Vec<String>,
}

impl StorageError {
    pub fn kind(&self) -> StorageErrorKind {
        self.kind
    }

    pub fn primary_code(&self) -> Option<i32> {
        self.primary_code
    }

    pub fn extended_code(&self) -> Option<i32> {
        self.extended_code
    }

    pub fn busy_elapsed(&self) -> Option<Duration> {
        self.busy_elapsed
    }

    pub fn corruption_check(&self) -> Option<&StorageCheckReport> {
        self.corruption_check.as_deref()
    }

    pub fn corruption_check_error(&self) -> Option<&str> {
        self.corruption_check_error.as_deref()
    }

    pub fn from_sqlite(
        context: impl Into<String>,
        source: rusqlite::Error,
        elapsed: Duration,
    ) -> Self {
        let extended_code = match &source {
            rusqlite::Error::SqliteFailure(error, _) => Some(error.extended_code),
            _ => None,
        };
        let primary_code = extended_code.map(|code| code & 0xff);
        let kind = match primary_code {
            Some(rusqlite::ffi::SQLITE_BUSY) => StorageErrorKind::Busy,
            Some(rusqlite::ffi::SQLITE_LOCKED) => StorageErrorKind::Locked,
            Some(rusqlite::ffi::SQLITE_CONSTRAINT) => StorageErrorKind::Constraint,
            Some(rusqlite::ffi::SQLITE_CORRUPT | rusqlite::ffi::SQLITE_NOTADB) => {
                StorageErrorKind::Corruption
            }
            Some(rusqlite::ffi::SQLITE_IOERR) => StorageErrorKind::Io,
            Some(rusqlite::ffi::SQLITE_READONLY) => StorageErrorKind::ReadOnly,
            Some(rusqlite::ffi::SQLITE_FULL) => StorageErrorKind::Full,
            Some(rusqlite::ffi::SQLITE_CANTOPEN) => StorageErrorKind::CannotOpen,
            _ => StorageErrorKind::Other,
        };
        Self {
            context: context.into(),
            kind,
            primary_code,
            extended_code,
            busy_elapsed: (kind == StorageErrorKind::Busy).then_some(elapsed),
            source: Some(Box::new(StorageErrorSource::Sqlite(source))),
            corruption_check: None,
            corruption_check_error: None,
        }
    }

    fn from_io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self {
            context: context.into(),
            kind: StorageErrorKind::Io,
            primary_code: None,
            extended_code: None,
            busy_elapsed: None,
            source: Some(Box::new(StorageErrorSource::Io(source))),
            corruption_check: None,
            corruption_check_error: None,
        }
    }

    fn policy(context: impl Into<String>, kind: StorageErrorKind) -> Self {
        Self {
            context: context.into(),
            kind,
            primary_code: None,
            extended_code: None,
            busy_elapsed: None,
            source: None,
            corruption_check: None,
            corruption_check_error: None,
        }
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.context)?;
        if let Some(source) = &self.source {
            match source.as_ref() {
                StorageErrorSource::Sqlite(source) => write!(formatter, ": {source}")?,
                StorageErrorSource::Io(source) => write!(formatter, ": {source}")?,
            };
        }
        if let Some(report) = &self.corruption_check {
            let quick = if report.quick_check.as_slice() == ["ok"] {
                "ok".to_string()
            } else {
                format!("{} issue(s)", report.quick_check.len())
            };
            let foreign_keys = if report.foreign_key_check.is_empty() {
                "ok".to_string()
            } else {
                format!("{} violation(s)", report.foreign_key_check.len())
            };
            write!(
                formatter,
                "; read-only diagnostics: quick_check={quick}, foreign_key_check={foreign_keys}"
            )?;
        } else if let Some(check_error) = &self.corruption_check_error {
            write!(
                formatter,
                "; read-only quick_check unavailable: {check_error}"
            )?;
        }
        Ok(())
    }
}

impl Error for StorageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self.source.as_deref()? {
            StorageErrorSource::Sqlite(source) => Some(source),
            StorageErrorSource::Io(source) => Some(source),
        }
    }
}

pub fn open_writable(path: &Path) -> Result<Connection, StorageError> {
    open_writable_inner(path).map_err(|error| diagnose_corruption(path, error))
}

fn open_writable_inner(path: &Path) -> Result<Connection, StorageError> {
    let existed = path.exists();
    if existed {
        reject_empty_database(path)?;
    } else if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| StorageError::from_io("create database directory", error))?;
    }

    let started = Instant::now();
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| {
        StorageError::from_sqlite(format!("open {}", path.display()), error, started.elapsed())
    })?;
    configure_writer(&conn)?;

    if !journal_mode(&conn)?.eq_ignore_ascii_case("wal") {
        request_and_verify_wal(&conn, path)?;
    }

    let version = user_version(&conn)?;
    if version > CURRENT_SCHEMA_VERSION {
        return Err(future_version_error(path, version));
    }
    if version == CURRENT_SCHEMA_VERSION {
        let identity = database_identity(path)?;
        if let Some(identity) = identity
            && database_identity_is_validated(&identity)?
        {
            return Ok(conn);
        }
        validate_complete_schema(&conn)?;
        validate_foreign_keys(&conn)?;
        mark_database_validated_if_unchanged(path, identity)?;
        return Ok(conn);
    }

    migrate(&conn, path, |_| Ok(()))?;
    Ok(conn)
}

pub fn open_readonly(path: &Path) -> Result<Connection, StorageError> {
    open_readonly_inner(path).map_err(|error| diagnose_corruption(path, error))
}

fn open_readonly_inner(path: &Path) -> Result<Connection, StorageError> {
    if !path.exists() {
        return Err(StorageError::policy(
            format!("database {} does not exist", path.display()),
            StorageErrorKind::CannotOpen,
        ));
    }
    reject_empty_database(path)?;
    let started = Instant::now();
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| {
        StorageError::from_sqlite(
            format!("open {} read-only", path.display()),
            error,
            started.elapsed(),
        )
    })?;
    configure_readonly(&conn)?;
    Ok(conn)
}

pub fn run_writer<T>(
    path: &Path,
    run: impl FnOnce(&Connection) -> rusqlite::Result<T>,
) -> Result<T, StorageError> {
    let conn = open_writable(path)?;
    let started = Instant::now();
    run(&conn)
        .map_err(|error| StorageError::from_sqlite("database operation", error, started.elapsed()))
        .map_err(|error| diagnose_corruption(path, error))
}

fn reject_empty_database(path: &Path) -> Result<(), StorageError> {
    let metadata = fs::metadata(path)
        .map_err(|error| StorageError::from_io(format!("inspect {}", path.display()), error))?;
    if metadata.len() == 0 {
        return Err(StorageError::policy(
            format!(
                "database {} is an existing empty file; refusing to initialize it",
                path.display()
            ),
            StorageErrorKind::Corruption,
        ));
    }
    Ok(())
}

fn diagnose_corruption(path: &Path, mut error: StorageError) -> StorageError {
    if error.kind != StorageErrorKind::Corruption {
        return error;
    }
    match quick_check_readonly(path) {
        Ok(report) => error.corruption_check = Some(Box::new(report)),
        Err(check_error) => {
            error.corruption_check_error = Some(check_error.to_string().into_boxed_str())
        }
    }
    error
}

fn configure_writer(conn: &Connection) -> Result<(), StorageError> {
    let started = Instant::now();
    conn.busy_timeout(WRITER_BUSY_TIMEOUT).map_err(|error| {
        StorageError::from_sqlite("configure writer busy timeout", error, started.elapsed())
    })?;
    set_and_verify_common_policy(conn)?;
    Ok(())
}

fn configure_readonly(conn: &Connection) -> Result<(), StorageError> {
    let started = Instant::now();
    conn.busy_timeout(Duration::ZERO).map_err(|error| {
        StorageError::from_sqlite("configure read-only busy timeout", error, started.elapsed())
    })?;
    set_and_verify_common_policy(conn)?;
    conn.pragma_update(None, "query_only", true)
        .map_err(|error| {
            StorageError::from_sqlite("enable SQLite query_only", error, started.elapsed())
        })?;
    let query_only: i64 = conn
        .pragma_query_value(None, "query_only", |row| row.get(0))
        .map_err(|error| {
            StorageError::from_sqlite("verify SQLite query_only", error, started.elapsed())
        })?;
    if query_only != 1 {
        return Err(StorageError::policy(
            "SQLite did not enable query_only",
            StorageErrorKind::Other,
        ));
    }
    Ok(())
}

fn set_and_verify_common_policy(conn: &Connection) -> Result<(), StorageError> {
    let started = Instant::now();
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(|error| {
            StorageError::from_sqlite("enable foreign keys", error, started.elapsed())
        })?;
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(|error| {
            StorageError::from_sqlite("set SQLite synchronous=FULL", error, started.elapsed())
        })?;
    let foreign_keys: i64 = conn
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .map_err(|error| {
            StorageError::from_sqlite("verify foreign key policy", error, started.elapsed())
        })?;
    let synchronous: i64 = conn
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .map_err(|error| {
            StorageError::from_sqlite("verify synchronous policy", error, started.elapsed())
        })?;
    if foreign_keys != 1 || synchronous != 2 {
        return Err(StorageError::policy(
            format!(
                "SQLite connection policy mismatch: foreign_keys={foreign_keys}, synchronous={synchronous}"
            ),
            StorageErrorKind::Other,
        ));
    }
    Ok(())
}

fn user_version(conn: &Connection) -> Result<u32, StorageError> {
    let started = Instant::now();
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| {
            StorageError::from_sqlite("read schema user_version", error, started.elapsed())
        })?;
    u32::try_from(version).map_err(|_| {
        StorageError::policy(
            format!("invalid negative schema user_version {version}"),
            StorageErrorKind::Corruption,
        )
    })
}

fn future_version_error(path: &Path, version: u32) -> StorageError {
    StorageError::policy(
        format!(
            "database {} has future schema version {version}; this Prism supports up to {CURRENT_SCHEMA_VERSION}",
            path.display()
        ),
        StorageErrorKind::Other,
    )
}

fn database_identity(path: &Path) -> Result<Option<DatabaseIdentity>, StorageError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::metadata(path).map_err(|error| {
            StorageError::from_io(format!("identify database {}", path.display()), error)
        })?;
        Ok(Some(DatabaseIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = path;
        Ok(None)
    }
}

fn database_identity_is_validated(identity: &DatabaseIdentity) -> Result<bool, StorageError> {
    let validated = VALIDATED_DATABASES.get_or_init(|| Mutex::new(HashSet::new()));
    validated
        .lock()
        .map(|validated| validated.contains(identity))
        .map_err(|_| {
            StorageError::policy(
                "validated database identity cache poisoned",
                StorageErrorKind::Other,
            )
        })
}

fn mark_database_validated(identity: Option<DatabaseIdentity>) -> Result<(), StorageError> {
    let Some(identity) = identity else {
        return Ok(());
    };
    let validated = VALIDATED_DATABASES.get_or_init(|| Mutex::new(HashSet::new()));
    validated
        .lock()
        .map(|mut validated| {
            validated.insert(identity);
        })
        .map_err(|_| {
            StorageError::policy(
                "validated database identity cache poisoned",
                StorageErrorKind::Other,
            )
        })
}

fn mark_database_validated_if_unchanged(
    path: &Path,
    identity: Option<DatabaseIdentity>,
) -> Result<(), StorageError> {
    if identity == database_identity(path)? {
        mark_database_validated(identity)?;
    }
    Ok(())
}

fn journal_mode(conn: &Connection) -> Result<String, StorageError> {
    let started = Instant::now();
    conn.pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(|error| {
            StorageError::from_sqlite("read SQLite journal_mode", error, started.elapsed())
        })
}

fn request_and_verify_wal(conn: &Connection, path: &Path) -> Result<(), StorageError> {
    let started = Instant::now();
    let requested: String = conn
        .query_row("pragma journal_mode = wal", [], |row| row.get(0))
        .map_err(|error| {
            StorageError::from_sqlite(
                format!("request WAL journal mode for {}", path.display()),
                error,
                started.elapsed(),
            )
        })?;
    let verified = journal_mode(conn)?;
    if !requested.eq_ignore_ascii_case("wal") || !verified.eq_ignore_ascii_case("wal") {
        return Err(StorageError::policy(
            format!(
                "database {} returned journal_mode={requested} then {verified}; Prism requires WAL on a local filesystem and will not silently fall back",
                path.display()
            ),
            StorageErrorKind::Io,
        ));
    }
    Ok(())
}

fn migrate(
    conn: &Connection,
    path: &Path,
    mut before_advance: impl FnMut(u32) -> Result<(), StorageError>,
) -> Result<(), StorageError> {
    let identity = database_identity(path)?;
    loop {
        execute_batch(
            conn,
            "begin immediate",
            "begin schema migration transaction",
        )?;
        let result = (|| {
            let version = user_version(conn)?;
            if version > CURRENT_SCHEMA_VERSION {
                return Err(future_version_error(path, version));
            }
            if version == CURRENT_SCHEMA_VERSION {
                validate_complete_schema(conn)?;
                validate_foreign_keys(conn)?;
                return execute_batch(conn, "commit", "commit schema validation transaction")
                    .map(|()| None);
            }

            let next = version + 1;
            match next {
                1 => apply_complete_schema_baseline(conn)?,
                _ => unreachable!("missing schema migration {next}"),
            }
            validate_complete_schema(conn)?;
            validate_foreign_keys(conn)?;
            before_advance(next)?;
            execute_batch(
                conn,
                &format!("pragma user_version = {next}"),
                &format!("record schema migration {next}"),
            )?;
            execute_batch(conn, "commit", &format!("commit schema migration {next}"))?;
            Ok((next < CURRENT_SCHEMA_VERSION).then_some(next))
        })();
        match result {
            Ok(Some(_)) => continue,
            Ok(None) => {
                mark_database_validated_if_unchanged(path, identity)?;
                return Ok(());
            }
            Err(error) => {
                let _ = conn.execute_batch("rollback");
                return Err(error);
            }
        }
    }
}

fn apply_complete_schema_baseline(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(
        "
        create table if not exists metadata (key text primary key, value text not null);
        create table if not exists event (
          id integer primary key autoincrement, time_unix_ms integer not null,
          level text not null, target text not null, action text not null,
          operation_id text, parent_operation_id text, repo text, branch text,
          session text, message text not null, data_json text
        );
        create index if not exists event_time_idx on event(time_unix_ms);
        create index if not exists event_target_idx on event(target);
        create index if not exists event_action_idx on event(action);
        create index if not exists event_branch_idx on event(branch);
        create index if not exists event_operation_idx on event(operation_id);
        create table if not exists startup_run (
          id text primary key, time_started_unix_ms integer not null,
          time_finished_unix_ms integer, status text not null, repo text,
          version text not null, error text
        );
        create table if not exists startup_phase (
          id integer primary key autoincrement,
          run_id text not null references startup_run(id) on delete cascade,
          phase text not null, time_started_unix_ms integer not null,
          time_finished_unix_ms integer, status text not null, error text
        );
        ",
    )
    .map_err(|error| {
        StorageError::from_sqlite("create observability schema", error, Duration::ZERO)
    })?;
    crate::session::migrate_worktree_session_schema(conn).map_err(migration_error)?;
    crate::opencode::migrate_runtime_schema(conn).map_err(migration_error)?;
    crate::plan_run::migrate_schema(conn).map_err(migration_error)?;
    crate::auto_flow::migrate_schema(conn).map_err(migration_error)?;
    crate::execution::migrate_schema(conn).map_err(migration_error)?;
    crate::github::migrate_pr_cache_schema(conn).map_err(migration_error)?;
    Ok(())
}

fn migration_error(message: String) -> StorageError {
    StorageError::policy(message, StorageErrorKind::Other)
}

struct RequiredTable {
    name: &'static str,
    columns: &'static [&'static str],
    primary_key: &'static [&'static str],
    minimum_foreign_keys: usize,
}

const REQUIRED_TABLES: &[RequiredTable] = &[
    RequiredTable {
        name: "metadata",
        columns: &["key", "value"],
        primary_key: &["key"],
        minimum_foreign_keys: 0,
    },
    RequiredTable {
        name: "event",
        columns: &[
            "id",
            "time_unix_ms",
            "level",
            "target",
            "action",
            "message",
            "data_json",
        ],
        primary_key: &["id"],
        minimum_foreign_keys: 0,
    },
    RequiredTable {
        name: "startup_run",
        columns: &["id", "time_started_unix_ms", "status", "version"],
        primary_key: &["id"],
        minimum_foreign_keys: 0,
    },
    RequiredTable {
        name: "startup_phase",
        columns: &["id", "run_id", "phase", "time_started_unix_ms", "status"],
        primary_key: &["id"],
        minimum_foreign_keys: 1,
    },
    RequiredTable {
        name: "task_metadata",
        columns: &[
            "branch",
            "prompt_summary",
            "initial_prompt",
            "worktree",
            "classification",
            "visibility",
            "updated_unix_ms",
        ],
        primary_key: &["branch"],
        minimum_foreign_keys: 0,
    },
    RequiredTable {
        name: "hidden_session",
        columns: &["branch", "hidden_unix_ms"],
        primary_key: &["branch"],
        minimum_foreign_keys: 0,
    },
    RequiredTable {
        name: "archived_worktree",
        columns: &[
            "branch",
            "repo_root",
            "worktree_path",
            "archived_unix_ms",
            "classification",
        ],
        primary_key: &["branch"],
        minimum_foreign_keys: 0,
    },
    RequiredTable {
        name: "agent_state",
        columns: &["branch", "state", "updated_unix_ms"],
        primary_key: &["branch"],
        minimum_foreign_keys: 0,
    },
    RequiredTable {
        name: "worktree_harness",
        columns: &[
            "branch",
            "worktree_path",
            "worktree_incarnation",
            "harness_id",
            "migration_policy",
            "updated_unix_ms",
        ],
        primary_key: &["branch"],
        minimum_foreign_keys: 0,
    },
    RequiredTable {
        name: "opencode_runtime",
        columns: &[
            "repo_root",
            "harness_id",
            "branch",
            "worktree_path",
            "server_port",
            "server_url",
            "generation",
            "updated_unix_ms",
            "server_start_time_ticks",
        ],
        primary_key: &["repo_root", "harness_id", "branch", "worktree_path"],
        minimum_foreign_keys: 0,
    },
    RequiredTable {
        name: "plan_run",
        columns: &[
            "id",
            "harness_id",
            "adapter_id",
            "repo_root",
            "scope_path",
            "plan_path",
            "status",
            "pause_requested",
            "archived_unix_ms",
        ],
        primary_key: &["id"],
        minimum_foreign_keys: 0,
    },
    RequiredTable {
        name: "plan_step_run",
        columns: &[
            "run_id",
            "step",
            "prompt",
            "status",
            "execution_state",
            "session_endpoint",
            "session_id",
            "execution_process_start_time_ticks",
        ],
        primary_key: &["run_id", "step"],
        minimum_foreign_keys: 1,
    },
    RequiredTable {
        name: "plan_output_line",
        columns: &[
            "run_id",
            "step",
            "line_number",
            "time_unix_ms",
            "kind",
            "text",
        ],
        primary_key: &["run_id", "step", "line_number"],
        minimum_foreign_keys: 1,
    },
    RequiredTable {
        name: "auto_run",
        columns: &[
            "id",
            "harness_id",
            "adapter_id",
            "repo_root",
            "worktree_path",
            "implementation_source",
            "plan_path",
            "plan_run_mode",
            "status",
            "pending_push_json",
        ],
        primary_key: &["id"],
        minimum_foreign_keys: 1,
    },
    RequiredTable {
        name: "auto_step_run",
        columns: &[
            "id",
            "run_id",
            "sequence",
            "step_key",
            "status",
            "execution_state",
            "session_endpoint",
            "session_id",
            "plan_run_id",
            "work_guard_json",
        ],
        primary_key: &["id"],
        minimum_foreign_keys: 1,
    },
    RequiredTable {
        name: "auto_output_line",
        columns: &["step_run_id", "line_number", "time_unix_ms", "kind", "text"],
        primary_key: &["step_run_id", "line_number"],
        minimum_foreign_keys: 1,
    },
    RequiredTable {
        name: "auto_event",
        columns: &[
            "id",
            "run_id",
            "step_run_id",
            "time_unix_ms",
            "kind",
            "data_json",
        ],
        primary_key: &["id"],
        minimum_foreign_keys: 2,
    },
    RequiredTable {
        name: "auto_schema_version",
        columns: &["id", "version"],
        primary_key: &["id"],
        minimum_foreign_keys: 0,
    },
    RequiredTable {
        name: "workflow_execution",
        columns: &[
            "workflow_kind",
            "run_id",
            "dispatch_state",
            "fencing_token",
            "requeue_requested",
            "interruption_generation",
            "created_unix_ms",
            "updated_unix_ms",
        ],
        primary_key: &["workflow_kind", "run_id"],
        minimum_foreign_keys: 0,
    },
    RequiredTable {
        name: "pr_cache",
        columns: &[
            "branch",
            "number",
            "title",
            "body",
            "requested_reviewers",
            "merge_state_status",
            "comment_count",
            "observation_error",
        ],
        primary_key: &["branch"],
        minimum_foreign_keys: 0,
    },
    RequiredTable {
        name: "pr_details_cache",
        columns: &[
            "branch",
            "pr_number",
            "head_sha",
            "comments",
            "check_contexts",
            "ci_failures",
            "observation_error",
        ],
        primary_key: &["branch"],
        minimum_foreign_keys: 0,
    },
    RequiredTable {
        name: "repo_policy_cache",
        columns: &[
            "repo_remote",
            "required_approvals",
            "required_checks",
            "merge_queue_required",
            "refreshed_unix_ms",
        ],
        primary_key: &["repo_remote"],
        minimum_foreign_keys: 0,
    },
];

fn validate_complete_schema(conn: &Connection) -> Result<(), StorageError> {
    for table in REQUIRED_TABLES {
        let object_type: Option<String> = conn
            .query_row(
                "select type from sqlite_master where name = ?1",
                [table.name],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                StorageError::from_sqlite(
                    format!("inspect {} schema object", table.name),
                    error,
                    Duration::ZERO,
                )
            })?;
        if object_type.as_deref() != Some("table") {
            return Err(StorageError::policy(
                format!("schema validation failed: {} is not a table", table.name),
                StorageErrorKind::Corruption,
            ));
        }
        let mut statement = conn
            .prepare(&format!("pragma table_info({})", table.name))
            .map_err(|error| {
                StorageError::from_sqlite(
                    format!("inspect {} columns", table.name),
                    error,
                    Duration::ZERO,
                )
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
            })
            .map_err(|error| {
                StorageError::from_sqlite(
                    format!("read {} columns", table.name),
                    error,
                    Duration::ZERO,
                )
            })?;
        let mut columns = Vec::new();
        let mut primary_key = Vec::new();
        for row in rows {
            let (column, position) = row.map_err(|error| {
                StorageError::from_sqlite(
                    format!("read {} column", table.name),
                    error,
                    Duration::ZERO,
                )
            })?;
            columns.push(column.clone());
            if position > 0 {
                primary_key.push((position, column));
            }
        }
        for required in table.columns {
            if !columns.iter().any(|column| column == required) {
                return Err(StorageError::policy(
                    format!(
                        "schema validation failed: {} is missing column {required}",
                        table.name
                    ),
                    StorageErrorKind::Corruption,
                ));
            }
        }
        primary_key.sort_by_key(|(position, _)| *position);
        let primary_key = primary_key
            .into_iter()
            .map(|(_, column)| column)
            .collect::<Vec<_>>();
        if primary_key != table.primary_key {
            return Err(StorageError::policy(
                format!(
                    "schema validation failed: {} has primary key {primary_key:?}, expected {:?}",
                    table.name, table.primary_key
                ),
                StorageErrorKind::Corruption,
            ));
        }
        let foreign_key_count: usize = conn
            .prepare(&format!("pragma foreign_key_list({})", table.name))
            .and_then(|mut statement| {
                let rows = statement.query_map([], |_| Ok(()))?;
                rows.collect::<Result<Vec<_>, _>>().map(|rows| rows.len())
            })
            .map_err(|error| {
                StorageError::from_sqlite(
                    format!("inspect {} foreign keys", table.name),
                    error,
                    Duration::ZERO,
                )
            })?;
        if foreign_key_count < table.minimum_foreign_keys {
            return Err(StorageError::policy(
                format!(
                    "schema validation failed: {} has {foreign_key_count} foreign keys, expected at least {}",
                    table.name, table.minimum_foreign_keys
                ),
                StorageErrorKind::Corruption,
            ));
        }
    }
    Ok(())
}

fn validate_foreign_keys(conn: &Connection) -> Result<(), StorageError> {
    let violations = foreign_key_check(conn)?;
    if let Some(first) = violations.first() {
        return Err(StorageError::policy(
            format!("foreign_key_check failed before schema version advance: {first}"),
            StorageErrorKind::Constraint,
        ));
    }
    Ok(())
}

pub fn quick_check_readonly(path: &Path) -> Result<StorageCheckReport, StorageError> {
    let conn = open_readonly_inner(path)?;
    Ok(StorageCheckReport {
        quick_check: quick_check(&conn)?,
        foreign_key_check: foreign_key_check(&conn)?,
    })
}

fn quick_check(conn: &Connection) -> Result<Vec<String>, StorageError> {
    let started = Instant::now();
    let mut statement = conn.prepare("pragma quick_check").map_err(|error| {
        StorageError::from_sqlite("prepare quick_check", error, started.elapsed())
    })?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| StorageError::from_sqlite("run quick_check", error, started.elapsed()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| StorageError::from_sqlite("read quick_check", error, started.elapsed()))
}

fn foreign_key_check(conn: &Connection) -> Result<Vec<String>, StorageError> {
    let started = Instant::now();
    let mut statement = conn.prepare("pragma foreign_key_check").map_err(|error| {
        StorageError::from_sqlite("prepare foreign_key_check", error, started.elapsed())
    })?;
    let rows = statement
        .query_map([], |row| {
            Ok(format!(
                "table={} rowid={} parent={} fk_index={}",
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "null".to_string()),
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?
            ))
        })
        .map_err(|error| {
            StorageError::from_sqlite("run foreign_key_check", error, started.elapsed())
        })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|error| {
        StorageError::from_sqlite("read foreign_key_check", error, started.elapsed())
    })
}

fn execute_batch(conn: &Connection, sql: &str, context: &str) -> Result<(), StorageError> {
    let started = Instant::now();
    conn.execute_batch(sql)
        .map_err(|error| StorageError::from_sqlite(context, error, started.elapsed()))
}

pub(crate) fn print_integrity(path: &Path) -> Result<(), StorageError> {
    println!("path = {}", path.display());
    let conn = match open_readonly(path) {
        Ok(conn) => conn,
        Err(error) => {
            println!("user_version = unavailable");
            println!("journal_mode = unavailable");
            println!("integrity_check:");
            println!("  ERROR: {error}");
            println!("foreign_key_check:");
            println!("  ERROR: database could not be opened read-only");
            return Err(error);
        }
    };
    let mut first_error = None;
    match user_version(&conn) {
        Ok(version) => println!("user_version = {version}"),
        Err(error) => {
            println!("user_version = unavailable ({error})");
            first_error = Some(error);
        }
    }
    match journal_mode(&conn) {
        Ok(mode) => println!("journal_mode = {mode}"),
        Err(error) => {
            println!("journal_mode = unavailable ({error})");
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }

    let started = Instant::now();
    let integrity = (|| -> rusqlite::Result<Vec<String>> {
        let mut statement = conn.prepare("pragma integrity_check")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect()
    })()
    .map_err(|error| StorageError::from_sqlite("run integrity_check", error, started.elapsed()));
    println!("integrity_check:");
    match integrity {
        Ok(rows) => {
            for row in &rows {
                println!("  {row}");
            }
            if rows.as_slice() != ["ok"] {
                first_error = Some(StorageError::policy(
                    "SQLite integrity_check reported corruption",
                    StorageErrorKind::Corruption,
                ));
            }
        }
        Err(error) => {
            println!("  ERROR: {error}");
            first_error = Some(error);
        }
    }

    println!("foreign_key_check:");
    match foreign_key_check(&conn) {
        Ok(rows) if rows.is_empty() => println!("  ok"),
        Ok(rows) => {
            for row in &rows {
                println!("  {row}");
            }
            first_error.get_or_insert_with(|| {
                StorageError::policy(
                    "SQLite foreign_key_check reported violations",
                    StorageErrorKind::Constraint,
                )
            });
        }
        Err(error) => {
            println!("  ERROR: {error}");
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, mpsc};

    static TEST_PATH_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

    fn test_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "prism-storage-{label}-{}-{}-{}.db",
            std::process::id(),
            std::thread::current().name().unwrap_or("test"),
            TEST_PATH_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[test]
    fn every_writer_enforces_foreign_keys_and_durable_wal_policy() {
        let path = test_path("policy");
        let first = open_writable(&path).unwrap();
        drop(first);
        let second = open_writable(&path).unwrap();

        let foreign_keys: i64 = second
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        let synchronous: i64 = second
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        let mode: String = second
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        let version: u32 = second
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
        assert_eq!(synchronous, 2);
        assert_eq!(mode, "wal");
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let reader = open_readonly(&path).unwrap();
        let reader_busy: i64 = reader
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        let query_only: i64 = reader
            .pragma_query_value(None, "query_only", |row| row.get(0))
            .unwrap();
        assert_eq!(reader_busy, 0);
        assert_eq!(query_only, 1);
        drop(reader);

        second.execute("insert into startup_phase (run_id, phase, time_started_unix_ms, status) values ('missing', 'x', 1, 'x')", []).unwrap_err();
        drop(second);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn validated_second_writer_opens_promptly_while_another_writer_holds_lock() {
        let path = test_path("validated-writer-fast-path");
        let first = open_writable(&path).unwrap();
        drop(first);

        let holder = open_writable(&path).unwrap();
        holder.execute_batch("begin immediate").unwrap();
        let started = Instant::now();

        let second = open_writable(&path).unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
        let foreign_keys: i64 = second
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
        holder.execute_batch("rollback").unwrap();
        drop(second);
        drop(holder);
        let _ = fs::remove_file(path);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn replacing_database_at_same_path_invalidates_validation_cache() {
        let path = test_path("replacement-invalidates-cache");
        let conn = open_writable(&path).unwrap();
        drop(conn);

        let replacement = path.with_extension("replacement.db");
        let conn = Connection::open(&replacement).unwrap();
        configure_writer(&conn).unwrap();
        request_and_verify_wal(&conn, &replacement).unwrap();
        conn.execute_batch("create table metadata (key text, value text)")
            .unwrap();
        conn.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)
            .unwrap();
        drop(conn);
        fs::rename(&replacement, &path).unwrap();
        let before = fs::read(&path).unwrap();

        let error = open_writable(&path).unwrap_err();

        assert_eq!(error.kind(), StorageErrorKind::Corruption);
        assert!(error.to_string().contains("primary key"));
        assert_eq!(fs::read(&path).unwrap(), before);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn migration_failure_rolls_back_and_retry_succeeds() {
        let path = test_path("migration-retry");
        let conn = Connection::open(&path).unwrap();
        configure_writer(&conn).unwrap();
        request_and_verify_wal(&conn, &path).unwrap();
        let error = migrate(&conn, &path, |version| {
            assert_eq!(version, 1);
            Err(StorageError::policy(
                "injected migration fault",
                StorageErrorKind::Other,
            ))
        })
        .unwrap_err();
        assert!(error.to_string().contains("injected migration fault"));
        assert_eq!(user_version(&conn).unwrap(), 0);
        assert!(conn.is_autocommit());
        assert_eq!(
            conn.query_row(
                "select count(*) from sqlite_master where type = 'table' and name = 'metadata'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
        drop(conn);

        let conn = open_writable(&path).unwrap();
        assert_eq!(user_version(&conn).unwrap(), CURRENT_SCHEMA_VERSION);
        drop(conn);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn future_version_is_rejected_without_mutation() {
        let path = test_path("future");
        let conn = Connection::open(&path).unwrap();
        configure_writer(&conn).unwrap();
        request_and_verify_wal(&conn, &path).unwrap();
        conn.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 1)
            .unwrap();
        drop(conn);
        let before = fs::read(&path).unwrap();

        let error = open_writable(&path).unwrap_err();
        assert!(error.to_string().contains("future schema version"));
        assert_eq!(fs::read(&path).unwrap(), before);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn malformed_same_name_table_is_not_marked_current() {
        let path = test_path("malformed");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("create table metadata (key text, value text)")
            .unwrap();
        configure_writer(&conn).unwrap();
        request_and_verify_wal(&conn, &path).unwrap();
        let error = migrate(&conn, &path, |_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("primary key"));
        assert_eq!(user_version(&conn).unwrap(), 0);
        drop(conn);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn malformed_current_schema_is_rejected_without_mutation() {
        let path = test_path("malformed-current");
        let conn = Connection::open(&path).unwrap();
        configure_writer(&conn).unwrap();
        request_and_verify_wal(&conn, &path).unwrap();
        conn.execute_batch("create table metadata (key text, value text)")
            .unwrap();
        conn.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)
            .unwrap();
        drop(conn);
        let before = fs::read(&path).unwrap();

        let error = open_writable(&path).unwrap_err();

        assert_eq!(error.kind(), StorageErrorKind::Corruption);
        assert!(error.to_string().contains("primary key"));
        assert!(error.to_string().contains("quick_check=ok"));
        assert!(error.corruption_check().is_some());
        assert_eq!(fs::read(&path).unwrap(), before);
        let conn = Connection::open(&path).unwrap();
        assert_eq!(user_version(&conn).unwrap(), CURRENT_SCHEMA_VERSION);
        let primary_key: i64 = conn
            .query_row(
                "select pk from pragma_table_info('metadata') where name = 'key'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(primary_key, 0);
        drop(conn);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn concurrent_initializers_apply_each_delta_once() {
        let path = test_path("concurrent-initialization");
        let conn = Connection::open(&path).unwrap();
        configure_writer(&conn).unwrap();
        request_and_verify_wal(&conn, &path).unwrap();
        drop(conn);

        let barrier = Arc::new(Barrier::new(3));
        let applied = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            let applied = Arc::clone(&applied);
            handles.push(std::thread::spawn(move || {
                let conn = Connection::open(&path).unwrap();
                configure_writer(&conn).unwrap();
                request_and_verify_wal(&conn, &path).unwrap();
                barrier.wait();
                migrate(&conn, &path, |_| {
                    applied.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();
            }));
        }
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(applied.load(Ordering::SeqCst), 1);
        let conn = open_writable(&path).unwrap();
        assert_eq!(user_version(&conn).unwrap(), CURRENT_SCHEMA_VERSION);
        drop(conn);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn partial_legacy_schema_is_reconciled_in_order_without_losing_rows() {
        let path = test_path("legacy-partial");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "create table task_metadata (
               branch text primary key,
               prompt_summary text not null,
               initial_prompt text not null,
               worktree text not null,
               updated_unix_ms integer not null
             );
             insert into task_metadata values ('feature', 'summary', 'prompt', '/repo/wt', 7);",
        )
        .unwrap();
        drop(conn);

        let conn = open_writable(&path).unwrap();
        assert_eq!(user_version(&conn).unwrap(), CURRENT_SCHEMA_VERSION);
        assert_eq!(
            conn.query_row(
                "select prompt_summary, classification, visibility from task_metadata where branch = 'feature'",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?)),
            )
            .unwrap(),
            ("summary".to_string(), "work".to_string(), 0)
        );
        assert_eq!(journal_mode(&conn).unwrap(), "wal");
        drop(conn);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn sqlite_error_classification_preserves_codes_and_sources() {
        for (code, expected) in [
            (rusqlite::ffi::SQLITE_BUSY, StorageErrorKind::Busy),
            (rusqlite::ffi::SQLITE_LOCKED, StorageErrorKind::Locked),
            (
                rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY,
                StorageErrorKind::Constraint,
            ),
            (rusqlite::ffi::SQLITE_CORRUPT, StorageErrorKind::Corruption),
            (rusqlite::ffi::SQLITE_NOTADB, StorageErrorKind::Corruption),
            (rusqlite::ffi::SQLITE_IOERR_READ, StorageErrorKind::Io),
            (rusqlite::ffi::SQLITE_READONLY, StorageErrorKind::ReadOnly),
            (rusqlite::ffi::SQLITE_FULL, StorageErrorKind::Full),
            (rusqlite::ffi::SQLITE_CANTOPEN, StorageErrorKind::CannotOpen),
        ] {
            let source = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
            let error = StorageError::from_sqlite("test", source, Duration::from_millis(3));
            assert_eq!(error.kind(), expected);
            assert_eq!(error.primary_code(), Some(code & 0xff));
            assert_eq!(error.extended_code(), Some(code));
            assert!(error.source().is_some());
            assert_eq!(
                error.busy_elapsed().is_some(),
                expected == StorageErrorKind::Busy
            );
        }
    }

    #[test]
    fn read_only_quick_check_reports_foreign_key_violations() {
        let path = test_path("quick-check");
        let conn = open_writable(&path).unwrap();
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn.execute(
            "insert into startup_phase (run_id, phase, time_started_unix_ms, status) values ('missing', 'phase', 1, 'failed')",
            [],
        )
        .unwrap();
        drop(conn);

        let report = quick_check_readonly(&path).unwrap();

        assert_eq!(report.quick_check, ["ok"]);
        assert_eq!(report.foreign_key_check.len(), 1);
        assert!(report.foreign_key_check[0].contains("table=startup_phase"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn coordinated_writer_contention_is_classified_and_data_remains_valid() {
        let path = test_path("busy");
        let conn = open_writable(&path).unwrap();
        conn.execute_batch("create table busy_test (id integer primary key, value text not null)")
            .unwrap();
        drop(conn);
        let barrier = Arc::new(Barrier::new(2));
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let writer_path = path.clone();
        let writer_barrier = Arc::clone(&barrier);
        let holder = std::thread::spawn(move || {
            let conn = open_writable(&writer_path).unwrap();
            conn.execute_batch("begin immediate; insert into busy_test values (1, 'held')")
                .unwrap();
            writer_barrier.wait();
            release_rx.recv().unwrap();
            conn.execute_batch("commit").unwrap();
        });
        barrier.wait();
        let error = run_writer(&path, |conn| {
            conn.execute("insert into busy_test values (2, 'blocked')", [])
                .map(|_| ())
        })
        .unwrap_err();
        assert_eq!(error.kind(), StorageErrorKind::Busy);
        assert_eq!(error.primary_code(), Some(rusqlite::ffi::SQLITE_BUSY));
        assert!(error.extended_code().is_some());
        assert!(error.busy_elapsed().unwrap() >= WRITER_BUSY_TIMEOUT);
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        let conn = open_readonly(&path).unwrap();
        assert_eq!(
            conn.query_row("select count(*) from busy_test", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("pragma integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        drop(conn);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn existing_empty_and_random_files_are_rejected() {
        for (label, bytes) in [
            ("empty", &b""[..]),
            ("random", &b"not a sqlite database"[..]),
        ] {
            let path = test_path(label);
            fs::write(&path, bytes).unwrap();
            let before = fs::read(&path).unwrap();
            let error = open_writable(&path).unwrap_err();
            assert_eq!(error.kind(), StorageErrorKind::Corruption);
            assert!(error.corruption_check().is_some() || error.corruption_check_error().is_some());
            assert_eq!(fs::read(&path).unwrap(), before);
            let _ = fs::remove_file(path);
        }
    }
}
