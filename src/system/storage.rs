use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

pub const CURRENT_SCHEMA_VERSION: u32 = 2;
pub const WRITER_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

static VALIDATED_DATABASES: OnceLock<Mutex<HashSet<DatabaseIdentity>>> = OnceLock::new();
static WAL_WARNING_BUCKETS: OnceLock<Mutex<BTreeMap<std::path::PathBuf, u64>>> = OnceLock::new();

const WAL_WARNING_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct DatabaseIdentity {
    path: PathBuf,
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

impl StorageErrorKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Locked => "locked",
            Self::Constraint => "constraint",
            Self::Corruption => "corruption",
            Self::Io => "io",
            Self::ReadOnly => "read_only",
            Self::Full => "full",
            Self::CannotOpen => "cannot_open",
            Self::Other => "other",
        }
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalStatus {
    pub main_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
    pub checkpoint_busy: i64,
    pub checkpoint_log_frames: i64,
    pub checkpointed_frames: i64,
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

    pub fn observation_data_json(&self) -> String {
        crate::observability::storage_error_data_json(
            self.kind.label(),
            self.primary_code,
            self.extended_code,
            self.busy_elapsed
                .map(|elapsed| elapsed.as_millis().min(i64::MAX as u128) as i64),
        )
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
    let started = Instant::now();
    let result = open_writable_inner(path).map_err(|error| diagnose_corruption(path, error));
    record_open("writable", started.elapsed(), &result);
    result
}

fn open_writable_inner(path: &Path) -> Result<Connection, StorageError> {
    let conn = open_writable_connection(path)?;
    let started = Instant::now();
    let version = user_version(&conn)?;
    if version > CURRENT_SCHEMA_VERSION {
        return Err(future_version_error(path, version));
    }
    if version == CURRENT_SCHEMA_VERSION {
        let identity = database_identity(path)?;
        if let Some(identity) = identity.as_ref()
            && database_identity_is_validated(identity)?
            && additive_schema_current(&conn)?
        {
            return Ok(conn);
        }
        conn.execute_batch("begin immediate").map_err(|error| {
            StorageError::from_sqlite("begin additive schema validation", error, started.elapsed())
        })?;
        let validation = apply_additive_schema_migrations(&conn)
            .and_then(|()| validate_complete_schema(&conn))
            .and_then(|()| validate_foreign_keys(&conn));
        if let Err(error) = validation {
            let _ = conn.execute_batch("rollback");
            return Err(error);
        }
        conn.execute_batch("commit").map_err(|error| {
            StorageError::from_sqlite(
                "commit additive schema validation",
                error,
                started.elapsed(),
            )
        })?;
        mark_database_validated_if_unchanged(path, identity)?;
        return Ok(conn);
    }

    migrate(&conn, path, |_| Ok(()))?;
    Ok(conn)
}

/// Opens SQLite with Prism's durability and concurrency policy but without
/// applying any schema. Domain stores own their independent migration policy.
pub(crate) fn open_writable_connection(path: &Path) -> Result<Connection, StorageError> {
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
    install_flight_recorder_trace(&conn);
    configure_writer(&conn)?;

    if !journal_mode(&conn)?.eq_ignore_ascii_case("wal") {
        request_and_verify_wal(&conn, path)?;
    }

    Ok(conn)
}

pub fn open_readonly(path: &Path) -> Result<Connection, StorageError> {
    let started = Instant::now();
    let result = open_readonly_inner(path, true).map_err(|error| diagnose_corruption(path, error));
    record_open("readonly", started.elapsed(), &result);
    result
}

fn record_open(access: &'static str, elapsed: Duration, result: &Result<Connection, StorageError>) {
    let mut fields = vec![
        crate::flight_recorder::text("access", access),
        crate::flight_recorder::boolean("success", result.is_ok()),
    ];
    if let Err(error) = result {
        fields.push(crate::flight_recorder::text(
            "error_kind",
            error.kind.label(),
        ));
        if let Some(busy) = error.busy_elapsed {
            fields.push(crate::flight_recorder::unsigned(
                "busy_wait_upper_bound_us",
                busy.as_micros(),
            ));
        }
    }
    crate::flight_recorder::record("sqlite", "open", Some(elapsed), fields);
}

fn install_flight_recorder_trace(conn: &Connection) {
    conn.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_PROFILE,
        Some(record_sqlite_profile),
    );
}

