#![allow(
    dead_code,
    reason = "session persistence supports optional prompt restoration"
)]

use std::path::{Path, PathBuf};

use sqlx::{Connection, FromRow, SqliteConnection};

use super::database::{block_on, writable_options};
use super::error::DatabaseError;
use crate::opencode::OpencodeRuntime;

type UnadoptedState = (Vec<(String, String)>, Vec<String>);

#[derive(Clone, Debug)]
pub(crate) struct PendingDeletion {
    pub branch: String,
    pub worktree_path: String,
    pub worktree_incarnation: String,
    pub branch_oid: Option<String>,
    pub worktree_removed: bool,
    pub branch_deleted: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct TaskMetadataRecord {
    pub prompt_summary: String,
    pub classification: String,
    pub visibility: i64,
}

#[derive(Clone, Debug)]
pub(crate) struct WorktreeHarnessRecord {
    pub worktree_path: String,
    pub worktree_incarnation: String,
    pub harness_id: String,
    pub migration_policy: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ArchivedWorktreeRecord {
    pub branch: String,
    pub worktree_path: String,
    pub classification: String,
}

#[derive(Clone, Debug)]
pub(crate) struct TaskMetadataInput<'a> {
    pub branch: &'a str,
    pub prompt_summary: &'a str,
    pub initial_prompt: &'a str,
    pub worktree: &'a str,
    pub classification: &'a str,
    pub visibility: i64,
    pub updated_unix_ms: i64,
}

#[derive(Clone, Debug)]
pub(crate) struct WorktreeHarnessInput<'a> {
    pub branch: &'a str,
    pub worktree_path: &'a str,
    pub worktree_incarnation: &'a str,
    pub harness_id: &'a str,
    pub migration_policy: &'a str,
    pub updated_unix_ms: i64,
}

#[derive(Clone, Debug)]
pub(crate) struct ArchiveInput<'a> {
    pub branch: &'a str,
    pub repo_root: &'a str,
    pub worktree_path: &'a str,
    pub archived_unix_ms: i64,
    pub classification: &'a str,
}

#[derive(FromRow)]
struct PendingDeletionRow {
    branch: String,
    worktree_path: String,
    worktree_incarnation: String,
    branch_oid: Option<String>,
    worktree_removed: i64,
    branch_deleted: i64,
}

impl From<PendingDeletionRow> for PendingDeletion {
    fn from(row: PendingDeletionRow) -> Self {
        Self {
            branch: row.branch,
            worktree_path: row.worktree_path,
            worktree_incarnation: row.worktree_incarnation,
            branch_oid: row.branch_oid,
            worktree_removed: row.worktree_removed != 0,
            branch_deleted: row.branch_deleted != 0,
        }
    }
}

#[derive(FromRow)]
struct TaskMetadataRow {
    prompt_summary: String,
    classification: String,
    visibility: i64,
}

#[derive(FromRow)]
struct WorktreeHarnessRow {
    worktree_path: String,
    worktree_incarnation: String,
    harness_id: String,
    migration_policy: String,
}

#[derive(FromRow)]
struct ArchivedWorktreeRow {
    branch: String,
    worktree_path: String,
    classification: String,
}

#[derive(FromRow)]
struct BranchPathRow {
    branch: String,
    worktree_path: String,
}

#[derive(FromRow)]
struct PersistedWorktreeRow {
    branch: String,
    worktree: String,
}

#[derive(FromRow)]
struct BranchRow {
    branch: String,
}

#[derive(FromRow)]
struct CleanupOwnerRow {
    worktree: String,
}

#[derive(FromRow)]
struct ExistsRow {
    exists: i64,
}

#[derive(FromRow)]
struct StateRow {
    state: String,
}

#[cfg(test)]
#[derive(FromRow)]
struct InitialPromptRow {
    initial_prompt: String,
}

