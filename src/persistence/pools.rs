#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use sqlx::sqlite::{SqliteConnectOptions, SqliteSynchronous};
use sqlx::{Connection, SqliteConnection};

use super::error::DatabaseError;

pub(super) const WRITER_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
static REPOSITORY_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/repository");

pub(super) async fn initialize_repository_database(path: &Path) -> Result<(), DatabaseError> {
    initialize_repository_database_for(path, WRITER_BUSY_TIMEOUT).await
}

async fn initialize_repository_database_for(
    path: &Path,
    writer_lock_timeout: Duration,
) -> Result<(), DatabaseError> {
    prepare_parent(path)?;
    secure_existing_database(path)?;
    let budget = LockRetryBudget::new(writer_lock_timeout);
    retry_historical_adoption_with_budget(path, &REPOSITORY_MIGRATOR, budget).await?;
    retry_migration_with_budget(path, &REPOSITORY_MIGRATOR, budget).await?;
    set_owner_only(path)
}

// WAL recovery can return SQLITE_BUSY_RECOVERY without invoking SQLite's busy handler, so
// initialization retries those transient lock codes within the normal writer timeout.
#[derive(Clone, Copy)]
struct LockRetryBudget {
    started: Instant,
    timeout: Duration,
}

impl LockRetryBudget {
    fn new(timeout: Duration) -> Self {
        Self {
            started: Instant::now(),
            timeout,
        }
    }

    fn remaining(self) -> Option<Duration> {
        self.timeout
            .checked_sub(self.started.elapsed())
            .filter(|remaining| !remaining.is_zero())
    }

    async fn wait(self) -> bool {
        let Some(remaining) = self.remaining() else {
            return false;
        };
        tokio::time::sleep(remaining.min(Duration::from_millis(10))).await;
        self.remaining().is_some()
    }
}

#[cfg(test)]
async fn retry_historical_adoption(
    path: &Path,
    migrator: &sqlx::migrate::Migrator,
) -> Result<(), DatabaseError> {
    retry_historical_adoption_for(path, migrator, WRITER_BUSY_TIMEOUT).await
}

#[cfg(test)]
async fn retry_historical_adoption_for(
    path: &Path,
    migrator: &sqlx::migrate::Migrator,
    timeout: Duration,
) -> Result<(), DatabaseError> {
    retry_historical_adoption_with_budget(path, migrator, LockRetryBudget::new(timeout)).await
}

async fn retry_historical_adoption_with_budget(
    path: &Path,
    migrator: &sqlx::migrate::Migrator,
    budget: LockRetryBudget,
) -> Result<(), DatabaseError> {
    let mut pending_lock = None;
    loop {
        if budget.remaining().is_none()
            && let Some(error) = pending_lock.take()
        {
            return Err(error);
        }
        let attempt_timeout = budget.remaining().unwrap_or(Duration::ZERO);
        match super::adoption::adopt_historical_repository_database(path, migrator, attempt_timeout)
            .await
        {
            Err(error) if database_error_is_transient_lock(&error) => {
                if !budget.wait().await {
                    return Err(error);
                }
                pending_lock = Some(error);
            }
            result => return result,
        }
    }
}

#[cfg(test)]
async fn retry_migration_for(
    path: &Path,
    migrator: &sqlx::migrate::Migrator,
    timeout: Duration,
) -> Result<(), DatabaseError> {
    retry_migration_with_budget(path, migrator, LockRetryBudget::new(timeout)).await
}

async fn retry_migration_with_budget(
    path: &Path,
    migrator: &sqlx::migrate::Migrator,
    budget: LockRetryBudget,
) -> Result<(), DatabaseError> {
    let mut pending_lock = None;
    loop {
        if budget.remaining().is_none()
            && let Some(error) = pending_lock.take()
        {
            return Err(error);
        }
        match migrate(path, migrator, budget).await {
            Err(error) if database_error_is_transient_lock(&error) => {
                if !budget.wait().await {
                    return Err(error);
                }
                pending_lock = Some(error);
            }
            result => return result,
        }
    }
}

async fn migrate(
    path: &Path,
    migrator: &sqlx::migrate::Migrator,
    budget: LockRetryBudget,
) -> Result<(), DatabaseError> {
    let mut connection = SqliteConnection::connect_with(&options_with_writer_busy_timeout(
        path,
        true,
        false,
        budget.remaining().unwrap_or(Duration::ZERO),
    )?)
    .await
    .map_err(|source| DatabaseError::Connect {
        path: path.into(),
        source,
    })?;
    let result = async {
        ensure_wal_journal_mode_with_budget(&mut connection, budget).await?;
        // WAL setup can consume most of the shared lock budget. Refresh this connection's busy
        // handler so the migrator cannot retain the larger timeout from before WAL setup.
        set_connection_busy_timeout(
            &mut connection,
            budget.remaining().unwrap_or(Duration::ZERO),
        )
        .await?;
        migrator
            .run(&mut connection)
            .await
            .map_err(DatabaseError::Migrate)?;
        validate_integrity(&mut connection).await
    }
    .await;
    close_connection(connection, result).await
}