fn record_sqlite_profile(event: rusqlite::trace::TraceEvent<'_>) {
    let rusqlite::trace::TraceEvent::Profile(statement, elapsed) = event else {
        return;
    };
    let sql = statement.sql();
    let (statement_type, name) = sqlite_statement_name(&sql);
    crate::flight_recorder::record(
        "sqlite",
        "statement",
        Some(elapsed),
        vec![
            crate::flight_recorder::text("statement_type", statement_type),
            crate::flight_recorder::text("name", &name),
        ],
    );
}

fn sqlite_statement_name(sql: &str) -> (&'static str, String) {
    let tokens = sql
        .split_whitespace()
        .map(|token| {
            token
                .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                .to_ascii_lowercase()
        })
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let statement_type = match tokens.first().map(String::as_str) {
        Some("select" | "with") => "query",
        Some("insert" | "update" | "delete" | "replace") => "write",
        Some("begin" | "commit" | "rollback" | "savepoint" | "release") => "transaction",
        Some("pragma") => "pragma",
        Some("create" | "alter" | "drop") => "schema",
        _ => "other",
    };
    let target = match tokens.first().map(String::as_str) {
        Some("select" | "delete") => token_after(&tokens, "from"),
        Some("insert" | "replace") => token_after(&tokens, "into"),
        Some("update" | "pragma") => tokens.get(1).map(String::as_str),
        Some("create" | "alter" | "drop") => schema_target(&tokens),
        _ => None,
    };
    let verb = tokens.first().map(String::as_str).unwrap_or("unknown");
    (
        statement_type,
        target.map_or_else(|| verb.to_string(), |target| format!("{verb}.{target}")),
    )
}

fn schema_target(tokens: &[String]) -> Option<&str> {
    let kind = tokens
        .iter()
        .position(|token| matches!(token.as_str(), "table" | "index" | "trigger"))?;
    tokens[kind + 1..]
        .iter()
        .find(|token| !matches!(token.as_str(), "if" | "not" | "exists"))
        .map(String::as_str)
}

fn token_after<'a>(tokens: &'a [String], needle: &str) -> Option<&'a str> {
    tokens
        .iter()
        .position(|token| token == needle)
        .and_then(|index| tokens.get(index + 1))
        .map(String::as_str)
}

fn open_readonly_inner(path: &Path, observed: bool) -> Result<Connection, StorageError> {
    open_readonly_connection(path, observed)
}

pub(crate) fn open_readonly_connection(
    path: &Path,
    observed: bool,
) -> Result<Connection, StorageError> {
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
    if observed {
        install_flight_recorder_trace(&conn);
    }
    configure_readonly(&conn)?;
    Ok(conn)
}

pub fn run_writer<T>(
    path: &Path,
    run: impl FnOnce(&Connection) -> rusqlite::Result<T>,
) -> Result<T, StorageError> {
    let result = (|| {
        let conn = open_writable(path)?;
        let started = Instant::now();
        run(&conn)
            .map_err(|error| {
                StorageError::from_sqlite("database operation", error, started.elapsed())
            })
            .map_err(|error| diagnose_corruption(path, error))
    })();
    match &result {
        Ok(_) => crate::observability::flush_deferred_events(),
        Err(error) => record_storage_error(error),
    }
    result
}

pub(crate) fn record_storage_error(error: &StorageError) {
    crate::observability::emit_deferred(crate::observability::EventInput {
        level: crate::observability::LogLevel::Error,
        target: "sqlite",
        action: "error",
        operation_id: None,
        parent_operation_id: None,
        branch: None,
        session: None,
        message: format!("SQLite operation failed: {}", error.kind.label()),
        data_json: Some(error.observation_data_json()),
    });
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
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(path).map_err(|error| {
        StorageError::from_io(format!("identify database {}", path.display()), error)
    })?;
    Ok(Some(DatabaseIdentity {
        path: path.to_path_buf(),
        device: metadata.dev(),
        inode: metadata.ino(),
    }))
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
        let transaction =
            crate::flight_recorder::TransactionTrace::begin("storage.schema_migration");
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
                apply_additive_schema_migrations(conn)?;
                validate_complete_schema(conn)?;
                validate_foreign_keys(conn)?;
                execute_batch(conn, "commit", "commit schema validation transaction")?;
                transaction.committed();
                return Ok(None);
            }

            let next = version + 1;
            match next {
                1 => apply_complete_schema_baseline(conn)?,
                2 => apply_additive_schema_migrations(conn)?,
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
            transaction.committed();
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
    apply_additive_schema_migrations(conn)?;
    Ok(())
}