#[derive(FromRow)]
struct HiddenSessionRow {
    branch: String,
    hidden_unix_ms: i64,
}

#[derive(FromRow)]
struct CountRow {
    count: i64,
}

pub(crate) struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    pub(crate) fn open(path: &Path) -> Result<Self, DatabaseError> {
        super::database::initialize(path)?;
        Ok(Self { path: path.into() })
    }

    fn options(&self) -> Result<sqlx::sqlite::SqliteConnectOptions, DatabaseError> {
        writable_options(&self.path, false)
    }

    pub(crate) fn load_pending_deletion(
        &self,
        branch: &str,
    ) -> Result<Option<PendingDeletion>, DatabaseError> {
        let options = self.options()?;
        let row = block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            sqlx::query_file_as!(
                PendingDeletionRow,
                "sql/session/load_pending_deletion.sql",
                branch
            )
            .fetch_optional(&mut connection)
            .await
        })?;
        Ok(row.map(Into::into))
    }

    pub(crate) fn list_pending_deletions(&self) -> Result<Vec<PendingDeletion>, DatabaseError> {
        let options = self.options()?;
        let rows = block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            sqlx::query_file_as!(PendingDeletionRow, "sql/session/list_pending_deletions.sql")
                .fetch_all(&mut connection)
                .await
        })?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub(crate) fn save_pending_deletion(
        &self,
        branch: &str,
        worktree_path: &str,
        worktree_incarnation: &str,
        branch_oid: Option<&str>,
        updated_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            let result = sqlx::query_file!(
                "sql/session/save_pending_deletion.sql",
                branch,
                worktree_path,
                worktree_incarnation,
                branch_oid,
                updated_unix_ms
            )
            .execute(&mut connection)
            .await?;
            require_one_row(result, "save pending worktree deletion")?;
            Ok(())
        })
    }

    pub(crate) fn mark_pending_phase(
        &self,
        branch: &str,
        worktree_removed: bool,
        updated_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            if worktree_removed {
                let result = sqlx::query_file!(
                    "sql/session/mark_pending_worktree_removed.sql",
                    updated_unix_ms,
                    branch
                )
                .execute(&mut connection)
                .await?;
                require_one_row(result, "mark pending worktree removal")?;
            } else {
                let result = sqlx::query_file!(
                    "sql/session/mark_pending_branch_deleted.sql",
                    updated_unix_ms,
                    branch
                )
                .execute(&mut connection)
                .await?;
                require_one_row(result, "mark pending branch deletion")?;
            }
            Ok(())
        })
    }

    pub(crate) fn persisted_worktrees(&self) -> Result<Vec<(String, String)>, DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            let rows = sqlx::query_file_as!(
                PersistedWorktreeRow,
                "sql/session/list_persisted_worktrees.sql"
            )
            .fetch_all(&mut connection)
            .await?;
            Ok(rows
                .into_iter()
                .map(|row| (row.branch, row.worktree))
                .collect())
        })
    }

    pub(crate) fn unadopted_state(&self) -> Result<UnadoptedState, DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            let runtimes =
                sqlx::query_file_as!(BranchPathRow, "sql/session/list_unadopted_runtimes.sql")
                    .fetch_all(&mut connection)
                    .await?;
            let branches =
                sqlx::query_file_as!(BranchRow, "sql/session/list_unadopted_agent_branches.sql")
                    .fetch_all(&mut connection)
                    .await?;
            Ok((
                runtimes
                    .into_iter()
                    .map(|row| (row.branch, row.worktree_path))
                    .collect(),
                branches.into_iter().map(|row| row.branch).collect(),
            ))
        })
    }

    pub(crate) fn repoint_worktree(
        &self,
        branch: &str,
        old_path: &str,
        new_path: &str,
        incarnation: &str,
        updated_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            super::database::begin_immediate_query()
                .execute(&mut connection)
                .await?;
            let result = async {
                sqlx::query_file!(
                    "sql/session/repoint_task_metadata.sql",
                    new_path,
                    branch,
                    old_path
                )
                .execute(&mut connection)
                .await?;
                sqlx::query_file!(
                    "sql/session/repoint_worktree_harness.sql",
                    new_path,
                    incarnation,
                    updated_unix_ms,
                    branch,
                    old_path
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
            result
        })
    }

    pub(crate) fn cleanup_owner(&self, branch: &str) -> Result<Option<String>, DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            sqlx::query_file_as!(CleanupOwnerRow, "sql/session/cleanup_owner.sql", branch)
                .fetch_optional(&mut connection)
                .await
                .map(|row| row.map(|row| row.worktree))
        })
    }

    pub(crate) fn remove_owned_state(
        &self,
        branch: &str,
        worktree_path: &str,
        runtimes: &[OpencodeRuntime],
    ) -> Result<(), DatabaseError> {
        let generations = runtimes
            .iter()
            .map(|runtime| {
                Ok((
                    runtime,
                    to_i64("opencode_runtime.generation", runtime.generation)?,
                ))
            })
            .collect::<Result<Vec<_>, DatabaseError>>()?;
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            super::database::begin_immediate_query()
                .execute(&mut connection)
                .await?;
            let result = async {
                let owner =
                    sqlx::query_file_as!(CleanupOwnerRow, "sql/session/cleanup_owner.sql", branch)
                        .fetch_optional(&mut connection)
                        .await?
                        .map(|row| row.worktree);
                if owner.as_deref().is_some_and(|owner| owner != worktree_path) {
                    return Err(sqlx::Error::Protocol(format!(
                        "retained state for {branch}: it now belongs to worktree {owner:?}"
                    )));
                }
                sqlx::query_file!("sql/session/delete_pr_details_cache.sql", branch)
                    .execute(&mut connection)
                    .await?;
                sqlx::query_file!("sql/session/delete_pr_cache.sql", branch)
                    .execute(&mut connection)
                    .await?;
                sqlx::query_file!("sql/session/delete_agent_state.sql", branch)
                    .execute(&mut connection)
                    .await?;
                for (runtime, generation) in &generations {
                    sqlx::query_file!(
                        "sql/session/delete_opencode_runtime.sql",
                        runtime.repo_root,
                        runtime.harness_id,
                        runtime.branch,
                        runtime.worktree_path,
                        generation
                    )
                    .execute(&mut connection)
                    .await?;
                }
                sqlx::query_file!(
                    "sql/session/delete_task_metadata.sql",
                    branch,
                    worktree_path
                )
                .execute(&mut connection)
                .await?;
                sqlx::query_file!(
                    "sql/session/delete_worktree_harness.sql",
                    branch,
                    worktree_path
                )
                .execute(&mut connection)
                .await?;
                sqlx::query_file!("sql/session/delete_hidden_session.sql", branch)
                    .execute(&mut connection)
                    .await?;
                sqlx::query_file!(
                    "sql/session/delete_archived_worktree_for_path.sql",
                    branch,
                    worktree_path
                )
                .execute(&mut connection)
                .await?;
                sqlx::query_file!(
                    "sql/session/delete_pending_deletion.sql",
                    branch,
                    worktree_path
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
            result
        })
    }

    pub(crate) fn write_task_metadata(
        &self,
        input: &TaskMetadataInput<'_>,
    ) -> Result<(), DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            let result = sqlx::query_file!(
                "sql/session/write_task_metadata.sql",
                input.branch,
                input.prompt_summary,
                input.initial_prompt,
                input.worktree,
                input.classification,
                input.visibility,
                input.updated_unix_ms
            )
            .execute(&mut connection)
            .await?;
            require_one_row(result, "write task metadata")?;
            Ok(())
        })
    }

    pub(crate) fn set_visibility(
        &self,
        input: &TaskMetadataInput<'_>,
    ) -> Result<(), DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            let result = sqlx::query_file!(
                "sql/session/set_worktree_visibility.sql",
                input.branch,
                input.prompt_summary,
                input.worktree,
                input.classification,
                input.visibility,
                input.updated_unix_ms
            )
            .execute(&mut connection)
            .await?;
            require_one_row(result, "set worktree visibility")?;
            Ok(())
        })
    }

    pub(crate) fn load_harness(
        &self,
        branch: &str,
    ) -> Result<Option<WorktreeHarnessRecord>, DatabaseError> {
        let options = self.options()?;
        let row = block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            sqlx::query_file_as!(
                WorktreeHarnessRow,
                "sql/session/load_worktree_harness.sql",
                branch
            )
            .fetch_optional(&mut connection)
            .await
        })?;
        Ok(row.map(|row| WorktreeHarnessRecord {
            worktree_path: row.worktree_path,
            worktree_incarnation: row.worktree_incarnation,
            harness_id: row.harness_id,
            migration_policy: row.migration_policy,
        }))
    }

    pub(crate) fn set_harness(
        &self,
        input: &WorktreeHarnessInput<'_>,
    ) -> Result<(), DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            let result = sqlx::query_file!(
                "sql/session/set_worktree_harness.sql",
                input.branch,
                input.worktree_path,
                input.worktree_incarnation,
                input.harness_id,
                input.migration_policy,
                input.updated_unix_ms
            )
            .execute(&mut connection)
            .await?;
            require_one_row(result, "set worktree harness")?;
            Ok(())
        })
    }

    pub(crate) fn archive(&self, input: &ArchiveInput<'_>) -> Result<(), DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            super::database::begin_immediate_query()
                .execute(&mut connection)
                .await?;
            let result = async {
                sqlx::query_file!(
                    "sql/session/upsert_hidden_session.sql",
                    input.branch,
                    input.archived_unix_ms
                )
                .execute(&mut connection)
                .await?;
                sqlx::query_file!(
                    "sql/session/upsert_archived_worktree.sql",
                    input.branch,
                    input.repo_root,
                    input.worktree_path,
                    input.archived_unix_ms,
                    input.classification
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
            result
        })
    }

    pub(crate) fn unarchive(&self, branch: &str) -> Result<(), DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            super::database::begin_immediate_query()
                .execute(&mut connection)
                .await?;
            let result = async {
                sqlx::query_file!("sql/session/delete_hidden_session.sql", branch)
                    .execute(&mut connection)
                    .await?;
                sqlx::query_file!("sql/session/delete_archived_worktree.sql", branch)
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
            result
        })
    }

    pub(crate) fn list_archived(&self) -> Result<Vec<ArchivedWorktreeRecord>, DatabaseError> {
        let options = self.options()?;
        let rows = block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            sqlx::query_file_as!(
                ArchivedWorktreeRow,
                "sql/session/list_archived_worktrees.sql"
            )
            .fetch_all(&mut connection)
            .await
        })?;
        Ok(rows
            .into_iter()
            .map(|row| ArchivedWorktreeRecord {
                branch: row.branch,
                worktree_path: row.worktree_path,
                classification: row.classification,
            })
            .collect())
    }

    pub(crate) fn hidden_exists(&self, branch: &str) -> Result<bool, DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            let row =
                sqlx::query_file_as!(ExistsRow, "sql/session/hidden_session_exists.sql", branch)
                    .fetch_one(&mut connection)
                    .await?;
            Ok(row.exists != 0)
        })
    }

    pub(crate) fn save_agent_state(
        &self,
        branch: &str,
        state: &str,
        updated: i64,
    ) -> Result<(), DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            let result =
                sqlx::query_file!("sql/session/save_agent_state.sql", branch, state, updated)
                    .execute(&mut connection)
                    .await?;
            require_one_row(result, "save agent state")?;
            Ok(())
        })
    }

    pub(crate) fn load_agent_state(&self, branch: &str) -> Result<Option<String>, DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            sqlx::query_file_as!(StateRow, "sql/session/load_agent_state.sql", branch)
                .fetch_optional(&mut connection)
                .await
                .map(|row| row.map(|row| row.state))
        })
    }

    pub(crate) fn load_task_metadata(
        &self,
        branch: &str,
    ) -> Result<Option<TaskMetadataRecord>, DatabaseError> {
        let options = self.options()?;
        let row = block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            sqlx::query_file_as!(
                TaskMetadataRow,
                "sql/session/load_task_metadata.sql",
                branch
            )
            .fetch_optional(&mut connection)
            .await
        })?;
        Ok(row.map(|row| TaskMetadataRecord {
            prompt_summary: row.prompt_summary,
            classification: row.classification,
            visibility: row.visibility,
        }))
    }

    #[cfg(test)]
    pub(crate) fn load_initial_prompt(
        &self,
        branch: &str,
    ) -> Result<Option<String>, DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            sqlx::query_file_as!(
                InitialPromptRow,
                "sql/session/load_task_initial_prompt.sql",
                branch
            )
            .fetch_optional(&mut connection)
            .await
            .map(|row| row.map(|row| row.initial_prompt))
        })
    }

    pub(crate) fn hidden_sessions(&self) -> Result<Vec<(String, i64)>, DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            let rows =
                sqlx::query_file_as!(HiddenSessionRow, "sql/session/list_hidden_sessions.sql")
                    .fetch_all(&mut connection)
                    .await?;
            Ok(rows
                .into_iter()
                .map(|row| (row.branch, row.hidden_unix_ms))
                .collect())
        })
    }

    pub(crate) fn remove_agent_state(&self, branch: &str) -> Result<(), DatabaseError> {
        let options = self.options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            sqlx::query_file!("sql/session/delete_agent_state.sql", branch)
                .execute(&mut connection)
                .await?;
            Ok(())
        })
    }
}

