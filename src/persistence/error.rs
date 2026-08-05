use std::error::Error;
use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub(crate) enum DatabaseError {
    IncompatibleFormat {
        path: PathBuf,
    },
    WrongDatabase {
        path: PathBuf,
        expected: &'static str,
    },
    UnknownHistoricalSchema {
        path: PathBuf,
        user_version: i64,
    },
    MissingMigrationBaseline,
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
    InspectOwnership {
        path: PathBuf,
        source: sqlx::Error,
    },
    Configure {
        setting: &'static str,
        expected: String,
        actual: String,
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
            Self::IncompatibleFormat { path } => write!(
                formatter,
                "database {} has an incompatible or unknown format; the original was not modified",
                path.display()
            ),
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
            Self::MissingMigrationBaseline => {
                formatter.write_str("repository migration history has no baseline")
            }
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
            Self::InspectOwnership { path, source } => write!(
                formatter,
                "inspect SQLx ownership of database {}: {source}",
                path.display()
            ),
            Self::Configure {
                setting,
                expected,
                actual,
            } => write!(
                formatter,
                "SQLite policy mismatch for {setting}: expected {expected}, got {actual}"
            ),
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
            Self::Connect { source, .. } | Self::InspectOwnership { source, .. } => Some(source),
            Self::Migrate(source) => Some(source),
            Self::Query(source) => Some(source),
            Self::IncompatibleFormat { .. }
            | Self::WrongDatabase { .. }
            | Self::UnknownHistoricalSchema { .. }
            | Self::MissingMigrationBaseline
            | Self::StaleClaim
            | Self::Conflict { .. }
            | Self::OutputBudgetExceeded { .. }
            | Self::Configure { .. }
            | Self::Integrity { .. }
            | Self::InvalidValue { .. } => None,
        }
    }
}
