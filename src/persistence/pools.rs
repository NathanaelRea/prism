use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::time::{Duration, Instant};

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Connection, SqliteConnection, SqlitePool};

use super::error::DatabaseError;

pub(super) const WRITER_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
static REPOSITORY_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/repository");
static WORKFLOW_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/workflow");

type WriteFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, DatabaseError>> + Send + 'a>>;

/// Async owner of a canonical repository database. Clones share the same pools.
#[allow(
    dead_code,
    reason = "repository stores are migrated behind this pool incrementally"
)]
#[derive(Clone)]
pub(crate) struct RepositoryDatabase {
    path: PathBuf,
    writer: SqlitePool,
    readers: SqlitePool,
}

/// Async owner of the user-scoped workflow control-plane database.
#[derive(Clone)]
pub(crate) struct WorkflowDatabase {
    path: PathBuf,
    writer: SqlitePool,
    readers: SqlitePool,
}

#[allow(
    dead_code,
    reason = "repository stores are migrated behind this pool incrementally"
)]
impl RepositoryDatabase {
    pub(crate) async fn open(path: &Path) -> Result<Self, DatabaseError> {
        initialize_repository_database(path).await?;
        let (writer, readers) = open_pools(path).await?;
        Ok(Self {
            path: path.into(),
            writer,
            readers,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn readers(&self) -> &SqlitePool {
        &self.readers
    }

    pub(crate) async fn close(&self) {
        self.readers.close().await;
        self.writer.close().await;
    }

    pub(crate) async fn write_immediate<T>(
        &self,
        operation: impl for<'c> FnOnce(&'c mut SqliteConnection) -> WriteFuture<'c, T>,
    ) -> Result<T, DatabaseError> {
        write_immediate(&self.writer, false, operation).await
    }
}

pub(super) async fn initialize_repository_database(path: &Path) -> Result<(), DatabaseError> {
    prepare_parent(path)?;
    prepare_repository_cutover(path).await?;
    migrate(path, &REPOSITORY_MIGRATOR).await?;
    set_owner_only(path)
}

async fn prepare_repository_cutover(path: &Path) -> Result<(), DatabaseError> {
    prepare_repository_cutover_with_worker_socket(path, &crate::worker::socket_path()).await
}

async fn prepare_repository_cutover_with_worker_socket(
    path: &Path,
    worker_socket: &Path,
) -> Result<(), DatabaseError> {
    if !path.exists()
        || std::fs::metadata(path)
            .map(|metadata| metadata.len() == 0)
            .unwrap_or(false)
    {
        return Ok(());
    }
    let mut connection = SqliteConnection::connect_with(&options(path, false, true)?)
        .await
        .map_err(|source| DatabaseError::Connect {
            path: path.into(),
            source,
        })?;
    let result = async {
        let migration_table: i64 = sqlx::query_scalar(
            "select count(*) from sqlite_master where type = 'table' and name = '_sqlx_migrations'",
        )
        .fetch_one(&mut connection)
        .await
        .map_err(DatabaseError::Query)?;
        if migration_table == 0 {
            let user_version = sqlx::query_scalar("pragma user_version")
                .fetch_one(&mut connection)
                .await
                .map_err(DatabaseError::Query)?;
            return Err(DatabaseError::UnknownHistoricalSchema {
                path: path.into(),
                user_version,
            });
        }
        let cutover_applied: i64 = sqlx::query_scalar(
            "select count(*) from _sqlx_migrations where version >= 2 and success = 1",
        )
        .fetch_one(&mut connection)
        .await
        .map_err(DatabaseError::Query)?;
        if cutover_applied > 0 {
            return Ok(());
        }
        if worker_socket.exists() {
            return Err(DatabaseError::LegacyWorkerActive { path: path.into() });
        }
        refuse_active_legacy_processes(path, &mut connection).await?;
        let protected: i64 = sqlx::query_scalar(include_str!(
            "../../sql/database/workflow_cutover_drop_preflight.sql"
        ))
        .fetch_one(&mut connection)
        .await
        .map_err(DatabaseError::Query)?;
        if protected > 0 {
            return Err(DatabaseError::ProtectedLegacyExecution {
                path: path.into(),
                count: protected,
            });
        }
        validate_integrity(&mut connection).await?;
        let backup = path.with_extension("db.pre-workflow-cutover-backup");
        if !backup.exists() {
            sqlx::query("vacuum into ?")
                .bind(backup.to_string_lossy().into_owned())
                .execute(&mut connection)
                .await
                .map_err(|source| DatabaseError::Backup {
                    path: path.into(),
                    backup: backup.clone(),
                    source: std::io::Error::other(source.to_string()),
                })?;
            set_owner_only(&backup)?;
        }
        Ok(())
    }
    .await;
    close_connection(connection, result).await
}

async fn refuse_active_legacy_processes(
    path: &Path,
    connection: &mut SqliteConnection,
) -> Result<(), DatabaseError> {
    let processes: Vec<(i64, Option<i64>)> = sqlx::query_as(include_str!(
        "../../sql/database/workflow_cutover_drop_processes.sql"
    ))
    .fetch_all(connection)
    .await
    .map_err(DatabaseError::Query)?;
    for (stored_pid, stored_identity) in processes {
        let pid = u32::try_from(stored_pid).map_err(|_| DatabaseError::InvalidValue {
            field: "legacy process id",
            value: stored_pid.to_string(),
        })?;
        let identity = stored_identity
            .map(u64::try_from)
            .transpose()
            .map_err(|_| DatabaseError::InvalidValue {
                field: "legacy process identity",
                value: stored_identity.unwrap_or_default().to_string(),
            })?;
        let observation = crate::process::observe_process(
            crate::process::RecordedProcess::from_stored(pid, identity),
        )
        .map_err(|error| DatabaseError::LegacyProcessInspection {
            path: path.into(),
            pid,
            details: error.to_string(),
        })?;
        if matches!(
            observation,
            crate::process::ProcessObservation::RunningSameProcess
                | crate::process::ProcessObservation::RunningUnverifiable
        ) {
            return Err(DatabaseError::LegacyProcessActive {
                path: path.into(),
                pid,
            });
        }
    }
    Ok(())
}

impl WorkflowDatabase {
    pub(crate) async fn open(path: &Path) -> Result<Self, DatabaseError> {
        prepare_parent(path)?;
        reject_wrong_workflow_database(path).await?;
        prepare_workflow_cutover(path).await?;
        migrate(path, &WORKFLOW_MIGRATOR).await?;
        set_owner_only(path)?;
        let (writer, readers) = open_pools(path).await?;
        let database = Self {
            path: path.into(),
            writer,
            readers,
        };
        database.validate().await?;
        Ok(database)
    }

    pub(crate) async fn open_default() -> Result<Self, DatabaseError> {
        Self::open(&crate::util::prism_config_dir().join("workflow.db")).await
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn readers(&self) -> &SqlitePool {
        &self.readers
    }

    pub(crate) fn pool_utilization(&self) -> (u32, usize, u32, usize) {
        (
            self.writer.size(),
            self.writer.num_idle(),
            self.readers.size(),
            self.readers.num_idle(),
        )
    }

    pub(crate) async fn close(&self) {
        self.readers.close().await;
        self.writer.close().await;
    }

    pub(crate) async fn write_immediate<T>(
        &self,
        operation: impl for<'c> FnOnce(&'c mut SqliteConnection) -> WriteFuture<'c, T>,
    ) -> Result<T, DatabaseError> {
        write_immediate(&self.writer, true, operation).await
    }

    pub(crate) async fn validate(&self) -> Result<(), DatabaseError> {
        let identity: Option<String> =
            sqlx::query_scalar("select kind from workflow_database_identity where singleton = 1")
                .fetch_optional(&self.readers)
                .await
                .map_err(DatabaseError::Query)?;
        if identity.as_deref() != Some("workflow") {
            return Err(DatabaseError::WrongDatabase {
                path: self.path.clone(),
                expected: "workflow",
            });
        }
        Ok(())
    }
}

async fn prepare_workflow_cutover(path: &Path) -> Result<(), DatabaseError> {
    prepare_workflow_cutover_with_worker_socket(path, &crate::worker::socket_path()).await
}

async fn prepare_workflow_cutover_with_worker_socket(
    path: &Path,
    worker_socket: &Path,
) -> Result<(), DatabaseError> {
    if !path.exists()
        || std::fs::metadata(path)
            .map(|metadata| metadata.len() == 0)
            .unwrap_or(false)
    {
        return Ok(());
    }
    let mut connection = SqliteConnection::connect_with(&options(path, false, true)?)
        .await
        .map_err(|source| DatabaseError::Connect {
            path: path.into(),
            source,
        })?;
    let result = async {
        let cutover_applied: i64 = sqlx::query_scalar(
            "select count(*) from _sqlx_migrations where version >= 3 and success = 1",
        )
        .fetch_one(&mut connection)
        .await
        .map_err(DatabaseError::Query)?;
        if cutover_applied > 0 {
            return Ok(());
        }
        if worker_socket.exists() {
            return Err(DatabaseError::LegacyWorkerActive { path: path.into() });
        }
        validate_integrity(&mut connection).await?;
        let backup = path.with_extension("db.pre-workflow-cutover-backup");
        if !backup.exists() {
            sqlx::query("vacuum into ?")
                .bind(backup.to_string_lossy().into_owned())
                .execute(&mut connection)
                .await
                .map_err(|source| DatabaseError::Backup {
                    path: path.into(),
                    backup: backup.clone(),
                    source: std::io::Error::other(source.to_string()),
                })?;
            set_owner_only(&backup)?;
        }
        Ok(())
    }
    .await;
    close_connection(connection, result).await
}

async fn open_pools(path: &Path) -> Result<(SqlitePool, SqlitePool), DatabaseError> {
    let writer_options = options(path, false, false)?;
    let writer = SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(WRITER_BUSY_TIMEOUT)
        .connect_with(writer_options)
        .await
        .map_err(|source| DatabaseError::Connect {
            path: path.into(),
            source,
        })?;

    let reader_options = options(path, false, true)?;
    let readers = SqlitePoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(1))
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("pragma query_only = on")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(reader_options)
        .await
        .map_err(|source| DatabaseError::Connect {
            path: path.into(),
            source,
        })?;
    Ok((writer, readers))
}

async fn write_immediate<T>(
    pool: &SqlitePool,
    observe: bool,
    operation: impl for<'c> FnOnce(&'c mut SqliteConnection) -> WriteFuture<'c, T>,
) -> Result<T, DatabaseError> {
    let waiting = Instant::now();
    let mut connection = pool.acquire().await.map_err(DatabaseError::Query)?;
    let wait_micros = i64::try_from(waiting.elapsed().as_micros()).unwrap_or(i64::MAX);
    let transaction = Instant::now();
    sqlx::query("begin immediate")
        .execute(&mut *connection)
        .await
        .map_err(DatabaseError::Query)?;
    match operation(&mut connection).await {
        Ok(value) => {
            if observe {
                let transaction_micros =
                    i64::try_from(transaction.elapsed().as_micros()).unwrap_or(i64::MAX);
                let now_unix_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .and_then(|duration| i64::try_from(duration.as_millis()).ok())
                    .unwrap_or(0);
                for (name, metric) in [
                    ("writer_wait_us", wait_micros),
                    ("writer_transaction_us", transaction_micros),
                ] {
                    sqlx::query("insert into control_plane_metric (name, value, labels_json, time_unix_ms) values (?, ?, '{}', ?)")
                        .bind(name)
                        .bind(metric)
                        .bind(now_unix_ms)
                        .execute(&mut *connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                }
            }
            sqlx::query("commit")
                .execute(&mut *connection)
                .await
                .map_err(DatabaseError::Query)?;
            Ok(value)
        }
        Err(error) => {
            let _ = sqlx::query("rollback").execute(&mut *connection).await;
            Err(error)
        }
    }
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

pub(super) fn options(
    path: &Path,
    create: bool,
    readonly: bool,
) -> Result<SqliteConnectOptions, DatabaseError> {
    SqliteConnectOptions::from_str(&path.to_string_lossy())
        .map_err(|source| DatabaseError::Connect {
            path: path.into(),
            source,
        })
        .map(|options| {
            let options = options
                .create_if_missing(create)
                .read_only(readonly)
                .foreign_keys(true)
                .busy_timeout(if readonly {
                    Duration::ZERO
                } else {
                    WRITER_BUSY_TIMEOUT
                });
            if readonly {
                options
            } else {
                options
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Full)
            }
        })
}

fn prepare_parent(path: &Path) -> Result<(), DatabaseError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| DatabaseError::CreateDirectory {
            path: parent.into(),
            source,
        })?;
    }
    Ok(())
}