#[derive(FromRow)]
struct RuntimeRow {
    repo_root: String,
    harness_id: String,
    branch: String,
    worktree_path: String,
    server_port: i64,
    server_url: String,
    server_pid: Option<i64>,
    opencode_session_id: Option<String>,
    generation: i64,
    updated_unix_ms: i64,
    server_start_time_ticks: Option<i64>,
}

impl TryFrom<RuntimeRow> for OpencodeRuntime {
    type Error = DatabaseError;

    fn try_from(row: RuntimeRow) -> Result<Self, Self::Error> {
        Ok(Self {
            repo_root: row.repo_root,
            harness_id: row.harness_id,
            branch: row.branch,
            worktree_path: row.worktree_path,
            server_port: checked_integer("opencode_runtime.server_port", row.server_port)?,
            server_url: row.server_url,
            server_pid: row
                .server_pid
                .map(|value| checked_integer("opencode_runtime.server_pid", value))
                .transpose()?,
            server_process_identity: row
                .server_start_time_ticks
                .map(|value| checked_integer("opencode_runtime.server_start_time_ticks", value))
                .transpose()?,
            opencode_session_id: row.opencode_session_id,
            generation: checked_integer("opencode_runtime.generation", row.generation)?,
            updated_unix_ms: checked_integer(
                "opencode_runtime.updated_unix_ms",
                row.updated_unix_ms,
            )?,
        })
    }
}

