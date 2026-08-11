#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::{Connection, SqliteConnection};

use super::error::DatabaseError;

pub(super) const WRITER_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
static REPOSITORY_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/repository");

pub(super) async fn initialize_repository_database(path: &Path) -> Result<(), DatabaseError> {
    prepare_parent(path)?;
    secure_existing_database(path)?;
    migrate(path, &REPOSITORY_MIGRATOR).await?;
    set_owner_only(path)
}

async fn migrate(path: &Path, migrator: &sqlx::migrate::Migrator) -> Result<(), DatabaseError> {
    let mut connection = SqliteConnection::connect_with(&options(path, true, false)?)
        .await
        .map_err(|source| DatabaseError::Connect {
            path: path.into(),
            source,
        })?;
    let result = async {
        migrator
            .run(&mut connection)
            .await
            .map_err(DatabaseError::Migrate)?;
        validate_integrity(&mut connection).await
    }
    .await;
    close_connection(connection, result).await
}

pub(super) async fn close_connection<T>(
    connection: SqliteConnection,
    result: Result<T, DatabaseError>,
) -> Result<T, DatabaseError> {
    let close = connection.close().await.map_err(DatabaseError::Query);
    match result {
        Err(error) => Err(error),
        Ok(value) => {
            close?;
            Ok(value)
        }
    }
}

pub(crate) fn options(
    path: &Path,
    create: bool,
    readonly: bool,
) -> Result<SqliteConnectOptions, DatabaseError> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(create)
        .read_only(readonly)
        .foreign_keys(true)
        .busy_timeout(if readonly {
            Duration::ZERO
        } else {
            WRITER_BUSY_TIMEOUT
        });
    Ok(if readonly {
        options
    } else {
        options
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
    })
}

fn prepare_parent(path: &Path) -> Result<(), DatabaseError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| DatabaseError::CreateDirectory {
            path: parent.into(),
            source,
        })?;
        #[cfg(windows)]
        crate::system::windows_security::secure_path(parent, true).map_err(|source| {
            DatabaseError::SetPermissions {
                path: parent.into(),
                source,
            }
        })?;
    }
    Ok(())
}

fn secure_existing_database(path: &Path) -> Result<(), DatabaseError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(DatabaseError::SetPermissions {
            path: path.into(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "database path is a symbolic link",
            ),
        }),
        Ok(_) => set_owner_only(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DatabaseError::SetPermissions {
            path: path.into(),
            source,
        }),
    }
}

pub(super) fn set_owner_only(path: &Path) -> Result<(), DatabaseError> {
    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        DatabaseError::SetPermissions {
            path: path.into(),
            source,
        }
    })?;
    #[cfg(windows)]
    crate::system::windows_security::secure_path(path, false).map_err(|source| {
        DatabaseError::SetPermissions {
            path: path.into(),
            source,
        }
    })?;
    Ok(())
}

pub(super) async fn validate_integrity(
    connection: &mut SqliteConnection,
) -> Result<(), DatabaseError> {
    let quick: Vec<String> = sqlx::query_scalar("pragma quick_check")
        .fetch_all(&mut *connection)
        .await
        .map_err(DatabaseError::Query)?;
    if quick.as_slice() != ["ok"] {
        return Err(DatabaseError::Integrity {
            check: "quick_check",
            details: quick.join("; "),
        });
    }
    let foreign_keys: Vec<(String, Option<i64>, String, i64)> =
        sqlx::query_as("pragma foreign_key_check")
            .fetch_all(connection)
            .await
            .map_err(DatabaseError::Query)?;
    if let Some((table, row, parent, index)) = foreign_keys.first() {
        return Err(DatabaseError::Integrity {
            check: "foreign_key_check",
            details: format!("table={table} rowid={row:?} parent={parent} fk_index={index}"),
        });
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::fs::symlink_file;

    #[test]
    fn windows_database_reparse_target_is_rejected_before_migration() {
        let root = std::env::temp_dir().join(format!(
            "prism-database-reparse-{}-{}",
            std::process::id(),
            crate::util::timestamp_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("target.sqlite");
        let database = root.join("repository.sqlite");
        std::fs::write(&target, b"sentinel").unwrap();
        if let Err(error) = symlink_file(&target, &database) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                std::fs::remove_dir_all(root).unwrap();
                return;
            }
            panic!("create database symlink: {error}");
        }
        assert!(secure_existing_database(&database).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"sentinel");
        std::fs::remove_dir_all(root).unwrap();
    }
}
