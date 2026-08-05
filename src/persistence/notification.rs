use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use sqlx::{Connection, FromRow, SqliteConnection};

use crate::agent::AgentState;

use crate::persistence::database::{block_on, writable_options};
use crate::persistence::error::DatabaseError;

#[derive(Debug)]
pub(crate) enum NotificationError {
    Database(DatabaseError),
    InvalidState(String),
    ConcurrentClaim,
    NotDispatching(i64),
}

impl fmt::Display for NotificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => error.fmt(formatter),
            Self::InvalidState(state) => {
                write!(formatter, "invalid persisted notification state '{state}'")
            }
            Self::ConcurrentClaim => {
                formatter.write_str("pending notification changed while it was claimed")
            }
            Self::NotDispatching(id) => write!(formatter, "notification {id} was not dispatching"),
        }
    }
}

impl From<DatabaseError> for NotificationError {
    fn from(error: DatabaseError) -> Self {
        Self::Database(error)
    }
}

#[derive(Clone, Debug, FromRow)]
pub(crate) struct PendingRow {
    pub id: i64,
    pub title: String,
    pub body: String,
}

#[derive(FromRow)]
struct SessionRow {
    state: String,
    transition_sequence: i64,
}

#[derive(FromRow)]
struct SessionIdentityRow {
    worktree_path: String,
    branch: String,
    incarnation: String,
}

pub(crate) struct ObserveInput<'a> {
    pub path: &'a str,
    pub branch: &'a str,
    pub incarnation: &'a str,
    pub state: &'a str,
    pub observed_unix_ms: i64,
    pub disabled_kinds: &'a [&'a str],
    pub notifications_enabled: bool,
    pub expected_previous_state: Option<&'a str>,
    pub outbox: Option<OutboxInput<'a>>,
}

pub(crate) struct OutboxInput<'a> {
    pub kind: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub expires_unix_ms: i64,
}

pub(crate) struct NotificationStore {
    path: PathBuf,
}

