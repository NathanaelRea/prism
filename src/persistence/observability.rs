use std::path::{Path, PathBuf};

use sqlx::{Connection, SqliteConnection};

use super::database::{block_on, writable_options};
use super::error::DatabaseError;

#[derive(Clone, Debug)]
pub(crate) struct EventRecord<'a> {
    pub time_unix_ms: i64,
    pub level: &'a str,
    pub target: &'a str,
    pub action: &'a str,
    pub operation_id: Option<&'a str>,
    pub parent_operation_id: Option<&'a str>,
    pub repo: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub session: Option<&'a str>,
    pub message: &'a str,
    pub data_json: Option<&'a str>,
}

#[derive(Clone, Debug)]
pub(crate) struct StartupPhaseRecord<'a> {
    pub run_id: &'a str,
    pub phase: &'a str,
    pub time_started_unix_ms: i64,
    pub time_finished_unix_ms: Option<i64>,
    pub status: &'a str,
    pub error: Option<&'a str>,
}

#[derive(Clone, Debug)]
pub(crate) struct StartupRunRecord<'a> {
    pub id: &'a str,
    pub time_started_unix_ms: i64,
    pub repo: &'a str,
    pub version: &'a str,
}

pub(crate) struct ObservabilityStore {
    path: PathBuf,
}

impl ObservabilityStore {
    pub(crate) fn open(path: &Path) -> Result<Self, DatabaseError> {
        super::database::initialize(path)?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub(crate) fn insert_event(&self, event: &EventRecord<'_>) -> Result<(), DatabaseError> {
        let options = writable_options(&self.path, false)?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            let result = sqlx::query_file!(
                "sql/observability/insert_event.sql",
                event.time_unix_ms,
                event.level,
                event.target,
                event.action,
                event.operation_id,
                event.parent_operation_id,
                event.repo,
                event.branch,
                event.session,
                event.message,
                event.data_json,
            )
            .execute(&mut connection)
            .await
            .map(|_| ());
            finish_connection(connection, result).await
        })
    }

    pub(crate) fn insert_phase(&self, phase: &StartupPhaseRecord<'_>) -> Result<(), DatabaseError> {
        let options = writable_options(&self.path, false)?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            let result = sqlx::query_file!(
                "sql/observability/insert_startup_phase.sql",
                phase.run_id,
                phase.phase,
                phase.time_started_unix_ms,
                phase.time_finished_unix_ms,
                phase.status,
                phase.error,
            )
            .execute(&mut connection)
            .await
            .map(|_| ());
            finish_connection(connection, result).await
        })
    }

    pub(crate) fn begin_run(
        &self,
        run: &StartupRunRecord<'_>,
        stale_run_ids: &[String],
    ) -> Result<(), DatabaseError> {
        let options = writable_options(&self.path, false)?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            let result = async {
                super::database::begin_immediate_query()
                    .execute(&mut connection)
                    .await?;
                for stale_run_id in stale_run_ids {
                    sqlx::query_file!(
                        "sql/observability/mark_startup_run_unclean.sql",
                        run.time_started_unix_ms,
                        stale_run_id,
                    )
                    .execute(&mut connection)
                    .await?;
                }
                sqlx::query_file!(
                    "sql/observability/insert_startup_run.sql",
                    run.id,
                    run.time_started_unix_ms,
                    run.repo,
                    run.version,
                )
                .execute(&mut connection)
                .await?;
                super::database::commit_query()
                    .execute(&mut connection)
                    .await?;
                Ok(())
            }
            .await;
            if result.is_err() {
                let _ = super::database::rollback_query()
                    .execute(&mut connection)
                    .await;
            }
            finish_connection(connection, result).await
        })
    }

    pub(crate) fn finish_run(
        &self,
        run_id: &str,
        time_finished_unix_ms: i64,
        status: &str,
        error: Option<&str>,
    ) -> Result<(), DatabaseError> {
        let options = writable_options(&self.path, false)?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            let result = sqlx::query_file!(
                "sql/observability/finish_startup_run.sql",
                time_finished_unix_ms,
                status,
                error,
                run_id,
            )
            .execute(&mut connection)
            .await
            .map(|_| ());
            finish_connection(connection, result).await
        })
    }
}

async fn finish_connection<T>(
    connection: SqliteConnection,
    result: Result<T, sqlx::Error>,
) -> Result<T, sqlx::Error> {
    let close = connection.close().await;
    match result {
        Err(error) => Err(error),
        Ok(value) => {
            close?;
            Ok(value)
        }
    }
}
