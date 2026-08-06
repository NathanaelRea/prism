#![allow(dead_code)]

use std::path::Path;
use std::str::FromStr;

use sha2::{Digest, Sha256};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, SqliteConnection};

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|error| format!("create SQLx test runtime: {error}"))
}

async fn connect(path: &Path, read_only: bool) -> Result<SqliteConnection, String> {
    let options = SqliteConnectOptions::from_str(&path.to_string_lossy())
        .map_err(|error| format!("configure {}: {error}", path.display()))?
        .read_only(read_only)
        .create_if_missing(false)
        .foreign_keys(true);
    SqliteConnection::connect_with(&options)
        .await
        .map_err(|error| format!("open {}: {error}", path.display()))
}

pub fn install_plan_control_fixture(path: &Path, repo: &Path) -> Result<(), String> {
    let repo_root = repo.display().to_string();
    let plan_path = repo.join("plan.md").display().to_string();
    let history_plan_path = repo.join("old-plan.md").display().to_string();
    runtime()?.block_on(async {
        let mut connection = connect(path, false).await?;
        let mut transaction = connection
            .begin()
            .await
            .map_err(|error| format!("begin fixture transaction: {error}"))?;
        sqlx::query_file!(
            "sql/plan_run/test_insert_control_run.sql",
            repo_root,
            plan_path
        )
        .execute(&mut *transaction)
        .await
        .map_err(|error| format!("insert control plan: {error}"))?;
        for step in 1_i64..=2 {
            sqlx::query_file!("sql/plan_run/test_insert_control_step.sql", step)
                .execute(&mut *transaction)
                .await
                .map_err(|error| format!("insert control plan step: {error}"))?;
        }
        sqlx::query_file!("sql/workflow/test_insert_control_execution.sql")
            .execute(&mut *transaction)
            .await
            .map_err(|error| format!("insert control workflow execution: {error}"))?;
        sqlx::query_file!(
            "sql/plan_run/test_insert_history_run.sql",
            repo_root,
            history_plan_path
        )
        .execute(&mut *transaction)
        .await
        .map_err(|error| format!("insert historical plan: {error}"))?;
        sqlx::query_file!("sql/workflow/test_insert_history_execution.sql")
            .execute(&mut *transaction)
            .await
            .map_err(|error| format!("insert historical workflow execution: {error}"))?;
        transaction
            .commit()
            .await
            .map_err(|error| format!("commit fixture transaction: {error}"))
    })
}

pub fn plan_control_state(path: &Path) -> Result<(String, i64, String), String> {
    runtime()?.block_on(async {
        let mut connection = connect(path, true).await?;
        let row = sqlx::query_file!("sql/workspace/test_load_plan_control_state.sql")
            .fetch_one(&mut connection)
            .await
            .map_err(|error| format!("load plan control state: {error}"))?;
        Ok((row.status, row.pause_requested, row.dispatch_state))
    })
}

pub fn prepare_plan_recovery(path: &Path) -> Result<(), String> {
    runtime()?.block_on(async {
        let mut connection = connect(path, false).await?;
        let mut transaction = connection
            .begin()
            .await
            .map_err(|error| format!("begin recovery fixture transaction: {error}"))?;
        sqlx::query_file!("sql/plan_run/test_prepare_recovery.sql")
            .execute(&mut *transaction)
            .await
            .map_err(|error| format!("prepare plan recovery: {error}"))?;
        sqlx::query_file!("sql/workflow/test_prepare_plan_recovery.sql")
            .execute(&mut *transaction)
            .await
            .map_err(|error| format!("prepare workflow recovery: {error}"))?;
        transaction
            .commit()
            .await
            .map_err(|error| format!("commit recovery fixture transaction: {error}"))
    })
}

pub fn plan_recovery_state(path: &Path) -> Result<(String, i64), String> {
    runtime()?.block_on(async {
        let mut connection = connect(path, true).await?;
        let row = sqlx::query_file!("sql/workflow/test_load_plan_recovery_state.sql")
            .fetch_one(&mut connection)
            .await
            .map_err(|error| format!("load plan recovery state: {error}"))?;
        Ok((row.dispatch_state, row.interruption_generation))
    })
}

pub fn latest_startup_run(path: &Path) -> Result<(String, String, Option<i64>), String> {
    runtime()?.block_on(async {
        let mut connection = connect(path, true).await?;
        let row = sqlx::query_file!("sql/observability/test_load_latest_startup_run.sql")
            .fetch_one(&mut connection)
            .await
            .map_err(|error| format!("load latest startup run: {error}"))?;
        Ok((row.id, row.status, row.time_finished_unix_ms))
    })
}

pub fn opencode_server_pid(path: &Path, branch: &str) -> Result<Option<u32>, String> {
    runtime()?.block_on(async {
        let mut connection = connect(path, true).await?;
        let pid = sqlx::query_file_scalar!("sql/session/test_load_server_pid.sql", branch)
            .fetch_one(&mut connection)
            .await
            .map_err(|error| format!("load OpenCode server PID: {error}"))?;
        pid.map(u32::try_from)
            .transpose()
            .map_err(|_| format!("OpenCode server PID is out of range: {pid:?}"))
    })
}

pub fn opencode_processes(path: &Path) -> Result<Vec<(u32, u16)>, String> {
    runtime()?.block_on(async {
        let mut connection = connect(path, true).await?;
        let rows = sqlx::query_file!("sql/session/test_list_server_processes.sql")
            .fetch_all(&mut connection)
            .await
            .map_err(|error| format!("list OpenCode server processes: {error}"))?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                Some((
                    u32::try_from(row.server_pid).ok()?,
                    u16::try_from(row.server_port).ok()?,
                ))
            })
            .collect())
    })
}

pub fn database_contract(path: &Path) -> Result<(String, String), String> {
    runtime()?.block_on(async {
        let mut connection = connect(path, true).await?;
        let migrations = sqlx::query_file!("sql/database/test_list_migrations.sql")
            .fetch_all(&mut connection)
            .await
            .map_err(|error| format!("list database migrations: {error}"))?;
        let migration_contract = migrations
            .into_iter()
            .map(|row| {
                format!(
                    "{}\t{}\t{}\t{}",
                    row.version, row.description, row.success, row.checksum
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let objects = sqlx::query_file!("sql/database/test_list_schema_objects.sql")
            .fetch_all(&mut connection)
            .await
            .map_err(|error| format!("list database schema objects: {error}"))?;
        let canonical = objects
            .into_iter()
            .map(|row| {
                format!(
                    "{}\u{1f}{}\u{1f}{}\u{1f}{}",
                    row.kind, row.name, row.table_name, row.sql
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok((
            migration_contract,
            format!("{:x}", Sha256::digest(canonical.as_bytes())),
        ))
    })
}