impl NotificationStore {
    pub(crate) fn open(path: &Path) -> Result<Self, NotificationError> {
        crate::persistence::database::initialize(path)?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub(crate) fn observe(
        &self,
        input: ObserveInput<'_>,
    ) -> Result<Option<i64>, NotificationError> {
        let options = writable_options(&self.path, false)?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            super::database::begin_immediate_query()
                .execute(&mut connection)
                .await?;
            let result = async {
                let previous = sqlx::query_file_as!(
                    SessionRow,
                    "sql/notification/load_session.sql",
                    input.path,
                    input.branch,
                    input.incarnation,
                )
                .fetch_optional(&mut connection)
                .await?;
                let Some(previous) = previous else {
                    sqlx::query_file!(
                        "sql/notification/insert_session.sql",
                        input.path,
                        input.branch,
                        input.incarnation,
                        input.state,
                        input.observed_unix_ms,
                    )
                    .execute(&mut connection)
                    .await?;
                    super::database::commit_query()
                        .execute(&mut connection)
                        .await?;
                    return Ok(None);
                };
                if input.notifications_enabled {
                    for kind in input.disabled_kinds {
                        sqlx::query_file!(
                            "sql/notification/supersede_pending_kind.sql",
                            input.observed_unix_ms,
                            input.path,
                            input.branch,
                            input.incarnation,
                            kind,
                        )
                        .execute(&mut connection)
                        .await?;
                    }
                } else {
                    sqlx::query_file!(
                        "sql/notification/supersede_pending_session.sql",
                        input.observed_unix_ms,
                        input.path,
                        input.branch,
                        input.incarnation,
                    )
                    .execute(&mut connection)
                    .await?;
                }
                if previous.state == input.state {
                    sqlx::query_file!(
                        "sql/notification/refresh_session.sql",
                        input.observed_unix_ms,
                        input.path,
                        input.branch,
                        input.incarnation,
                    )
                    .execute(&mut connection)
                    .await?;
                    super::database::commit_query()
                        .execute(&mut connection)
                        .await?;
                    return Ok(None);
                }
                let sequence = previous.transition_sequence.saturating_add(1);
                sqlx::query_file!(
                    "sql/notification/advance_session.sql",
                    input.state,
                    sequence,
                    input.observed_unix_ms,
                    input.path,
                    input.branch,
                    input.incarnation,
                )
                .execute(&mut connection)
                .await?;
                sqlx::query_file!(
                    "sql/notification/supersede_pending_session.sql",
                    input.observed_unix_ms,
                    input.path,
                    input.branch,
                    input.incarnation,
                )
                .execute(&mut connection)
                .await?;
                let id = if input.expected_previous_state == Some(previous.state.as_str())
                    && let Some(outbox) = input.outbox
                {
                    sqlx::query_file_scalar!(
                        "sql/notification/insert_outbox.sql",
                        input.path,
                        input.branch,
                        input.incarnation,
                        sequence,
                        outbox.kind,
                        outbox.title,
                        outbox.body,
                        input.observed_unix_ms,
                        outbox.expires_unix_ms,
                        input.observed_unix_ms,
                    )
                    .fetch_one(&mut connection)
                    .await?
                    .into()
                } else {
                    None
                };
                super::database::commit_query()
                    .execute(&mut connection)
                    .await?;
                Ok(id)
            }
            .await;
            if result.is_err() {
                let _ = super::database::rollback_query()
                    .execute(&mut connection)
                    .await;
            }
            result
        })
        .map_err(Into::into)
    }

    pub(crate) fn last_state(
        &self,
        path: &str,
        branch: &str,
        incarnation: &str,
    ) -> Result<Option<AgentState>, NotificationError> {
        let options = writable_options(&self.path, false)?;
        let state = block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            sqlx::query_file_scalar!("sql/notification/load_state.sql", path, branch, incarnation)
                .fetch_optional(&mut connection)
                .await
        })?;
        state
            .map(|state| AgentState::parse(&state).ok_or(NotificationError::InvalidState(state)))
            .transpose()
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> Result<Vec<PendingRow>, NotificationError> {
        let options = writable_options(&self.path, false)?;
        Ok(block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            sqlx::query_file_as!(PendingRow, "sql/notification/list_pending.sql")
                .fetch_all(&mut connection)
                .await
        })?)
    }

    pub(crate) fn claim_next(&self, now: i64) -> Result<Option<PendingRow>, NotificationError> {
        let options = writable_options(&self.path, false)?;
        let result = block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            super::database::begin_immediate_query()
                .execute(&mut connection)
                .await?;
            let result = async {
                sqlx::query_file!("sql/notification/expire_pending.sql", now, now)
                    .execute(&mut connection)
                    .await?;
                let pending =
                    sqlx::query_file_as!(PendingRow, "sql/notification/next_pending.sql", now)
                        .fetch_optional(&mut connection)
                        .await?;
                let Some(pending) = pending else {
                    super::database::commit_query()
                        .execute(&mut connection)
                        .await?;
                    return Ok((None, true));
                };
                let changed =
                    sqlx::query_file!("sql/notification/mark_dispatching.sql", now, pending.id)
                        .execute(&mut connection)
                        .await?
                        .rows_affected();
                if changed != 1 {
                    return Ok((Some(pending), false));
                }
                super::database::commit_query()
                    .execute(&mut connection)
                    .await?;
                Ok((Some(pending), true))
            }
            .await;
            if result
                .as_ref()
                .map(|(_, committed)| !committed)
                .unwrap_or(true)
            {
                let _ = super::database::rollback_query()
                    .execute(&mut connection)
                    .await;
            }
            result
        })?;
        match result {
            (pending, true) => Ok(pending),
            (_, false) => Err(NotificationError::ConcurrentClaim),
        }
    }

    pub(crate) fn expire_pending(&self, now: i64) -> Result<usize, NotificationError> {
        self.execute_count("expire", |connection| {
            Box::pin(async move {
                sqlx::query_file!("sql/notification/expire_pending.sql", now, now)
                    .execute(connection)
                    .await
            })
        })
    }

    pub(crate) fn mark_accepted(&self, id: i64, at: i64) -> Result<(), NotificationError> {
        self.require_dispatching(id, |connection| {
            Box::pin(async move {
                sqlx::query_file!("sql/notification/mark_accepted.sql", at, id)
                    .execute(connection)
                    .await
            })
        })
    }

    pub(crate) fn retry(
        &self,
        id: i64,
        available: i64,
        category: &str,
    ) -> Result<(), NotificationError> {
        let category = category.to_string();
        self.require_dispatching(id, move |connection| {
            Box::pin(async move {
                sqlx::query_file!("sql/notification/retry.sql", available, category, id)
                    .execute(connection)
                    .await
            })
        })
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn mark_uncertain(
        &self,
        id: i64,
        at: i64,
        category: &str,
    ) -> Result<(), NotificationError> {
        let category = category.to_string();
        self.require_dispatching(id, move |connection| {
            Box::pin(async move {
                sqlx::query_file!("sql/notification/mark_uncertain.sql", at, category, id)
                    .execute(connection)
                    .await
            })
        })
    }

    pub(crate) fn abandon_uncertain(&self, now: i64) -> Result<usize, NotificationError> {
        self.execute_count("abandon", |connection| {
            Box::pin(async move {
                sqlx::query_file!("sql/notification/abandon_dispatching.sql", now)
                    .execute(connection)
                    .await
            })
        })
    }

    pub(crate) fn retain(
        &self,
        live: &BTreeSet<(String, String, String)>,
        now: i64,
        cutoff: i64,
    ) -> Result<(), NotificationError> {
        let options = writable_options(&self.path, false)?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            super::database::begin_immediate_query()
                .execute(&mut connection)
                .await?;
            let result = async {
                let persisted =
                    sqlx::query_file_as!(SessionIdentityRow, "sql/notification/list_sessions.sql")
                        .fetch_all(&mut connection)
                        .await?;
                for session in persisted {
                    if live.contains(&(
                        session.worktree_path.clone(),
                        session.branch.clone(),
                        session.incarnation.clone(),
                    )) {
                        continue;
                    }
                    sqlx::query_file!(
                        "sql/notification/supersede_pending_session.sql",
                        now,
                        session.worktree_path,
                        session.branch,
                        session.incarnation
                    )
                    .execute(&mut connection)
                    .await?;
                    sqlx::query_file!(
                        "sql/notification/delete_session.sql",
                        session.worktree_path,
                        session.branch,
                        session.incarnation
                    )
                    .execute(&mut connection)
                    .await?;
                }
                sqlx::query_file!("sql/notification/prune_history.sql", cutoff)
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
        .map_err(Into::into)
    }

    fn execute_count<F>(&self, _operation: &'static str, run: F) -> Result<usize, NotificationError>
    where
        F: for<'c> FnOnce(
            &'c mut SqliteConnection,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<sqlx::sqlite::SqliteQueryResult, sqlx::Error>,
                    > + 'c,
            >,
        >,
    {
        let options = writable_options(&self.path, false)?;
        Ok(block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            run(&mut connection).await
        })?
        .rows_affected() as usize)
    }

    fn require_dispatching<F>(&self, id: i64, run: F) -> Result<(), NotificationError>
    where
        F: for<'c> FnOnce(
            &'c mut SqliteConnection,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<sqlx::sqlite::SqliteQueryResult, sqlx::Error>,
                    > + 'c,
            >,
        >,
    {
        (self.execute_count("dispatch", run)? == 1)
            .then_some(())
            .ok_or(NotificationError::NotDispatching(id))
    }
}