async fn set_connection_busy_timeout(
    connection: &mut SqliteConnection,
    timeout: Duration,
) -> Result<(), DatabaseError> {
    let milliseconds = timeout.as_millis().min(i32::MAX as u128);
    // SQLX_RUNTIME_SQL: the bounded integer is generated internally for SQLite's busy PRAGMA.
    let statement = format!("pragma busy_timeout = {milliseconds}");
    sqlx::query(&statement)
        .execute(connection)
        .await
        .map(|_| ())
        .map_err(DatabaseError::Query)
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
    options_with_writer_busy_timeout(path, create, readonly, WRITER_BUSY_TIMEOUT)
}

pub(super) fn options_with_writer_busy_timeout(
    path: &Path,
    create: bool,
    readonly: bool,
    writer_busy_timeout: Duration,
) -> Result<SqliteConnectOptions, DatabaseError> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(create)
        .read_only(readonly)
        .foreign_keys(true)
        .busy_timeout(if readonly {
            Duration::ZERO
        } else {
            writer_busy_timeout
        });
    Ok(if readonly {
        options
    } else {
        // WAL is persistent and is established by the database initializers. Reasserting it on
        // every connection requires an exclusive lock that SQLite's busy handler cannot wait for.
        options.synchronous(SqliteSynchronous::Full)
    })
}

pub(super) async fn ensure_wal_journal_mode(
    connection: &mut SqliteConnection,
) -> Result<(), DatabaseError> {
    ensure_wal_journal_mode_with_budget(connection, LockRetryBudget::new(WRITER_BUSY_TIMEOUT)).await
}

async fn ensure_wal_journal_mode_with_budget(
    connection: &mut SqliteConnection,
    budget: LockRetryBudget,
) -> Result<(), DatabaseError> {
    loop {
        // SQLX_RUNTIME_SQL: SQLite journal policy is inspected and changed with PRAGMAs.
        let current = match sqlx::query_scalar::<_, String>("pragma journal_mode")
            .fetch_one(&mut *connection)
            .await
        {
            Ok(current) => current,
            Err(error) => {
                wait_for_journal_mode_lock(budget, error).await?;
                continue;
            }
        };
        if current.eq_ignore_ascii_case("wal") {
            return Ok(());
        }

        match sqlx::query_scalar::<_, String>("pragma journal_mode = wal")
            .fetch_one(&mut *connection)
            .await
        {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => return Ok(()),
            Ok(mode) => {
                if budget.wait().await {
                    continue;
                }
                return Err(DatabaseError::InvalidValue {
                    field: "journal_mode",
                    value: mode,
                });
            }
            Err(error) => wait_for_journal_mode_lock(budget, error).await?,
        }
    }
}

async fn wait_for_journal_mode_lock(
    budget: LockRetryBudget,
    error: sqlx::Error,
) -> Result<(), DatabaseError> {
    if sqlx_error_is_transient_lock(&error) && budget.wait().await {
        Ok(())
    } else {
        Err(DatabaseError::Query(error))
    }
}

fn database_error_is_transient_lock(error: &DatabaseError) -> bool {
    match error {
        DatabaseError::Connect { source, .. }
        | DatabaseError::BackupQuery { source, .. }
        | DatabaseError::Query(source) => sqlx_error_is_transient_lock(source),
        DatabaseError::Migrate(sqlx::migrate::MigrateError::Execute(source))
        | DatabaseError::Migrate(sqlx::migrate::MigrateError::ExecuteMigration(source, _)) => {
            sqlx_error_is_transient_lock(source)
        }
        _ => false,
    }
}