fn apply_additive_schema_migrations(conn: &Connection) -> Result<(), StorageError> {
    crate::session::migrate_worktree_session_schema(conn).map_err(migration_error)?;
    crate::opencode::migrate_runtime_schema(conn).map_err(migration_error)?;
    crate::plan_run::migrate_schema(conn).map_err(migration_error)?;
    crate::auto_flow::migrate_schema(conn).map_err(migration_error)?;
    crate::integration::migrate_schema(conn).map_err(migration_error)?;
    crate::execution::migrate_schema(conn).map_err(migration_error)?;
    crate::remote::migrate_pr_cache_schema(conn).map_err(migration_error)?;
    crate::notification::migrate_schema(conn).map_err(migration_error)?;
    Ok(())
}

fn additive_schema_current(conn: &Connection) -> Result<bool, StorageError> {
    Ok(table_has_column(conn, "pr_cache", "author")?
        && table_has_column(conn, "pending_worktree_deletion", "branch_deleted")?
        && table_has_column(conn, "active_worktree_session", "worktree_session_id")?
        && table_has_column(conn, "merge_intent", "placement")?
        && table_has_column(conn, "workflow_execution", "execution_version")?
        && table_has_column(conn, "workflow_execution", "not_before_unix_ms")?
        && table_has_column(conn, "workflow_execution", "wake_reason")?
        && table_has_column(conn, "workflow_execution", "workflow_revision")?
        && table_has_column(conn, "notification_outbox", "backend_accepted_unix_ms")?)
}

fn table_has_column(
    conn: &Connection,
    table: &'static str,
    column: &'static str,
) -> Result<bool, StorageError> {
    let started = Instant::now();
    conn.query_row(
        &format!("select count(*) from pragma_table_info('{table}') where name = ?1"),
        [column],
        |row| Ok(row.get::<_, i64>(0)? > 0),
    )
    .map_err(|error| {
        StorageError::from_sqlite(
            format!("inspect SQLite column {table}.{column}"),
            error,
            started.elapsed(),
        )
    })
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
        name: "worktree_session",
        columns: &[
            "id",
            "repo_root",
            "initial_branch",
            "initial_worktree_path",
            "created_unix_ms",
        ],
        primary_key: &["id"],
        minimum_foreign_keys: 0,
    },
    RequiredTable {
        name: "active_worktree_session",
        columns: &[
            "worktree_session_id",
            "repo_root",
            "branch",
            "worktree_path",
            "worktree_incarnation",
            "observed_unix_ms",
        ],
        primary_key: &["worktree_session_id"],
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
            "worker_id",
            "daemon_instance_id",
            "lease_expires_unix_ms",
            "heartbeat_unix_ms",
            "fencing_token",
            "executor_pid",
            "executor_process_identity",
            "requeue_requested",
            "interruption_generation",
            "recovery_decided_unix_ms",
            "execution_version",
            "not_before_unix_ms",
            "wake_reason",
            "workflow_revision",
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
    let conn = open_readonly_inner(path, true)?;
    Ok(StorageCheckReport {
        quick_check: quick_check(&conn)?,
        foreign_key_check: foreign_key_check(&conn)?,
    })
}

