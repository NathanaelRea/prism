use std::error::Error;
use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub(crate) enum DatabaseError {
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
            Self::SetPermissions { path, source } => write!(
                formatter,
                "set owner-only permissions on {}: {source}",
                path.display()
            ),
            Self::CreateDirectory { path, source } => write!(
                formatter,
                "create database directory {}: {source}",
                path.display()
            ),
            Self::Connect { path, source } => {
                write!(formatter, "open database {}: {source}", path.display())
            }
            Self::Migrate(source) => write!(formatter, "apply database migrations: {source}"),
            Self::Query(source) => write!(formatter, "database operation: {source}"),
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
            | Self::SetPermissions { source, .. }
            | Self::Runtime(source) => Some(source),
            Self::Connect { source, .. } => Some(source),
            Self::Migrate(source) => Some(source),
            Self::Query(source) => Some(source),
            Self::Integrity { .. } | Self::InvalidValue { .. } => None,
        }
    }
}