fn checked_integer<T>(field: &'static str, value: i64) -> Result<T, DatabaseError>
where
    T: TryFrom<i64>,
{
    T::try_from(value).map_err(|_| DatabaseError::InvalidValue {
        field,
        value: value.to_string(),
    })
}

pub(crate) fn load_runtime(
    path: &Path,
    repo_root: &str,
    harness_id: &str,
    branch: &str,
    worktree_path: &str,
) -> Result<Option<OpencodeRuntime>, DatabaseError> {
    super::database::initialize(path)?;
    let options = writable_options(path, false)?;
    let row = block_on(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_file_as!(
            RuntimeRow,
            "sql/session/load_opencode_runtime.sql",
            repo_root,
            harness_id,
            branch,
            worktree_path,
        )
        .fetch_optional(&mut connection)
        .await
    })?;
    row.map(TryInto::try_into).transpose()
}

pub(crate) fn list_runtimes_for_harness(
    path: &Path,
    repo_root: &str,
    harness_id: &str,
) -> Result<Vec<OpencodeRuntime>, DatabaseError> {
    super::database::initialize(path)?;
    let options = writable_options(path, false)?;
    let rows = block_on(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_file_as!(
            RuntimeRow,
            "sql/session/list_opencode_runtimes_for_harness.sql",
            repo_root,
            harness_id,
        )
        .fetch_all(&mut connection)
        .await
    })?;
    rows.into_iter().map(TryInto::try_into).collect()
}

