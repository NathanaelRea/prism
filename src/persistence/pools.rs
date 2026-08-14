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
    super::adoption::adopt_historical_repository_database(path, &REPOSITORY_MIGRATOR).await?;
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);

    fn database_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "prism-{label}-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    async fn open_connection(path: &Path) -> SqliteConnection {
        SqliteConnection::connect_with(&options(path, true, false).unwrap())
            .await
            .unwrap()
    }

    fn released_migration_history(applied: usize) -> String {
        let rows = [
            "insert into _sqlx_migrations (version, description, success, checksum, execution_time) values (1, 'initial', 1, X'AFA7799DCF872B465A2F56538B06E5AF6023B8B55256C1BC3A4136A046047F6B73AAD5EE40554F922C62D19CF40EEC4C', 0);",
            "insert into _sqlx_migrations (version, description, success, checksum, execution_time) values (2, 'drop legacy workflows', 1, X'E0A2ACB7A8032D00C9869FEAA578021DBCF1CF865568BFFA97FB82FCDB84237EC19B63895D6C589E5585D6839ECFEBC1', 0);",
        ];
        format!(
            "create table _sqlx_migrations (version bigint primary key, description text not null, installed_on timestamp not null default current_timestamp, success boolean not null, checksum blob not null, execution_time bigint not null);{}",
            rows[..applied].join("")
        )
    }

    async fn assert_released_sqlx_upgrade(applied: usize) {
        let path = database_path(&format!("sqlx-upgrade-v{applied}"));
        let backup = path.with_extension("db.pre-migration-rebaseline-backup");
        let mut connection = open_connection(&path).await;
        sqlx::raw_sql(include_str!("fixtures/released_repository_0001.sql"))
            .execute(&mut connection)
            .await
            .unwrap();
        if applied == 2 {
            sqlx::raw_sql(include_str!("fixtures/released_repository_0002.sql"))
                .execute(&mut connection)
                .await
                .unwrap();
        }
        sqlx::raw_sql(&released_migration_history(applied))
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("insert into metadata (key, value) values ('preserved', 'yes')")
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();

        initialize_repository_database(&path).await.unwrap();

        let mut connection = open_connection(&path).await;
        let history: Vec<(i64, String)> = sqlx::query_as(
            "select version, hex(checksum) from _sqlx_migrations where success order by version",
        )
        .fetch_all(&mut connection)
        .await
        .unwrap();
        assert_eq!(
            history,
            [
                (1, "C72EACE717551A3E3EAA63642B38BDFC9EBCD840B940686BECE9C81BC7AC3C2B24518C4F4AA7239654345D4FEA4B7D2F".into()),
                (2, "5EDB6152EA6D11D90A7CE3A1E2239674C6F6DAE299F631EE30F894137694BD6D532A8B5F8CFBBE57FF8981C35B2C6C3C".into()),
            ]
        );
        let preserved: String =
            sqlx::query_scalar("select value from metadata where key = 'preserved'")
                .fetch_one(&mut connection)
                .await
                .unwrap();
        assert_eq!(preserved, "yes");
        connection.close().await.unwrap();
        assert!(backup.exists());

        let before = std::fs::metadata(&backup).unwrap().modified().unwrap();
        initialize_repository_database(&path).await.unwrap();
        assert_eq!(
            std::fs::metadata(&backup).unwrap().modified().unwrap(),
            before
        );

        let _ = std::fs::remove_file(backup);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn upgrades_released_sqlx_repository_schemas() {
        crate::async_runtime::block_on(async {
            assert_released_sqlx_upgrade(1).await;
            assert_released_sqlx_upgrade(2).await;
        })
        .unwrap();
    }

    #[test]
    fn adopts_the_released_v2_schema_and_promotes_policy_cache() {
        let path = database_path("v2-adoption");
        crate::async_runtime::block_on(async {
            let mut connection = open_connection(&path).await;
            sqlx::raw_sql(include_str!(
                "../../tests/fixtures/sql/repository-v2-progressive.sql"
            ))
            .execute(&mut connection)
            .await
            .unwrap();
            sqlx::query(
                "insert into repo_policy_cache_v2 (provider, canonical_host, project_path, project_path_key, target_branch, repo_remote, refreshed_unix_ms) values ('github', 'github.com', 'Acme/Prism', 'acme/prism', 'main', 'origin', 42)",
            )
            .execute(&mut connection)
            .await
            .unwrap();
            sqlx::query(
                "insert into pr_cache (branch, number, title, url, state, review_decision, head_ref, base_ref, head_sha, updated_at, check_status, merged, draft, last_refreshed, refreshed_unix_ms, provider, canonical_host, project_path, native_cr_id, display_number, source_provider, source_canonical_host, source_project_path, target_provider, target_canonical_host, target_project_path, identity_complete) values ('feature/cache', 7, 'Cached', 'https://example.test/7', 'OPEN', '', 'feature/cache', 'main', 'abc', '', '', 0, 0, '', 42, 'github', 'github.com', 'Acme/Prism', '7', 7, 'github', 'github.com', 'Acme/Prism', 'github', 'github.com', 'Acme/Prism', 1)",
            )
            .execute(&mut connection)
            .await
            .unwrap();
            connection.close().await.unwrap();

            let backup = path.with_extension("db.pre-sqlx-backup");
            std::fs::write(&backup, "stale backup").unwrap();
            initialize_repository_database(&path).await.unwrap();

            let mut connection = open_connection(&path).await;
            let policy: (String, String, i64) = sqlx::query_as(
                "select project_path, project_path_key, refreshed_unix_ms from repo_policy_cache",
            )
            .fetch_one(&mut connection)
            .await
            .unwrap();
            assert_eq!(policy, ("Acme/Prism".into(), "acme/prism".into(), 42));
            let requested_reviewers: String =
                sqlx::query_scalar("select requested_reviewers from pr_cache")
                    .fetch_one(&mut connection)
                    .await
                    .unwrap();
            assert_eq!(requested_reviewers, "[]");
            let legacy_tables: i64 = sqlx::query_scalar(
                "select count(*) from sqlite_master where type = 'table' and name in ('repo_policy_cache_v2', 'auto_run', 'plan_run', 'workflow_execution')",
            )
            .fetch_one(&mut connection)
            .await
            .unwrap();
            assert_eq!(legacy_tables, 0);
            connection.close().await.unwrap();

            let mut backup_connection =
                SqliteConnection::connect_with(&options(&backup, false, true).unwrap())
                    .await
                    .unwrap();
            let backup_version: i64 = sqlx::query_scalar("pragma user_version")
                .fetch_one(&mut backup_connection)
                .await
                .unwrap();
            assert_eq!(backup_version, 2);
            backup_connection.close().await.unwrap();
        })
        .unwrap();
        let _ = std::fs::remove_file(path.with_extension("db.pre-sqlx-backup"));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(windows)]
    #[test]
    fn windows_database_reparse_target_is_rejected_before_migration() {
        use std::os::windows::fs::symlink_file;

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