pub(crate) fn verify_unclean_database_readonly(
    path: &Path,
) -> Result<StorageCheckReport, StorageError> {
    let report = quick_check_readonly(path)?;
    if report.quick_check.as_slice() != ["ok"] {
        return Err(StorageError::policy(
            format!(
                "read-only quick_check after an unclean run reported: {}",
                report.quick_check.join("; ")
            ),
            StorageErrorKind::Corruption,
        ));
    }
    if !report.foreign_key_check.is_empty() {
        return Err(StorageError::policy(
            format!(
                "read-only foreign_key_check after an unclean run reported: {}",
                report.foreign_key_check.join("; ")
            ),
            StorageErrorKind::Constraint,
        ));
    }
    Ok(report)
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
    match database_file_sizes(path) {
        Ok((main, wal, shm)) => {
            println!("main_bytes = {main}");
            println!("wal_bytes = {wal}");
            println!("shm_bytes = {shm}");
        }
        Err(error) => {
            println!("main_bytes = unavailable ({error})");
            println!("wal_bytes = unavailable");
            println!("shm_bytes = unavailable");
        }
    }
    let conn = match open_readonly_inner(path, false) {
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

pub(crate) fn passive_checkpoint_status(path: &Path) -> Result<WalStatus, StorageError> {
    let conn = open_writable(path)?;
    let started = Instant::now();
    let (checkpoint_busy, checkpoint_log_frames, checkpointed_frames) = conn
        .query_row("pragma wal_checkpoint(passive)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(|error| {
            StorageError::from_sqlite("run passive WAL checkpoint", error, started.elapsed())
        })?;
    let (main_bytes, wal_bytes, shm_bytes) = database_file_sizes(path)?;
    Ok(WalStatus {
        main_bytes,
        wal_bytes,
        shm_bytes,
        checkpoint_busy,
        checkpoint_log_frames,
        checkpointed_frames,
    })
}

pub(crate) fn monitor_wal_growth(path: &Path) {
    let Ok((main_bytes, wal_bytes, shm_bytes)) = database_file_sizes(path) else {
        return;
    };
    let ratio = wal_bytes / WAL_WARNING_BYTES;
    let bucket = if ratio == 0 {
        0
    } else {
        u64::from(ratio.ilog2()) + 1
    };
    let Ok(mut warned) = WAL_WARNING_BUCKETS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
    else {
        return;
    };
    let previous = warned.get(path).copied().unwrap_or_default();
    if bucket == 0 {
        warned.remove(path);
        return;
    }
    if bucket <= previous {
        return;
    }
    warned.insert(path.to_path_buf(), bucket);
    drop(warned);

    crate::observability::emit_deferred(crate::observability::EventInput {
        level: crate::observability::LogLevel::Warn,
        target: "sqlite",
        action: "wal_growth",
        operation_id: None,
        parent_operation_id: None,
        branch: None,
        session: None,
        message: format!(
            "SQLite WAL grew to {wal_bytes} bytes; inspect checkpoint progress with `prism debug info`"
        ),
        data_json: Some(crate::observability::wal_growth_data_json(
            main_bytes,
            wal_bytes,
            shm_bytes,
            WAL_WARNING_BYTES,
            bucket,
        )),
    });
}

fn database_file_sizes(path: &Path) -> Result<(u64, u64, u64), StorageError> {
    Ok((
        file_size(path)?,
        file_size(&sidecar_path(path, "-wal"))?,
        file_size(&sidecar_path(path, "-shm"))?,
    ))
}

fn file_size(path: &Path) -> Result<u64, StorageError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(StorageError::from_io(
            format!("inspect {} size", path.display()),
            error,
        )),
    }
}

