use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use sqlx::{Row, SqliteConnection};

pub const WRITER_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

static VALIDATED_DATABASES: OnceLock<Mutex<HashSet<DatabaseIdentity>>> = OnceLock::new();
static WAL_WARNING_BUCKETS: OnceLock<Mutex<BTreeMap<PathBuf, u64>>> = OnceLock::new();

const WAL_WARNING_BYTES: u64 = 64 * 1024 * 1024;
const SQLITE_BUSY: i32 = 5;
const SQLITE_LOCKED: i32 = 6;
const SQLITE_READONLY: i32 = 8;
const SQLITE_IOERR: i32 = 10;
const SQLITE_CORRUPT: i32 = 11;
const SQLITE_FULL: i32 = 13;
const SQLITE_CANTOPEN: i32 = 14;
const SQLITE_CONSTRAINT: i32 = 19;
const SQLITE_NOTADB: i32 = 26;

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
    Sqlx(sqlx::Error),
    Io(std::io::Error),
    Database(crate::persistence::error::DatabaseError),
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

    fn from_sqlx(context: impl Into<String>, source: sqlx::Error, elapsed: Duration) -> Self {
        let extended_code = sqlite_error_code(&source);
        let primary_code = extended_code.map(|code| code & 0xff);
        let kind = classify_sqlite_code(primary_code);
        Self {
            context: context.into(),
            kind,
            primary_code,
            extended_code,
            busy_elapsed: (kind == StorageErrorKind::Busy).then_some(elapsed),
            source: Some(Box::new(StorageErrorSource::Sqlx(source))),
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

    fn from_database(source: crate::persistence::error::DatabaseError, elapsed: Duration) -> Self {
        let code = database_error_code(&source);
        let mut kind = classify_sqlite_code(code.map(|value| value & 0xff));
        if code.is_none() {
            kind = match &source {
                crate::persistence::error::DatabaseError::Integrity { .. } => {
                    StorageErrorKind::Corruption
                }
                crate::persistence::error::DatabaseError::Connect { .. } => {
                    StorageErrorKind::CannotOpen
                }
                crate::persistence::error::DatabaseError::CreateDirectory { .. }
                | crate::persistence::error::DatabaseError::Runtime(_) => StorageErrorKind::Io,
                _ => StorageErrorKind::Other,
            };
        }
        Self {
            context: "initialize database".to_string(),
            kind,
            primary_code: code.map(|value| value & 0xff),
            extended_code: code,
            busy_elapsed: (kind == StorageErrorKind::Busy).then_some(elapsed),
            source: Some(Box::new(StorageErrorSource::Database(source))),
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

fn classify_sqlite_code(code: Option<i32>) -> StorageErrorKind {
    match code {
        Some(SQLITE_BUSY) => StorageErrorKind::Busy,
        Some(SQLITE_LOCKED) => StorageErrorKind::Locked,
        Some(SQLITE_CONSTRAINT) => StorageErrorKind::Constraint,
        Some(SQLITE_CORRUPT | SQLITE_NOTADB) => StorageErrorKind::Corruption,
        Some(SQLITE_IOERR) => StorageErrorKind::Io,
        Some(SQLITE_READONLY) => StorageErrorKind::ReadOnly,
        Some(SQLITE_FULL) => StorageErrorKind::Full,
        Some(SQLITE_CANTOPEN) => StorageErrorKind::CannotOpen,
        _ => StorageErrorKind::Other,
    }
}

fn sqlite_error_code(error: &sqlx::Error) -> Option<i32> {
    error.as_database_error()?.code()?.parse::<i32>().ok()
}

fn database_error_code(error: &crate::persistence::error::DatabaseError) -> Option<i32> {
    let source = match error {
        crate::persistence::error::DatabaseError::Connect { source, .. }
        | crate::persistence::error::DatabaseError::InspectOwnership { source, .. }
        | crate::persistence::error::DatabaseError::Query(source) => source,
        _ => return None,
    };
    sqlite_error_code(source)
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.context)?;
        if let Some(source) = &self.source {
            match source.as_ref() {
                StorageErrorSource::Sqlx(source) => write!(formatter, ": {source}")?,
                StorageErrorSource::Io(source) => write!(formatter, ": {source}")?,
                StorageErrorSource::Database(source) => write!(formatter, ": {source}")?,
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
            StorageErrorSource::Sqlx(source) => Some(source),
            StorageErrorSource::Io(source) => Some(source),
            StorageErrorSource::Database(source) => Some(source),
        }
    }
}

fn open_writable(path: &Path) -> Result<SqliteConnection, StorageError> {
    let started = Instant::now();
    let result = open_writable_inner(path).map_err(|error| diagnose_corruption(path, error));
    record_open("writable", started.elapsed(), &result);
    result
}

fn open_writable_inner(path: &Path) -> Result<SqliteConnection, StorageError> {
    let identity = database_identity(path)?;
    let initialized = match identity.as_ref() {
        Some(identity) => database_identity_is_validated(identity)?,
        None => false,
    };
    let started = Instant::now();
    let connection = if initialized {
        crate::persistence::database::connect_writable(path)
    } else {
        let connection = crate::persistence::database::open_writable(path);
        if connection.is_ok() {
            mark_database_validated(database_identity(path)?)?;
        }
        connection
    };
    connection.map_err(|error| StorageError::from_database(error, started.elapsed()))
}

fn open_readonly(path: &Path) -> Result<SqliteConnection, StorageError> {
    let started = Instant::now();
    let result = open_readonly_inner(path).map_err(|error| diagnose_corruption(path, error));
    record_open("readonly", started.elapsed(), &result);
    result
}

fn open_readonly_inner(path: &Path) -> Result<SqliteConnection, StorageError> {
    if !path.exists() {
        return Err(StorageError::policy(
            format!("database {} does not exist", path.display()),
            StorageErrorKind::CannotOpen,
        ));
    }
    let started = Instant::now();
    crate::persistence::database::open_readonly(path)
        .map_err(|error| StorageError::from_database(error, started.elapsed()))
}

fn record_open(
    access: &'static str,
    elapsed: Duration,
    result: &Result<SqliteConnection, StorageError>,
) {
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

pub fn prepare_writable(path: &Path) -> Result<(), StorageError> {
    open_writable(path).map(drop)
}

pub fn verify_readonly(path: &Path) -> Result<(), StorageError> {
    open_readonly(path).map(drop)
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

fn diagnose_corruption(path: &Path, mut error: StorageError) -> StorageError {
    if error.kind != StorageErrorKind::Corruption {
        return error;
    }
    match quick_check_readonly(path) {
        Ok(report) => error.corruption_check = Some(Box::new(report)),
        Err(check_error) => {
            error.corruption_check_error = Some(check_error.to_string().into_boxed_str());
        }
    }
    error
}

fn database_identity(path: &Path) -> Result<Option<DatabaseIdentity>, StorageError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(StorageError::from_io(
                format!("identify database {}", path.display()),
                error,
            ));
        }
    };
    Ok(Some(DatabaseIdentity {
        path: path.to_path_buf(),
        device: metadata.dev(),
        inode: metadata.ino(),
    }))
}

fn database_identity_is_validated(identity: &DatabaseIdentity) -> Result<bool, StorageError> {
    VALIDATED_DATABASES
        .get_or_init(|| Mutex::new(HashSet::new()))
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
    VALIDATED_DATABASES
        .get_or_init(|| Mutex::new(HashSet::new()))
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

fn journal_mode(connection: &mut SqliteConnection) -> Result<String, StorageError> {
    let started = Instant::now();
    run_sqlx(async {
        // SQLX_RUNTIME_SQL: storage diagnostics use SQLite's runtime-only PRAGMA interface.
        sqlx::query_scalar::<_, String>("pragma journal_mode")
            .fetch_one(connection)
            .await
    })
    .map_err(|error| StorageError::from_sqlx("read SQLite journal_mode", error, started.elapsed()))
}

pub fn quick_check_readonly(path: &Path) -> Result<StorageCheckReport, StorageError> {
    let mut connection = open_readonly_inner(path)?;
    Ok(StorageCheckReport {
        quick_check: quick_check(&mut connection)?,
        foreign_key_check: foreign_key_check(&mut connection)?,
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

fn quick_check(connection: &mut SqliteConnection) -> Result<Vec<String>, StorageError> {
    let started = Instant::now();
    run_sqlx(async {
        // SQLX_RUNTIME_SQL: integrity PRAGMAs expose dynamic result schemas unsupported by macros.
        sqlx::query_scalar::<_, String>("pragma quick_check")
            .fetch_all(connection)
            .await
    })
    .map_err(|error| StorageError::from_sqlx("run quick_check", error, started.elapsed()))
}

fn foreign_key_check(connection: &mut SqliteConnection) -> Result<Vec<String>, StorageError> {
    let started = Instant::now();
    run_sqlx(async {
        // SQLX_RUNTIME_SQL: integrity PRAGMAs expose dynamic result schemas unsupported by macros.
        let rows = sqlx::query("pragma foreign_key_check")
            .fetch_all(connection)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(format!(
                    "table={} rowid={} parent={} fk_index={}",
                    row.try_get::<String, _>(0)?,
                    row.try_get::<Option<i64>, _>(1)?
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "null".to_string()),
                    row.try_get::<String, _>(2)?,
                    row.try_get::<i64, _>(3)?,
                ))
            })
            .collect()
    })
    .map_err(|error| StorageError::from_sqlx("run foreign_key_check", error, started.elapsed()))
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
    let mut connection = match open_readonly_inner(path) {
        Ok(connection) => connection,
        Err(error) => {
            println!("journal_mode = unavailable");
            println!("integrity_check:");
            println!("  ERROR: {error}");
            println!("foreign_key_check:");
            println!("  ERROR: database could not be opened read-only");
            return Err(error);
        }
    };
    let mut first_error = None;
    match journal_mode(&mut connection) {
        Ok(mode) => println!("journal_mode = {mode}"),
        Err(error) => {
            println!("journal_mode = unavailable ({error})");
            first_error = Some(error);
        }
    }

    let started = Instant::now();
    let integrity: Result<Vec<String>, StorageError> = run_sqlx(async {
        // SQLX_RUNTIME_SQL: integrity PRAGMAs expose dynamic result schemas unsupported by macros.
        sqlx::query_scalar::<_, String>("pragma integrity_check")
            .fetch_all(&mut connection)
            .await
    })
    .map_err(|error| StorageError::from_sqlx("run integrity_check", error, started.elapsed()));
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
    match foreign_key_check(&mut connection) {
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
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

pub(crate) fn passive_checkpoint_status(path: &Path) -> Result<WalStatus, StorageError> {
    let mut connection = open_writable(path)?;
    let started = Instant::now();
    let (checkpoint_busy, checkpoint_log_frames, checkpointed_frames) = run_sqlx(async {
        // SQLX_RUNTIME_SQL: WAL checkpoint control is a runtime-only SQLite PRAGMA.
        sqlx::query_as::<_, (i64, i64, i64)>("pragma wal_checkpoint(passive)")
            .fetch_one(&mut connection)
            .await
    })
    .map_err(|error| {
        StorageError::from_sqlx("run passive WAL checkpoint", error, started.elapsed())
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

fn run_sqlx<T>(
    future: impl std::future::Future<Output = Result<T, sqlx::Error>>,
) -> Result<T, sqlx::Error> {
    crate::async_runtime::block_on(future).map_err(sqlx::Error::Io)?
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

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(suffix);
    value.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn writable_and_readonly_connections_enforce_policy() {
        let path = test_path("policy");
        let mut writer = open_writable(&path).unwrap();

        assert_eq!(integer_pragma(&mut writer, "foreign_keys"), 1);
        assert_eq!(integer_pragma(&mut writer, "synchronous"), 2);
        assert_eq!(text_pragma(&mut writer, "journal_mode"), "wal");
        drop(writer);

        let mut reader = open_readonly(&path).unwrap();
        assert_eq!(integer_pragma(&mut reader, "busy_timeout"), 0);
        assert_eq!(integer_pragma(&mut reader, "query_only"), 1);
        drop(reader);
        remove_database(&path);
    }

    #[test]
    fn readonly_checks_and_passive_checkpoint_report_healthy_database() {
        let path = test_path("checks");
        drop(open_writable(&path).unwrap());

        let report = quick_check_readonly(&path).unwrap();
        let wal = passive_checkpoint_status(&path).unwrap();

        assert_eq!(report.quick_check, ["ok"]);
        assert!(report.foreign_key_check.is_empty());
        assert!(wal.main_bytes > 0);
        assert!(wal.checkpoint_busy >= 0);
        remove_database(&path);
    }

    fn integer_pragma(connection: &mut SqliteConnection, name: &str) -> i64 {
        run_sqlx(async {
            sqlx::query_scalar::<_, i64>(&format!("pragma {name}"))
                .fetch_one(connection)
                .await
        })
        .unwrap()
    }

    fn text_pragma(connection: &mut SqliteConnection, name: &str) -> String {
        run_sqlx(async {
            sqlx::query_scalar::<_, String>(&format!("pragma {name}"))
                .fetch_one(connection)
                .await
        })
        .unwrap()
    }

    fn test_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "prism-storage-{label}-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn remove_database(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(sidecar_path(path, "-wal"));
        let _ = fs::remove_file(sidecar_path(path, "-shm"));
    }
}
