use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{FromRow, SqlitePool};
use tokio::sync::OnceCell;

#[derive(Clone, Debug, FromRow)]
pub(crate) struct AgentRow {
    pub state: String,
    pub updated_unix_ms: i64,
}

#[derive(Clone, Debug, FromRow)]
pub(crate) struct PullRequestRow {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub merge_state_status: Option<String>,
    pub check_status: Option<String>,
    pub refreshed_unix_ms: i64,
    pub merged: i64,
    pub draft: i64,
    pub observation_error: Option<String>,
}

#[derive(FromRow)]
struct HiddenRow {
    branch: String,
}

/// Async read-only interface for repository-local caches.
///
/// Workflow state deliberately does not appear here: it is projected from the global run ledger.
pub(crate) struct WorkspaceReader {
    options: SqliteConnectOptions,
    readers: OnceCell<SqlitePool>,
}

impl WorkspaceReader {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Err(format!("database does not exist: {}", path.display()));
        }
        Ok(Self {
            options: readonly_options(path)?,
            readers: OnceCell::new(),
        })
    }

    async fn readers(&self) -> &SqlitePool {
        self.readers
            .get_or_init(|| async {
                SqlitePoolOptions::new()
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
                    .connect_lazy_with(self.options.clone())
            })
            .await
    }

    pub(crate) async fn hidden(&self) -> Result<Vec<String>, String> {
        let rows = sqlx::query_file_as!(HiddenRow, "sql/workspace/load_hidden.sql")
            .fetch_all(self.readers().await)
            .await
            .map_err(query_error)?;
        Ok(rows.into_iter().map(|row| row.branch).collect())
    }

    pub(crate) async fn agent(&self, branch: &str) -> Result<Option<AgentRow>, String> {
        sqlx::query_file_as!(AgentRow, "sql/workspace/load_agent.sql", branch)
            .fetch_optional(self.readers().await)
            .await
            .map_err(query_error)
    }

    pub(crate) async fn pull_request(
        &self,
        branch: &str,
    ) -> Result<Option<PullRequestRow>, String> {
        sqlx::query_file_as!(
            PullRequestRow,
            "sql/workspace/load_pull_request.sql",
            branch
        )
        .fetch_optional(self.readers().await)
        .await
        .map_err(query_error)
    }
}

fn query_error(error: sqlx::Error) -> String {
    format!("read repository cache projection: {error}")
}

fn readonly_options(path: &Path) -> Result<SqliteConnectOptions, String> {
    Ok(SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .create_if_missing(false)
        .foreign_keys(true)
        .busy_timeout(Duration::from_millis(50)))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn workspace_reader_projects_named_rows_from_a_file_database() {
        let path = std::env::temp_dir().join(format!(
            "prism-workspace-interface-{}-{}.db",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ));
        let store = crate::persistence::session::SessionStore::open(&path).unwrap();
        store
            .archive(&crate::persistence::session::ArchiveInput {
                branch: "feature/workspace-reader",
                repo_root: "/tmp/repo",
                worktree_path: "/tmp/repo-worktree",
                archived_unix_ms: 1,
                classification: "work",
            })
            .unwrap();
        store
            .save_agent_state("feature/workspace-reader", "running", 2)
            .unwrap();

        crate::async_runtime::block_on(async {
            let reader = WorkspaceReader::open(&path).unwrap();
            assert_eq!(reader.hidden().await.unwrap(), ["feature/workspace-reader"]);
            let agent = reader
                .agent("feature/workspace-reader")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(agent.state, "running");
            assert_eq!(agent.updated_unix_ms, 2);
        })
        .unwrap();

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }
}