pub(crate) fn list_runtimes_for_worktree(
    path: &Path,
    repo_root: &str,
    branch: &str,
    worktree_path: &str,
) -> Result<Vec<OpencodeRuntime>, DatabaseError> {
    super::database::initialize(path)?;
    let options = writable_options(path, false)?;
    let rows = block_on(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_file_as!(
            RuntimeRow,
            "sql/session/list_opencode_runtimes_for_worktree.sql",
            repo_root,
            branch,
            worktree_path,
        )
        .fetch_all(&mut connection)
        .await
    })?;
    rows.into_iter().map(TryInto::try_into).collect()
}

fn to_i64(field: &'static str, value: u64) -> Result<i64, DatabaseError> {
    i64::try_from(value).map_err(|_| DatabaseError::InvalidValue {
        field,
        value: value.to_string(),
    })
}

fn require_one_row(
    result: sqlx::sqlite::SqliteQueryResult,
    operation: &'static str,
) -> Result<(), sqlx::Error> {
    if result.rows_affected() == 1 {
        Ok(())
    } else {
        Err(sqlx::Error::Protocol(format!(
            "{operation} affected {} rows; expected 1",
            result.rows_affected()
        )))
    }
}

pub(crate) fn save_runtime(path: &Path, runtime: &OpencodeRuntime) -> Result<(), DatabaseError> {
    super::database::initialize(path)?;
    let generation = to_i64("opencode_runtime.generation", runtime.generation)?;
    let updated = to_i64("opencode_runtime.updated_unix_ms", runtime.updated_unix_ms)?;
    let server_port = i64::from(runtime.server_port);
    let server_pid = runtime.server_pid.map(i64::from);
    let process_identity = runtime
        .server_process_identity
        .map(|value| to_i64("opencode_runtime.server_start_time_ticks", value))
        .transpose()?;
    let options = writable_options(path, false)?;
    block_on(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_file!(
            "sql/session/upsert_opencode_runtime.sql",
            runtime.repo_root,
            runtime.harness_id,
            runtime.branch,
            runtime.worktree_path,
            server_port,
            runtime.server_url,
            server_pid,
            runtime.opencode_session_id,
            generation,
            updated,
            process_identity,
        )
        .execute(&mut connection)
        .await?;
        Ok(())
    })
}