fn sqlx_error_is_transient_lock(error: &sqlx::Error) -> bool {
    let primary_code = error
        .as_database_error()
        .and_then(|error| error.code())
        .and_then(|code| code.parse::<i32>().ok())
        .map(|code| code & 0xff);
    matches!(primary_code, Some(5 | 6))
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

    #[test]
    fn operational_connection_open_does_not_reassert_wal_mode() {
        let path = database_path("operational-journal-mode");
        crate::async_runtime::block_on(async {
            let setup_options = SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true);
            let mut blocker = SqliteConnection::connect_with(&setup_options)
                .await
                .unwrap();
            sqlx::query("create table journal_mode_fixture(value integer)")
                .execute(&mut blocker)
                .await
                .unwrap();
            sqlx::query("begin").execute(&mut blocker).await.unwrap();
            let _: i64 = sqlx::query_scalar("select count(*) from journal_mode_fixture")
                .fetch_one(&mut blocker)
                .await
                .unwrap();

            let opening_options = options(&path, false, false).unwrap();
            let opening = SqliteConnection::connect_with(&opening_options);
            let mut connection = tokio::time::timeout(Duration::from_millis(500), opening)
                .await
                .expect("an operational connection should not wait to change journal mode")
                .expect("open an operational connection while a reader is active");
            let journal_mode: String = sqlx::query_scalar("pragma journal_mode")
                .fetch_one(&mut connection)
                .await
                .unwrap();
            assert_eq!(journal_mode, "delete");

            connection.close().await.unwrap();
            sqlx::query("rollback").execute(&mut blocker).await.unwrap();
            blocker.close().await.unwrap();
        })
        .unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn wal_initialization_waits_for_a_transient_read_lock() {
        let path = database_path("wal-initialization-lock");
        crate::async_runtime::block_on(async {
            let setup_options = SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true);
            let mut blocker = SqliteConnection::connect_with(&setup_options)
                .await
                .unwrap();
            sqlx::query("create table journal_mode_fixture(value integer)")
                .execute(&mut blocker)
                .await
                .unwrap();
            sqlx::query("begin").execute(&mut blocker).await.unwrap();
            let _: i64 = sqlx::query_scalar("select count(*) from journal_mode_fixture")
                .fetch_one(&mut blocker)
                .await
                .unwrap();

            let mut initializer =
                SqliteConnection::connect_with(&options(&path, false, false).unwrap())
                    .await
                    .unwrap();
            let mut transition = Box::pin(ensure_wal_journal_mode(&mut initializer));
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut transition)
                    .await
                    .is_err(),
                "WAL initialization should remain pending while the read lock is held"
            );

            sqlx::query("rollback").execute(&mut blocker).await.unwrap();
            blocker.close().await.unwrap();
            tokio::time::timeout(WRITER_BUSY_TIMEOUT, transition)
                .await
                .expect("WAL initialization should finish after the read lock is released")
                .expect("initialize WAL after a transient SQLite lock");
            initializer.close().await.unwrap();
        })
        .unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn repository_adoption_waits_for_a_transient_schema_lock() {
        let path = database_path("repository-adoption-lock");
        crate::async_runtime::block_on(async {
            initialize_repository_database(&path).await.unwrap();
            let mut blocker =
                SqliteConnection::connect_with(&options(&path, false, false).unwrap())
                    .await
                    .unwrap();
            let journal_mode: String = sqlx::query_scalar("pragma journal_mode = delete")
                .fetch_one(&mut blocker)
                .await
                .unwrap();
            assert_eq!(journal_mode, "delete");
            sqlx::query("begin exclusive")
                .execute(&mut blocker)
                .await
                .unwrap();

            let mut adoption = Box::pin(retry_historical_adoption(&path, &REPOSITORY_MIGRATOR));
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut adoption)
                    .await
                    .is_err(),
                "repository adoption should remain pending while the schema lock is held"
            );

            sqlx::query("rollback").execute(&mut blocker).await.unwrap();
            blocker.close().await.unwrap();
            tokio::time::timeout(WRITER_BUSY_TIMEOUT, adoption)
                .await
                .expect("repository adoption should finish after the schema lock is released")
                .expect("inspect repository schema after a transient SQLite lock");
        })
        .unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn repository_initialization_shares_one_lock_retry_budget() {
        let path = database_path("shared-initialization-lock-budget");
        crate::async_runtime::block_on(async {
            initialize_repository_database(&path).await.unwrap();
            let mut blocker = open_connection(&path).await;
            let journal_mode: String = sqlx::query_scalar("pragma journal_mode = delete")
                .fetch_one(&mut blocker)
                .await
                .unwrap();
            assert_eq!(journal_mode, "delete");
            sqlx::query("begin exclusive")
                .execute(&mut blocker)
                .await
                .unwrap();

            let lock_budget = Duration::from_secs(2);
            let mut initialization =
                Box::pin(initialize_repository_database_for(&path, lock_budget));
            assert!(
                tokio::time::timeout(Duration::from_millis(1_250), &mut initialization)
                    .await
                    .is_err(),
                "historical inspection should remain blocked by the exclusive lock"
            );

            sqlx::query("rollback").execute(&mut blocker).await.unwrap();
            sqlx::query("begin immediate")
                .execute(&mut blocker)
                .await
                .unwrap();
            let release_second_lock = async move {
                // This release is after the original deadline but before a fresh migration budget
                // would expire. A shared budget must therefore fail instead of succeeding here.
                tokio::time::sleep(Duration::from_millis(1_250)).await;
                sqlx::query("rollback").execute(&mut blocker).await.unwrap();
                blocker.close().await.unwrap();
            };
            let (result, ()) = tokio::time::timeout(Duration::from_secs(6), async {
                tokio::join!(initialization, release_second_lock)
            })
            .await
            .expect("initialization and lock-release fixture should finish");
            let error = result.expect_err(
                "migration incorrectly received a fresh lock budget after historical adoption",
            );
            assert!(database_error_is_transient_lock(&error), "{error}");
        })
        .unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn migration_retry_stops_when_its_lock_budget_expires() {
        let path = database_path("migration-lock-timeout");
        crate::async_runtime::block_on(async {
            let mut setup = open_connection(&path).await;
            ensure_wal_journal_mode(&mut setup).await.unwrap();
            setup.close().await.unwrap();

            let mut blocker = open_connection(&path).await;
            sqlx::query("begin immediate")
                .execute(&mut blocker)
                .await
                .unwrap();

            let mut contender = SqliteConnection::connect_with(
                &options_with_writer_busy_timeout(&path, false, false, Duration::ZERO).unwrap(),
            )
            .await
            .unwrap();
            let source = sqlx::query("begin immediate")
                .execute(&mut contender)
                .await
                .expect_err("the fixture should expose a SQLite lock error");
            let backup_error = DatabaseError::BackupQuery {
                path: path.clone(),
                backup: path.with_extension("backup"),
                source,
            };
            assert!(database_error_is_transient_lock(&backup_error));
            contender.close().await.unwrap();

            let migration =
                retry_migration_for(&path, &REPOSITORY_MIGRATOR, Duration::from_millis(100));
            let error = tokio::time::timeout(Duration::from_secs(2), migration)
                .await
                .expect("migration retry should not start a full-timeout attempt after its budget")
                .expect_err("the write lock should outlive the migration retry budget");
            assert!(database_error_is_transient_lock(&error));

            sqlx::query("rollback").execute(&mut blocker).await.unwrap();
            blocker.close().await.unwrap();
        })
        .unwrap();
        let _ = std::fs::remove_file(path);
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
                (3, "F7C834FB7660F6AEDB29597CD63F9257E836C693B759990D9506D9B50B88AD59120AF8144CCD70ED6C6D7C7EAFC8925F".into()),
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
    fn reopens_repository_with_deferred_merge_cleanup_history() {
        let path = database_path("deferred-merge-cleanup-history");
        crate::async_runtime::block_on(async {
            let mut connection = open_connection(&path).await;
            sqlx::raw_sql(include_str!(
                "../../migrations/repository/0001_initial.sql"
            ))
            .execute(&mut connection)
            .await
            .unwrap();
            sqlx::raw_sql(include_str!(
                "../../migrations/repository/0002_drop_legacy_workflows.sql"
            ))
            .execute(&mut connection)
            .await
            .unwrap();
            sqlx::raw_sql(include_str!(
                "../../migrations/repository/0003_deferred_merge_cleanup.sql"
            ))
            .execute(&mut connection)
            .await
            .unwrap();
            sqlx::raw_sql(
                "CREATE TABLE _sqlx_migrations (version bigint primary key, description text not null, installed_on timestamp not null default current_timestamp, success boolean not null, checksum blob not null, execution_time bigint not null);\
                 INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES\
                   (1, 'initial', 1, X'C72EACE717551A3E3EAA63642B38BDFC9EBCD840B940686BECE9C81BC7AC3C2B24518C4F4AA7239654345D4FEA4B7D2F', 0),\
                   (2, 'drop legacy workflows', 1, X'5EDB6152EA6D11D90A7CE3A1E2239674C6F6DAE299F631EE30F894137694BD6D532A8B5F8CFBBE57FF8981C35B2C6C3C', 0),\
                   (3, 'deferred merge cleanup', 1, X'F7C834FB7660F6AEDB29597CD63F9257E836C693B759990D9506D9B50B88AD59120AF8144CCD70ED6C6D7C7EAFC8925F', 0);",
            )
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
            let preserved: String =
                sqlx::query_scalar("select value from metadata where key = 'preserved'")
                    .fetch_one(&mut connection)
                    .await
                    .unwrap();
            assert_eq!(preserved, "yes");
            connection.close().await.unwrap();
        })
        .unwrap();
        let _ = std::fs::remove_file(path);
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