pub(super) fn set_owner_only(path: &Path) -> Result<(), DatabaseError> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        DatabaseError::SetPermissions {
            path: path.into(),
            source,
        }
    })
}

async fn reject_wrong_workflow_database(path: &Path) -> Result<(), DatabaseError> {
    if !path.exists()
        || std::fs::metadata(path)
            .map(|m| m.len() == 0)
            .unwrap_or(false)
    {
        return Ok(());
    }
    let mut connection = SqliteConnection::connect_with(&options(path, false, true)?)
        .await
        .map_err(|source| DatabaseError::Connect {
            path: path.into(),
            source,
        })?;
    let result = async {
        let workflow_identity: i64 = sqlx::query_scalar(
            "select count(*) from sqlite_master where type = 'table' and name = 'workflow_database_identity'",
        ).fetch_one(&mut connection).await.map_err(DatabaseError::Query)?;
        let repository_marker: i64 = sqlx::query_scalar(
            "select count(*) from sqlite_master where type = 'table' and name = '_sqlx_migrations'",
        ).fetch_one(&mut connection).await.map_err(DatabaseError::Query)?;
        if workflow_identity == 0 && repository_marker > 0 {
            return Err(DatabaseError::WrongDatabase {
                path: path.into(),
                expected: "workflow",
            });
        }
        Ok(())
    }
    .await;
    close_connection(connection, result).await
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
mod cutover_tests {
    use super::*;

    fn test_directory(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("prism-{label}-{}-{nonce}", std::process::id()))
    }

    async fn repository_at_baseline(root: &Path) -> PathBuf {
        let migrations = root.join("migrations");
        std::fs::create_dir_all(&migrations).unwrap();
        std::fs::write(
            migrations.join("0001_initial.sql"),
            include_str!("../../migrations/repository/0001_initial.sql"),
        )
        .unwrap();
        let database = root.join("repository.db");
        let migrator = sqlx::migrate::Migrator::new(migrations.as_path())
            .await
            .unwrap();
        migrate(&database, &migrator).await.unwrap();
        database
    }

    async fn complete_cutover(root: &Path, database: &Path) -> Result<(), DatabaseError> {
        prepare_repository_cutover_with_worker_socket(database, &root.join("missing-worker.sock"))
            .await?;
        migrate(database, &REPOSITORY_MIGRATOR).await?;
        set_owner_only(database)
    }

    async fn workflow_at_pre_cutover_baseline(root: &Path) -> PathBuf {
        let migrations = root.join("workflow-migrations");
        std::fs::create_dir_all(&migrations).unwrap();
        for (name, source) in [
            (
                "0001_workflow_ledger.sql",
                include_str!("../../migrations/workflow/0001_workflow_ledger.sql"),
            ),
            (
                "0002_async_control_plane.sql",
                include_str!("../../migrations/workflow/0002_async_control_plane.sql"),
            ),
        ] {
            std::fs::write(migrations.join(name), source).unwrap();
        }
        let database = root.join("workflow.db");
        let migrator = sqlx::migrate::Migrator::new(migrations.as_path())
            .await
            .unwrap();
        migrate(&database, &migrator).await.unwrap();
        database
    }

    #[tokio::test]
    async fn repository_cutover_backs_up_then_deletes_legacy_schema() {
        let root = test_directory("repository-cutover");
        let database = repository_at_baseline(&root).await;

        complete_cutover(&root, &database).await.unwrap();

        assert!(
            database
                .with_extension("db.pre-workflow-cutover-backup")
                .exists()
        );
        let mut connection =
            SqliteConnection::connect_with(&options(&database, false, false).unwrap())
                .await
                .unwrap();
        let count: i64 = sqlx::query_scalar(include_str!(
            "../../sql/database/workflow_cutover_drop_assert.sql"
        ))
        .fetch_one(&mut connection)
        .await
        .unwrap();
        connection.close().await.unwrap();
        assert_eq!(count, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn repository_cutover_refuses_protected_execution_without_backup() {
        let root = test_directory("repository-cutover-protected");
        let database = repository_at_baseline(&root).await;
        let mut connection =
            SqliteConnection::connect_with(&options(&database, false, false).unwrap())
                .await
                .unwrap();
        sqlx::raw_sql(include_str!(
            "../../sql/database/workflow_cutover_drop_seed_protected.sql"
        ))
        .execute(&mut connection)
        .await
        .unwrap();
        connection.close().await.unwrap();

        let error = complete_cutover(&root, &database).await.unwrap_err();

        assert!(matches!(
            error,
            DatabaseError::ProtectedLegacyExecution { count: 1, .. }
        ));
        assert!(
            !database
                .with_extension("db.pre-workflow-cutover-backup")
                .exists()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn repository_cutover_refuses_a_live_recorded_process() {
        let root = test_directory("repository-cutover-process");
        let database = repository_at_baseline(&root).await;
        let mut connection =
            SqliteConnection::connect_with(&options(&database, false, false).unwrap())
                .await
                .unwrap();
        sqlx::raw_sql(include_str!(
            "../../sql/database/workflow_cutover_drop_seed_process.sql"
        ))
        .execute(&mut connection)
        .await
        .unwrap();
        sqlx::query(include_str!(
            "../../sql/database/workflow_cutover_drop_seed_process_pid.sql"
        ))
        .bind(i64::from(std::process::id()))
        .execute(&mut connection)
        .await
        .unwrap();
        connection.close().await.unwrap();

        let error = complete_cutover(&root, &database).await.unwrap_err();

        assert!(matches!(
            error,
            DatabaseError::LegacyProcessActive { pid, .. } if pid == std::process::id()
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn repository_cutover_refuses_armed_or_reserved_mutation_without_backup() {
        let root = test_directory("repository-cutover-mutation");
        let database = repository_at_baseline(&root).await;
        let mut connection =
            SqliteConnection::connect_with(&options(&database, false, false).unwrap())
                .await
                .unwrap();
        sqlx::raw_sql(include_str!(
            "../../sql/database/workflow_cutover_drop_seed_mutation.sql"
        ))
        .execute(&mut connection)
        .await
        .unwrap();
        connection.close().await.unwrap();

        let error = complete_cutover(&root, &database).await.unwrap_err();

        assert!(matches!(
            error,
            DatabaseError::ProtectedLegacyExecution { count: 2, .. }
        ));
        assert!(
            !database
                .with_extension("db.pre-workflow-cutover-backup")
                .exists()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn workflow_cutover_backs_up_and_repairs_legacy_runtime_statuses() {
        let root = test_directory("workflow-cutover");
        let database = workflow_at_pre_cutover_baseline(&root).await;
        let mut connection =
            SqliteConnection::connect_with(&options(&database, false, false).unwrap())
                .await
                .unwrap();
        sqlx::query("insert into definition_snapshot (id, definition_name, revision, source, trusted, body_json, digest, created_unix_ms) values ('legacy-definition', 'legacy', '1', 'test', 1, '{}', 'legacy-digest', 1)")
            .execute(&mut connection)
            .await
            .unwrap();
        for (status, completed) in [
            ("cancelled", Some(2_i64)),
            ("failed", Some(2)),
            ("succeeded", Some(2)),
            ("runnable", None),
        ] {
            let run_id = format!("legacy-{status}");
            sqlx::query("insert into workflow_run (id, definition_snapshot_id, repository, status, created_unix_ms, updated_unix_ms, completed_unix_ms) values (?, 'legacy-definition', '/repo', ?, 1, 2, ?)")
                .bind(&run_id)
                .bind(status)
                .bind(completed)
                .execute(&mut connection)
                .await
                .unwrap();
            sqlx::query("insert into workflow_step (id, run_id, step_key, implementation, target_id, status, available_unix_ms, input_json) values (?, ?, 'legacy-step', 'legacy', 'local', ?, 1, '{}')")
                .bind(format!("{run_id}-step"))
                .bind(&run_id)
                .bind(if status == "runnable" { "runnable" } else { status })
                .execute(&mut connection)
                .await
                .unwrap();
        }
        connection.close().await.unwrap();

        prepare_workflow_cutover_with_worker_socket(&database, &root.join("missing-worker.sock"))
            .await
            .unwrap();
        migrate(&database, &WORKFLOW_MIGRATOR).await.unwrap();

        assert!(
            database
                .with_extension("db.pre-workflow-cutover-backup")
                .exists()
        );
        let mut connection =
            SqliteConnection::connect_with(&options(&database, false, true).unwrap())
                .await
                .unwrap();
        let count: i64 = sqlx::query_scalar(include_str!(
            "../../sql/database/workflow_cutover_drop_import_assert.sql"
        ))
        .fetch_one(&mut connection)
        .await
        .unwrap();
        assert_eq!(count, 0);
        let run_mismatches: i64 = sqlx::query_scalar("select count(*) from workflow_run where id like 'legacy-%' and runtime_status <> status")
            .fetch_one(&mut connection)
            .await
            .unwrap();
        let step_mismatches: i64 = sqlx::query_scalar("select count(*) from workflow_step where id like 'legacy-%' and runtime_status <> status")
            .fetch_one(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
        assert_eq!(run_mismatches, 0);
        assert_eq!(step_mismatches, 0);
        std::fs::remove_dir_all(root).unwrap();
    }
}