fn sidecar_path(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(suffix);
    value.into()
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
    fn sqlite_statement_names_expose_operations_without_bound_values() {
        assert_eq!(
            sqlite_statement_name("select text from plan_output_line where run_id = ?1"),
            ("query", "select.plan_output_line".to_string())
        );
        assert_eq!(
            sqlite_statement_name("BEGIN IMMEDIATE"),
            ("transaction", "begin".to_string())
        );
        assert_eq!(
            sqlite_statement_name("insert into event (message) values (?1)"),
            ("write", "insert.event".to_string())
        );
        assert_eq!(
            sqlite_statement_name("create table if not exists metadata (key text)"),
            ("schema", "create.metadata".to_string())
        );
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
    fn current_schema_reopen_applies_additive_pr_cache_migrations_before_validation() {
        let path = test_path("current-additive-pr-cache");
        {
            let conn = open_writable(&path).unwrap();
            conn.execute("alter table pr_cache rename to pr_cache_old", [])
                .unwrap();
            conn.execute_batch(
                "
                create table pr_cache (
                  branch text primary key,
                  number integer not null,
                  title text not null,
                  body text not null default '',
                  url text not null,
                  state text not null,
                  review_decision text not null,
                  requested_reviewers text not null default '',
                  head_ref text not null,
                  base_ref text not null,
                  head_sha text not null,
                  updated_at text not null,
                  check_status text not null,
                  merge_state_status text not null default '',
                  comment_count integer not null default 0,
                  merged integer not null,
                  draft integer not null,
                  last_refreshed text not null,
                  refreshed_unix_ms integer not null,
                  observation_error text
                );
                drop table pr_cache_old;
                pragma user_version = 1;
                ",
            )
            .unwrap();
        }

        let conn = open_writable(&path).unwrap();
        let has_author: bool = conn
            .query_row(
                "select count(*) from pragma_table_info('pr_cache') where name = 'author'",
                [],
                |row| Ok(row.get::<_, i64>(0)? > 0),
            )
            .unwrap();

        assert!(has_author);
        drop(conn);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn current_schema_reopen_creates_pending_worktree_deletion_table_additively() {
        let path = test_path("current-additive-worktree-deletion");
        {
            let conn = open_writable(&path).unwrap();
            conn.execute("drop table pending_worktree_deletion", [])
                .unwrap();
        }

        let conn = open_writable(&path).unwrap();
        let table_count: i64 = conn
            .query_row(
                "select count(*) from sqlite_master
                 where type = 'table' and name = 'pending_worktree_deletion'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(table_count, 1);
        drop(conn);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn version_one_database_migrates_durable_scheduling_columns() {
        let path = test_path("worker-scheduling-v1");
        {
            let conn = open_writable(&path).unwrap();
            conn.execute(
                "insert into workflow_execution (
                   workflow_kind, run_id, dispatch_state, fencing_token,
                   interruption_generation, created_unix_ms, updated_unix_ms
                 ) values ('plan', 'legacy-plan', 'paused', 3, 0, 1, 1)",
                [],
            )
            .unwrap();
            conn.execute_batch(
                "drop index workflow_execution_dispatch_idx;
                 alter table workflow_execution drop column execution_version;
                 alter table workflow_execution drop column not_before_unix_ms;
                 alter table workflow_execution drop column wake_reason;
                 alter table workflow_execution drop column workflow_revision;
                 pragma user_version = 1;",
            )
            .unwrap();
        }

        let conn = open_writable(&path).unwrap();
        let scheduling: (i64, Option<i64>, Option<String>, i64) = conn
            .query_row(
                "select execution_version, not_before_unix_ms, wake_reason, workflow_revision
                 from workflow_execution where run_id = 'legacy-plan'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();

        assert_eq!(user_version(&conn).unwrap(), CURRENT_SCHEMA_VERSION);
        assert_eq!(scheduling, (1, None, None, 0));
        drop(conn);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn current_database_adds_durable_notification_outbox() {
        let path = test_path("notification-outbox-migration");
        {
            let conn = open_writable(&path).unwrap();
            conn.execute_batch(
                "drop table notification_outbox;
                 drop table notification_session;
                 pragma user_version = 1;",
            )
            .unwrap();
        }

        let conn = open_writable(&path).unwrap();
        let version: u32 = conn
            .query_row("pragma user_version", [], |row| row.get(0))
            .unwrap();
        let outbox_count: i64 = conn
            .query_row(
                "select count(*) from sqlite_master
                  where type = 'table' and name = 'notification_outbox'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        assert_eq!(outbox_count, 1);
        drop(conn);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn wal_growth_warnings_are_sparse_and_reset_after_shrink() {
        let _ = crate::observability::take_captured_events();
        let path = test_path("wal-growth-warning");
        drop(open_writable(&path).unwrap());
        let wal_path = sidecar_path(&path, "-wal");
        let wal = fs::File::create(&wal_path).unwrap();
        wal.set_len(WAL_WARNING_BYTES + 1).unwrap();

        monitor_wal_growth(&path);
        monitor_wal_growth(&path);

        let first = crate::observability::take_captured_events();
        assert_eq!(
            first
                .iter()
                .filter(|event| event.action == "wal_growth")
                .count(),
            1
        );

        wal.set_len(0).unwrap();
        monitor_wal_growth(&path);
        wal.set_len(WAL_WARNING_BYTES + 1).unwrap();
        monitor_wal_growth(&path);

        let reset = crate::observability::take_captured_events();
        assert_eq!(
            reset
                .iter()
                .filter(|event| event.action == "wal_growth")
                .count(),
            1
        );
        drop(wal);
        let _ = fs::remove_file(wal_path);
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
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn validation_cache_does_not_alias_reused_inodes_at_different_paths() {
        let first = DatabaseIdentity {
            path: PathBuf::from("first.db"),
            device: 1,
            inode: 2,
        };
        let second = DatabaseIdentity {
            path: PathBuf::from("second.db"),
            device: 1,
            inode: 2,
        };

        assert_ne!(first, second);
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

        assert_eq!(
            applied.load(Ordering::SeqCst),
            CURRENT_SCHEMA_VERSION as usize
        );
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
        let _ = crate::observability::take_captured_events();
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
        let diagnostic: serde_json::Value =
            serde_json::from_str(&error.observation_data_json()).unwrap();
        assert_eq!(diagnostic["kind"], "busy");
        assert_eq!(diagnostic["primary_code"], rusqlite::ffi::SQLITE_BUSY);
        assert_eq!(diagnostic["extended_code"], error.extended_code().unwrap());
        assert!(diagnostic["busy_ms"].as_i64().is_some());
        let events = crate::observability::take_captured_events()
            .into_iter()
            .filter(|event| event.target == "sqlite" && event.action == "error")
            .filter_map(|event| event.data_json)
            .map(|data| serde_json::from_str::<serde_json::Value>(&data).unwrap())
            .filter(|data| {
                data["kind"] == "busy" && data["primary_code"] == rusqlite::ffi::SQLITE_BUSY
            })
            .count();
        assert_eq!(events, 1);
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
