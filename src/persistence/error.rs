use std::error::Error;
use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub(crate) enum DatabaseError {
    WrongDatabase {
        path: PathBuf,
        expected: &'static str,
    },
    UnknownHistoricalSchema {
        path: PathBuf,
        user_version: i64,
    },
    ProtectedLegacyExecution {
        path: PathBuf,
        count: i64,
    },
    LegacyWorkerActive {
        path: PathBuf,
    },
    LegacyProcessActive {
        path: PathBuf,
        pid: u32,
    },
    LegacyProcessInspection {
        path: PathBuf,
        pid: u32,
        details: String,
    },
    Backup {
        path: PathBuf,
        backup: PathBuf,
        source: std::io::Error,
    },
    SetPermissions {
        path: PathBuf,
        source: std::io::Error,
    },
    CreateDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    Connect {
        path: PathBuf,
        source: sqlx::Error,
    },
    Migrate(sqlx::migrate::MigrateError),
    Query(sqlx::Error),
    StaleClaim,
    Conflict {
        operation: &'static str,
    },
    OutputBudgetExceeded {
        attempted_bytes: usize,
        maximum_bytes: usize,
    },
    InvalidValue {
        field: &'static str,
        value: String,
    },
    Integrity {
        check: &'static str,
        details: String,
    },
    Runtime(std::io::Error),
}

impl fmt::Display for DatabaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongDatabase { path, expected } => write!(
                formatter,
                "database {} is not a {expected} database",
                path.display()
            ),
            Self::UnknownHistoricalSchema { path, user_version } => write!(
                formatter,
                "database {} has an unknown historical schema (user_version={user_version}); the original was not modified",
                path.display()
            ),
            Self::ProtectedLegacyExecution { path, count } => write!(
                formatter,
                "database {} has {count} protected queued, claimed, recovery-pending, running, waiting, or paused legacy execution record(s); resolve them with the previous Prism version before migration",
                path.display()
            ),
            Self::LegacyWorkerActive { path } => write!(
                formatter,
                "database {} still has a Prism Worker endpoint; stop the old Worker before the destructive workflow cutover",
                path.display()
            ),
            Self::LegacyProcessActive { path, pid } => write!(
                formatter,
                "database {} still references active legacy process {pid}; stop it before the destructive workflow cutover",
                path.display()
            ),
            Self::LegacyProcessInspection { path, pid, details } => write!(
                formatter,
                "cannot safely inspect legacy process {pid} referenced by {}: {details}",
                path.display()
            ),
            Self::Backup {
                path,
                backup,
                source,
            } => write!(
                formatter,
                "back up database {} to {}: {source}",
                path.display(),
                backup.display()
            ),
            Self::SetPermissions { path, source } => write!(
                formatter,
                "set owner-only permissions on {}: {source}",
                path.display()
            ),
            Self::CreateDirectory { path, source } => {
                write!(
                    formatter,
                    "create database directory {}: {source}",
                    path.display()
                )
            }
            Self::Connect { path, source } => {
                write!(formatter, "open database {}: {source}", path.display())
            }
            Self::Migrate(source) => write!(formatter, "apply database migrations: {source}"),
            Self::Query(source) => write!(formatter, "database operation: {source}"),
            Self::StaleClaim => formatter.write_str("execution claim is stale"),
            Self::Conflict { operation } => {
                write!(formatter, "database conflict during {operation}")
            }
            Self::OutputBudgetExceeded {
                attempted_bytes,
                maximum_bytes,
            } => write!(
                formatter,
                "attempt output budget exceeded: {attempted_bytes} bytes exceeds {maximum_bytes} bytes"
            ),
            Self::InvalidValue { field, value } => {
                write!(formatter, "invalid persisted value for {field}: {value}")
            }
            Self::Integrity { check, details } => {
                write!(formatter, "SQLite {check} failed: {details}")
            }
            Self::Runtime(source) => write!(formatter, "create database runtime: {source}"),
        }
    }
}

impl Error for DatabaseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CreateDirectory { source, .. }
            | Self::Backup { source, .. }
            | Self::SetPermissions { source, .. }
            | Self::Runtime(source) => Some(source),
            Self::Connect { source, .. } => Some(source),
            Self::Migrate(source) => Some(source),
            Self::Query(source) => Some(source),
            Self::WrongDatabase { .. }
            | Self::UnknownHistoricalSchema { .. }
            | Self::ProtectedLegacyExecution { .. }
            | Self::LegacyWorkerActive { .. }
            | Self::LegacyProcessActive { .. }
            | Self::LegacyProcessInspection { .. }
            | Self::StaleClaim
            | Self::Conflict { .. }
            | Self::OutputBudgetExceeded { .. }
            | Self::Integrity { .. }
            | Self::InvalidValue { .. } => None,
        }
    }
}
