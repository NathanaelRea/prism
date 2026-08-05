use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::time::{Duration, Instant};

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Connection, SqliteConnection, SqlitePool};

use super::error::DatabaseError;

const WRITER_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
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
        prepare_parent(path)?;
        adopt_historical_repository_database(path).await?;
        migrate(path, &REPOSITORY_MIGRATOR).await?;
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

    pub(crate) async fn write_immediate<T>(
        &self,
        operation: impl for<'c> FnOnce(&'c mut SqliteConnection) -> WriteFuture<'c, T>,
    ) -> Result<T, DatabaseError> {
        write_immediate(&self.writer, false, operation).await
    }
}

impl WorkflowDatabase {
    pub(crate) async fn open(path: &Path) -> Result<Self, DatabaseError> {
        prepare_parent(path)?;
        reject_wrong_workflow_database(path).await?;
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
        let mut connection = self.readers.acquire().await.map_err(DatabaseError::Query)?;
        validate_integrity(&mut connection).await
    }
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
    migrator
        .run(&mut connection)
        .await
        .map_err(DatabaseError::Migrate)?;
    validate_integrity(&mut connection).await?;
    connection.close().await.map_err(DatabaseError::Query)
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

fn set_owner_only(path: &Path) -> Result<(), DatabaseError> {
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
    let workflow_identity: i64 = sqlx::query_scalar(
        "select count(*) from sqlite_master where type = 'table' and name = 'workflow_database_identity'",
    ).fetch_one(&mut connection).await.map_err(DatabaseError::Query)?;
    let repository_marker: i64 = sqlx::query_scalar(
        "select count(*) from sqlite_master where type = 'table' and name in ('workflow_execution','plan_run')",
    ).fetch_one(&mut connection).await.map_err(DatabaseError::Query)?;
    if workflow_identity == 0 && repository_marker > 0 {
        return Err(DatabaseError::WrongDatabase {
            path: path.into(),
            expected: "workflow",
        });
    }
    Ok(())
}

pub(super) async fn adopt_historical_repository_database(path: &Path) -> Result<(), DatabaseError> {
    if !path.exists()
        || std::fs::metadata(path)
            .map(|m| m.len() == 0)
            .unwrap_or(false)
    {
        return Ok(());
    }
    // Classification is deliberately read-only. A database that is unknown, corrupt, or from a
    // future Prism version must not gain a WAL, migration table, or any other side effect merely
    // because Prism inspected it.
    let mut connection = SqliteConnection::connect_with(&options(path, false, true)?)
        .await
        .map_err(|source| DatabaseError::Connect {
            path: path.into(),
            source,
        })?;
    let owned: i64 = sqlx::query_scalar(
        "select count(*) from sqlite_master where type = 'table' and name = '_sqlx_migrations'",
    )
    .fetch_one(&mut connection)
    .await
    .map_err(DatabaseError::Query)?;
    if owned == 1 {
        return Ok(());
    }

    // Released pre-SQLx databases used user_version=1 and these canonical anchor tables.
    let user_version: i64 = sqlx::query_scalar("pragma user_version")
        .fetch_one(&mut connection)
        .await
        .map_err(DatabaseError::Query)?;
    let anchors: i64 = sqlx::query_scalar(
        "select count(*) from sqlite_master where type = 'table' and name in ('metadata','plan_run','auto_run','workflow_execution','notification_outbox')",
    ).fetch_one(&mut connection).await.map_err(DatabaseError::Query)?;
    if user_version != 1 || anchors != 5 {
        return Err(DatabaseError::UnknownHistoricalSchema {
            path: path.into(),
            user_version,
        });
    }
    validate_integrity(&mut connection).await?;
    if schema_contract(&mut connection).await? != canonical_repository_schema_contract().await? {
        return Err(DatabaseError::UnknownHistoricalSchema {
            path: path.into(),
            user_version,
        });
    }

    let backup = path.with_extension("db.pre-sqlx-backup");
    if !backup.exists() {
        // `VACUUM INTO` uses SQLite's own consistent snapshot machinery, so committed WAL pages
        // are included. Copying only the main file can silently produce an incomplete recovery
        // artifact when the historical database is in WAL mode.
        let backup_name = backup.to_string_lossy().into_owned();
        sqlx::query("vacuum into ?")
            .bind(backup_name)
            .execute(&mut connection)
            .await
            .map_err(|source| DatabaseError::Backup {
                path: path.into(),
                backup: backup.clone(),
                source: std::io::Error::other(source.to_string()),
            })?;
        set_owner_only(&backup)?;
    }
    connection.close().await.map_err(DatabaseError::Query)?;

    let mut connection = SqliteConnection::connect_with(&options(path, false, false)?)
        .await
        .map_err(|source| DatabaseError::Connect {
            path: path.into(),
            source,
        })?;
    let baseline = REPOSITORY_MIGRATOR
        .iter()
        .next()
        .ok_or(DatabaseError::MissingMigrationBaseline)?;
    sqlx::query("begin immediate")
        .execute(&mut connection)
        .await
        .map_err(DatabaseError::Query)?;
    let adoption = async {
        sqlx::query("create table _sqlx_migrations (version bigint primary key, description text not null, installed_on timestamp not null default current_timestamp, success boolean not null, checksum blob not null, execution_time bigint not null)")
            .execute(&mut connection).await?;
        sqlx::query("insert into _sqlx_migrations (version, description, success, checksum, execution_time) values (?, ?, 1, ?, 0)")
            .bind(baseline.version).bind(baseline.description.as_ref()).bind(baseline.checksum.as_ref())
            .execute(&mut connection).await?;
        Ok::<_, sqlx::Error>(())
    }.await;
    match adoption {
        Ok(()) => sqlx::query("commit")
            .execute(&mut connection)
            .await
            .map(|_| ())
            .map_err(DatabaseError::Query),
        Err(source) => {
            let _ = sqlx::query("rollback").execute(&mut connection).await;
            Err(DatabaseError::Query(source))
        }
    }
}

async fn canonical_repository_schema_contract()
-> Result<Vec<(String, String, String, String)>, DatabaseError> {
    let mut canonical = SqliteConnection::connect("sqlite::memory:")
        .await
        .map_err(DatabaseError::Query)?;
    REPOSITORY_MIGRATOR
        .run(&mut canonical)
        .await
        .map_err(DatabaseError::Migrate)?;
    schema_contract(&mut canonical).await
}

async fn schema_contract(
    connection: &mut SqliteConnection,
) -> Result<Vec<(String, String, String, String)>, DatabaseError> {
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "select type, name, tbl_name, coalesce(sql, '') from sqlite_master where name not like 'sqlite_%' and name <> '_sqlx_migrations' order by type, name",
    )
    .fetch_all(connection)
    .await
    .map_err(DatabaseError::Query)?;
    Ok(rows
        .into_iter()
        .map(|(kind, name, table, sql)| {
            let normalized = sql
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase();
            (kind, name, table, normalized)
        })
        .collect())
}

async fn validate_integrity(connection: &mut SqliteConnection) -> Result<(), DatabaseError> {
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