pub(crate) fn count_server_references(
    path: &Path,
    repo_root: &str,
    server_url: &str,
) -> Result<i64, DatabaseError> {
    super::database::initialize(path)?;
    let options = writable_options(path, false)?;
    block_on(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_file_as!(
            CountRow,
            "sql/session/count_opencode_server_references.sql",
            repo_root,
            server_url,
        )
        .fetch_one(&mut connection)
        .await
        .map(|row| row.count)
    })
}

pub(crate) fn delete_runtime(path: &Path, runtime: &OpencodeRuntime) -> Result<(), DatabaseError> {
    let generation = to_i64("opencode_runtime.generation", runtime.generation)?;
    super::database::initialize(path)?;
    let options = writable_options(path, false)?;
    block_on(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_file!(
            "sql/session/delete_opencode_runtime.sql",
            runtime.repo_root,
            runtime.harness_id,
            runtime.branch,
            runtime.worktree_path,
            generation,
        )
        .execute(&mut connection)
        .await?;
        Ok(())
    })
}

pub(crate) fn save_shared_server_runtime(
    path: &Path,
    runtime: &OpencodeRuntime,
) -> Result<(), DatabaseError> {
    let server_port = i64::from(runtime.server_port);
    let server_pid = runtime.server_pid.map(i64::from);
    let generation = to_i64("opencode_runtime.generation", runtime.generation)?;
    let updated = to_i64("opencode_runtime.updated_unix_ms", runtime.updated_unix_ms)?;
    let process_identity = runtime
        .server_process_identity
        .map(|value| to_i64("opencode_runtime.server_start_time_ticks", value))
        .transpose()?;
    super::database::initialize(path)?;
    let options = writable_options(path, false)?;
    block_on(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        super::database::begin_immediate_query()
            .execute(&mut connection)
            .await?;
        let result = async {
            sqlx::query_file!(
                "sql/session/update_shared_opencode_server.sql",
                server_port,
                runtime.server_url,
                server_pid,
                process_identity,
                updated,
                runtime.repo_root,
                runtime.harness_id,
                server_port,
                runtime.server_url,
                server_pid,
                process_identity,
            )
            .execute(&mut connection)
            .await?;
            sqlx::query_file!(
                "sql/session/upsert_opencode_runtime.sql",
                runtime.repo_root,
                runtime.harness_id,
                runtime.branch,
                runtime.worktree_path,
                server_port,
                runtime.server_url,
                server_pid,
                runtime.opencode_session_id,
                generation,
                updated,
                process_identity,
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
        result
    })
}

#[cfg(test)]
pub(crate) fn test_install_shared_server_runtime_upsert_failure(
    path: &Path,
) -> Result<(), DatabaseError> {
    let mut connection = super::database::open_writable(path)?;
    block_on(async {
        sqlx::query_file!("sql/session/test_fail_shared_server_runtime_upsert.sql")
            .execute(&mut connection)
            .await?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

    fn database_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "prism-session-interface-{}-{}.db",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn session_store_round_trips_state_through_a_file_database() {
        let path = database_path();
        let store = SessionStore::open(&path).unwrap();
        let metadata = TaskMetadataInput {
            branch: "feature/session-store",
            prompt_summary: "persist sessions",
            initial_prompt: "Implement persistence",
            worktree: "/tmp/worktree",
            classification: "work",
            visibility: 2,
            updated_unix_ms: 42,
        };

        store.write_task_metadata(&metadata).unwrap();
        store
            .save_agent_state(metadata.branch, "running", 43)
            .unwrap();
        store
            .archive(&ArchiveInput {
                branch: metadata.branch,
                repo_root: "/tmp/repo",
                worktree_path: metadata.worktree,
                archived_unix_ms: 44,
                classification: metadata.classification,
            })
            .unwrap();

        let loaded = store.load_task_metadata(metadata.branch).unwrap().unwrap();
        assert_eq!(loaded.prompt_summary, metadata.prompt_summary);
        assert_eq!(loaded.visibility, metadata.visibility);
        assert_eq!(
            store.load_agent_state(metadata.branch).unwrap().as_deref(),
            Some("running")
        );
        assert!(store.hidden_exists(metadata.branch).unwrap());
        assert_eq!(store.list_archived().unwrap()[0].branch, metadata.branch);

        store.unarchive(metadata.branch).unwrap();
        assert!(!store.hidden_exists(metadata.branch).unwrap());
        remove_database(&path);
    }

    fn remove_database(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }
}
