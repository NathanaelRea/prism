use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::agent::AgentState;
use crate::config::NotificationConfig;
use crate::session::WorktreeSessionKey;

const DELIVERY_LIFETIME_MS: i64 = 10 * 60 * 1_000;
const DELIVERY_HISTORY_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotificationKind {
    NeedsInput,
    Completed,
    Failed,
    NeedsRestart,
}

impl NotificationKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::NeedsInput => "needs_input",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::NeedsRestart => "needs_restart",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::NeedsInput => "Input required",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::NeedsRestart => "Restart required",
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            Self::NeedsInput => "is waiting for input",
            Self::Completed => "finished",
            Self::Failed => "failed",
            Self::NeedsRestart => "needs to be restarted",
        }
    }
}

pub(crate) struct NotificationObservation<'a> {
    pub(crate) session: &'a WorktreeSessionKey,
    pub(crate) repo_label: &'a str,
    pub(crate) state: AgentState,
    pub(crate) config: NotificationConfig,
    pub(crate) observed_unix_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ObserveResult {
    pub(crate) event_id: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingNotification {
    pub(crate) id: i64,
    pub(crate) title: String,
    pub(crate) body: String,
}

pub(crate) struct NotificationCoordinator<'a> {
    conn: &'a mut Connection,
}

impl<'a> NotificationCoordinator<'a> {
    pub(crate) fn new(conn: &'a mut Connection) -> Self {
        Self { conn }
    }

    pub(crate) fn observe(
        &mut self,
        observation: NotificationObservation<'_>,
    ) -> Result<ObserveResult, String> {
        let path = observation.session.path.display().to_string();
        let branch = observation.session.branch.as_str();
        let incarnation = observation.session.incarnation.as_str();
        let transaction = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("begin notification observation: {error}"))?;
        let previous = transaction
            .query_row(
                "select state, transition_sequence
                   from notification_session
                  where worktree_path = ?1 and branch = ?2 and incarnation = ?3",
                params![path, branch, incarnation],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|error| format!("read notification baseline: {error}"))?;

        let Some((previous, sequence)) = previous else {
            transaction
                .execute(
                    "insert into notification_session (
                       worktree_path, branch, incarnation, state, transition_sequence,
                       observed_unix_ms
                     ) values (?1, ?2, ?3, ?4, 0, ?5)",
                    params![
                        path,
                        branch,
                        incarnation,
                        observation.state.label(),
                        observation.observed_unix_ms
                    ],
                )
                .map_err(|error| format!("record notification baseline: {error}"))?;
            transaction
                .commit()
                .map_err(|error| format!("commit notification baseline: {error}"))?;
            return Ok(ObserveResult { event_id: None });
        };
        let previous = AgentState::parse(&previous)
            .ok_or_else(|| format!("invalid persisted notification state '{previous}'"))?;
        if !observation.config.enabled {
            transaction
                .execute(
                    "update notification_outbox
                        set delivery_state = 'superseded', superseded_unix_ms = ?4
                      where worktree_path = ?1 and branch = ?2 and incarnation = ?3
                        and delivery_state = 'pending'",
                    params![path, branch, incarnation, observation.observed_unix_ms],
                )
                .map_err(|error| format!("disable pending notifications: {error}"))?;
        } else {
            for kind in [
                (!observation.config.needs_input).then_some(NotificationKind::NeedsInput),
                (!observation.config.completed).then_some(NotificationKind::Completed),
                (!observation.config.failed).then_some(NotificationKind::Failed),
                (!observation.config.failed).then_some(NotificationKind::NeedsRestart),
            ]
            .into_iter()
            .flatten()
            {
                transaction
                    .execute(
                        "update notification_outbox
                            set delivery_state = 'superseded', superseded_unix_ms = ?5
                          where worktree_path = ?1 and branch = ?2 and incarnation = ?3
                            and kind = ?4 and delivery_state = 'pending'",
                        params![
                            path,
                            branch,
                            incarnation,
                            kind.label(),
                            observation.observed_unix_ms
                        ],
                    )
                    .map_err(|error| format!("disable pending notification kind: {error}"))?;
            }
        }
        if previous == observation.state {
            transaction
                .execute(
                    "update notification_session
                        set observed_unix_ms = ?4
                      where worktree_path = ?1 and branch = ?2 and incarnation = ?3",
                    params![path, branch, incarnation, observation.observed_unix_ms],
                )
                .map_err(|error| format!("refresh notification observation: {error}"))?;
            transaction
                .commit()
                .map_err(|error| format!("commit notification observation: {error}"))?;
            return Ok(ObserveResult { event_id: None });
        }

        let sequence = sequence.saturating_add(1);
        transaction
            .execute(
                "update notification_session
                    set state = ?4, transition_sequence = ?5, observed_unix_ms = ?6
                  where worktree_path = ?1 and branch = ?2 and incarnation = ?3",
                params![
                    path,
                    branch,
                    incarnation,
                    observation.state.label(),
                    sequence,
                    observation.observed_unix_ms
                ],
            )
            .map_err(|error| format!("advance notification observation: {error}"))?;
        transaction
            .execute(
                "update notification_outbox
                    set delivery_state = 'superseded', superseded_unix_ms = ?4
                  where worktree_path = ?1 and branch = ?2 and incarnation = ?3
                    and delivery_state = 'pending'",
                params![path, branch, incarnation, observation.observed_unix_ms],
            )
            .map_err(|error| format!("supersede obsolete notifications: {error}"))?;

        let event_id = transition(previous, observation.state)
            .filter(|kind| enabled(observation.config, *kind))
            .map(|kind| {
                let subject = if observation.repo_label.is_empty() {
                    branch.to_string()
                } else {
                    format!("{}: {branch}", observation.repo_label)
                };
                let title = format!("Prism: {}", kind.title());
                let body = format!("{subject} {}", kind.suffix());
                transaction
                    .execute(
                        "insert into notification_outbox (
                           worktree_path, branch, incarnation, transition_sequence, kind,
                           title, body, observed_unix_ms, expires_unix_ms, delivery_state,
                           available_unix_ms
                         ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', ?8)",
                        params![
                            path,
                            branch,
                            incarnation,
                            sequence,
                            kind.label(),
                            title,
                            body,
                            observation.observed_unix_ms,
                            observation
                                .observed_unix_ms
                                .saturating_add(DELIVERY_LIFETIME_MS)
                        ],
                    )
                    .map_err(|error| format!("enqueue desktop notification: {error}"))?;
                Ok::<i64, String>(transaction.last_insert_rowid())
            })
            .transpose()?;
        transaction
            .commit()
            .map_err(|error| format!("commit notification transition: {error}"))?;
        Ok(ObserveResult { event_id })
    }

    pub(crate) fn last_state(
        &self,
        session: &WorktreeSessionKey,
    ) -> Result<Option<AgentState>, String> {
        let state = self
            .conn
            .query_row(
                "select state
                   from notification_session
                  where worktree_path = ?1 and branch = ?2 and incarnation = ?3",
                params![
                    session.path.display().to_string(),
                    session.branch.as_str(),
                    session.incarnation.as_str()
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| format!("read durable notification state: {error}"))?;
        state
            .map(|state| {
                AgentState::parse(&state)
                    .ok_or_else(|| format!("invalid persisted notification state '{state}'"))
            })
            .transpose()
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> Result<Vec<PendingNotification>, String> {
        let mut statement = self
            .conn
            .prepare(
                "select id, title, body
                   from notification_outbox
                  where delivery_state = 'pending'
                  order by id",
            )
            .map_err(|error| format!("prepare pending notifications: {error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| format!("query pending notifications: {error}"))?;
        rows.map(|row| {
            let (id, title, body) =
                row.map_err(|error| format!("read pending notification: {error}"))?;
            Ok(PendingNotification { id, title, body })
        })
        .collect()
    }

    pub(crate) fn claim_next(
        &mut self,
        now_unix_ms: i64,
    ) -> Result<Option<PendingNotification>, String> {
        let transaction = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("begin notification claim: {error}"))?;
        transaction
            .execute(
                "update notification_outbox
                    set delivery_state = 'expired', superseded_unix_ms = ?1
                  where delivery_state = 'pending' and expires_unix_ms <= ?1",
                [now_unix_ms],
            )
            .map_err(|error| format!("expire pending notifications: {error}"))?;
        let pending = transaction
            .query_row(
                "select id, title, body
                   from notification_outbox
                  where delivery_state = 'pending' and available_unix_ms <= ?1
                  order by id
                  limit 1",
                [now_unix_ms],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| format!("claim pending notification: {error}"))?;
        let Some((id, title, body)) = pending else {
            transaction
                .commit()
                .map_err(|error| format!("commit empty notification claim: {error}"))?;
            return Ok(None);
        };
        let changed = transaction
            .execute(
                "update notification_outbox
                    set delivery_state = 'dispatching', attempted_unix_ms = ?2,
                        attempt_count = attempt_count + 1
                  where id = ?1 and delivery_state = 'pending'",
                params![id, now_unix_ms],
            )
            .map_err(|error| format!("mark notification dispatching: {error}"))?;
        if changed != 1 {
            return Err("pending notification changed while it was claimed".to_string());
        }
        transaction
            .commit()
            .map_err(|error| format!("commit notification claim: {error}"))?;
        Ok(Some(PendingNotification { id, title, body }))
    }

    pub(crate) fn expire_pending(&mut self, now_unix_ms: i64) -> Result<usize, String> {
        self.conn
            .execute(
                "update notification_outbox
                    set delivery_state = 'expired', superseded_unix_ms = ?1
                  where delivery_state = 'pending' and expires_unix_ms <= ?1",
                [now_unix_ms],
            )
            .map_err(|error| format!("expire pending notifications: {error}"))
    }

    pub(crate) fn mark_accepted(&mut self, id: i64, accepted_unix_ms: i64) -> Result<(), String> {
        let changed = self
            .conn
            .execute(
                "update notification_outbox
                    set delivery_state = 'delivered', backend_accepted_unix_ms = ?2,
                        last_failure_category = null
                  where id = ?1 and delivery_state = 'dispatching'",
                params![id, accepted_unix_ms],
            )
            .map_err(|error| format!("acknowledge desktop notification: {error}"))?;
        (changed == 1)
            .then_some(())
            .ok_or_else(|| format!("notification {id} was not dispatching"))
    }

    pub(crate) fn retry(
        &mut self,
        id: i64,
        available_unix_ms: i64,
        category: &'static str,
    ) -> Result<(), String> {
        let changed = self
            .conn
            .execute(
                "update notification_outbox
                    set delivery_state = 'pending', available_unix_ms = ?2,
                        last_failure_category = ?3
                  where id = ?1 and delivery_state = 'dispatching'",
                params![id, available_unix_ms, category],
            )
            .map_err(|error| format!("retry desktop notification: {error}"))?;
        (changed == 1)
            .then_some(())
            .ok_or_else(|| format!("notification {id} was not dispatching"))
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn mark_uncertain(
        &mut self,
        id: i64,
        at_unix_ms: i64,
        category: &'static str,
    ) -> Result<(), String> {
        let changed = self
            .conn
            .execute(
                "update notification_outbox
                    set delivery_state = 'uncertain', superseded_unix_ms = ?2,
                        last_failure_category = ?3
                  where id = ?1 and delivery_state = 'dispatching'",
                params![id, at_unix_ms, category],
            )
            .map_err(|error| format!("classify uncertain desktop notification: {error}"))?;
        (changed == 1)
            .then_some(())
            .ok_or_else(|| format!("notification {id} was not dispatching"))
    }

    pub(crate) fn abandon_uncertain(&mut self, now_unix_ms: i64) -> Result<usize, String> {
        self.conn
            .execute(
                "update notification_outbox
                    set delivery_state = 'uncertain', superseded_unix_ms = ?1,
                        last_failure_category = 'interrupted_dispatch'
                  where delivery_state = 'dispatching'",
                [now_unix_ms],
            )
            .map_err(|error| format!("classify interrupted notifications: {error}"))
    }

    pub(crate) fn retain<'b>(
        &mut self,
        live: impl IntoIterator<Item = &'b WorktreeSessionKey>,
        now_unix_ms: i64,
    ) -> Result<(), String> {
        let live = live
            .into_iter()
            .map(|session| {
                (
                    session.path.display().to_string(),
                    session.branch.clone(),
                    session.incarnation.clone(),
                )
            })
            .collect::<BTreeSet<_>>();
        let transaction = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("begin notification retention: {error}"))?;
        let persisted = {
            let mut statement = transaction
                .prepare(
                    "select worktree_path, branch, incarnation
                       from notification_session",
                )
                .map_err(|error| format!("prepare notification session retention: {error}"))?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|error| format!("query notification session retention: {error}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("read notification session retention: {error}"))?
        };
        for (path, branch, incarnation) in persisted {
            if live.contains(&(path.clone(), branch.clone(), incarnation.clone())) {
                continue;
            }
            transaction
                .execute(
                    "update notification_outbox
                        set delivery_state = 'superseded', superseded_unix_ms = ?4
                      where worktree_path = ?1 and branch = ?2 and incarnation = ?3
                        and delivery_state = 'pending'",
                    params![path, branch, incarnation, now_unix_ms],
                )
                .map_err(|error| format!("supersede retired session notifications: {error}"))?;
            transaction
                .execute(
                    "delete from notification_session
                      where worktree_path = ?1 and branch = ?2 and incarnation = ?3",
                    params![path, branch, incarnation],
                )
                .map_err(|error| format!("retire notification session: {error}"))?;
        }
        transaction
            .execute(
                "delete from notification_outbox
                  where delivery_state not in ('pending', 'dispatching')
                    and observed_unix_ms < ?1",
                [now_unix_ms.saturating_sub(DELIVERY_HISTORY_RETENTION_MS)],
            )
            .map_err(|error| format!("prune notification history: {error}"))?;
        transaction
            .commit()
            .map_err(|error| format!("commit notification retention: {error}"))
    }
}

pub(crate) fn migrate_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        create table if not exists notification_session (
          worktree_path text not null,
          branch text not null,
          incarnation text not null,
          state text not null,
          transition_sequence integer not null,
          observed_unix_ms integer not null,
          primary key (worktree_path, branch, incarnation)
        );
        create table if not exists notification_outbox (
          id integer primary key autoincrement,
          worktree_path text not null,
          branch text not null,
          incarnation text not null,
          transition_sequence integer not null,
          kind text not null,
          title text not null,
          body text not null,
          observed_unix_ms integer not null,
          expires_unix_ms integer not null,
          delivery_state text not null,
          attempt_count integer not null default 0,
          available_unix_ms integer not null,
          attempted_unix_ms integer,
          backend_accepted_unix_ms integer,
          superseded_unix_ms integer,
          last_failure_category text,
          unique (worktree_path, branch, incarnation, transition_sequence)
        );
        create index if not exists notification_outbox_delivery_idx
          on notification_outbox(delivery_state, expires_unix_ms, id);
        ",
    )
    .map_err(|error| format!("migrate notification schema: {error}"))
}

fn transition(previous: AgentState, current: AgentState) -> Option<NotificationKind> {
    if !matches!(previous, AgentState::Attached | AgentState::Running) {
        return None;
    }
    match current {
        AgentState::NeedsInput => Some(NotificationKind::NeedsInput),
        AgentState::ExitedOk => Some(NotificationKind::Completed),
        AgentState::ExitedError => Some(NotificationKind::Failed),
        AgentState::NeedsRestart => Some(NotificationKind::NeedsRestart),
        _ => None,
    }
}

fn enabled(config: NotificationConfig, kind: NotificationKind) -> bool {
    config.enabled
        && match kind {
            NotificationKind::NeedsInput => config.needs_input,
            NotificationKind::Completed => config.completed,
            NotificationKind::Failed | NotificationKind::NeedsRestart => config.failed,
        }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{
        DELIVERY_LIFETIME_MS, NotificationCoordinator, NotificationObservation, migrate_schema,
    };
    use crate::agent::AgentState;
    use crate::config::NotificationConfig;
    use crate::session::{WorktreeRepositoryKey, WorktreeSessionKey};

    fn session() -> WorktreeSessionKey {
        WorktreeSessionKey {
            repository: WorktreeRepositoryKey::new("/tmp/repo".into()),
            path: "/tmp/repo/feature".into(),
            branch: "feature".to_string(),
            incarnation: "one".to_string(),
        }
    }

    fn observation(state: AgentState, at: i64) -> NotificationObservation<'static> {
        NotificationObservation {
            session: Box::leak(Box::new(session())),
            repo_label: "repo",
            state,
            config: NotificationConfig::default(),
            observed_unix_ms: at,
        }
    }

    #[test]
    fn coordinator_baselines_then_records_one_active_to_attention_transition() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let mut coordinator = NotificationCoordinator::new(&mut conn);

        let baseline = coordinator
            .observe(observation(AgentState::Running, 1_000))
            .unwrap();
        let transition = coordinator
            .observe(observation(AgentState::NeedsInput, 2_000))
            .unwrap();
        let duplicate = coordinator
            .observe(observation(AgentState::NeedsInput, 3_000))
            .unwrap();

        assert_eq!(baseline.event_id, None);
        assert!(transition.event_id.is_some());
        assert_eq!(duplicate.event_id, None);
        assert_eq!(coordinator.pending().unwrap().len(), 1);
    }

    #[test]
    fn coordinator_supersedes_obsolete_attention_before_rearming() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let mut coordinator = NotificationCoordinator::new(&mut conn);

        coordinator
            .observe(observation(AgentState::Running, 1_000))
            .unwrap();
        let first = coordinator
            .observe(observation(AgentState::NeedsInput, 2_000))
            .unwrap()
            .event_id
            .unwrap();
        coordinator
            .observe(observation(AgentState::Running, 3_000))
            .unwrap();
        let second = coordinator
            .observe(observation(AgentState::NeedsInput, 4_000))
            .unwrap()
            .event_id
            .unwrap();

        let pending = coordinator.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, second);
        assert_ne!(first, second);
    }

    #[test]
    fn claimed_notifications_are_not_replayed_after_acceptance_or_interrupted_dispatch() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let mut coordinator = NotificationCoordinator::new(&mut conn);
        coordinator
            .observe(observation(AgentState::Running, 1_000))
            .unwrap();
        coordinator
            .observe(observation(AgentState::NeedsInput, 2_000))
            .unwrap();

        let claimed = coordinator.claim_next(2_000).unwrap().unwrap();
        coordinator.mark_accepted(claimed.id, 2_100).unwrap();
        assert!(coordinator.claim_next(2_200).unwrap().is_none());

        coordinator
            .observe(observation(AgentState::Running, 3_000))
            .unwrap();
        coordinator
            .observe(observation(AgentState::NeedsInput, 4_000))
            .unwrap();
        coordinator.claim_next(4_000).unwrap().unwrap();
        assert_eq!(coordinator.abandon_uncertain(4_100).unwrap(), 1);
        assert!(coordinator.claim_next(4_200).unwrap().is_none());
    }

    #[test]
    fn retired_session_supersedes_pending_delivery_and_rebaselines_if_recreated() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let mut coordinator = NotificationCoordinator::new(&mut conn);
        coordinator
            .observe(observation(AgentState::Running, 1_000))
            .unwrap();
        coordinator
            .observe(observation(AgentState::NeedsInput, 2_000))
            .unwrap();

        coordinator.retain(std::iter::empty(), 3_000).unwrap();
        assert!(coordinator.pending().unwrap().is_empty());
        assert_eq!(
            coordinator
                .observe(observation(AgentState::NeedsInput, 4_000))
                .unwrap()
                .event_id,
            None
        );
    }

    #[test]
    fn stale_notification_expires_instead_of_replaying() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let mut coordinator = NotificationCoordinator::new(&mut conn);
        coordinator
            .observe(observation(AgentState::Running, 1_000))
            .unwrap();
        coordinator
            .observe(observation(AgentState::NeedsInput, 2_000))
            .unwrap();

        assert!(
            coordinator
                .claim_next(2_000 + DELIVERY_LIFETIME_MS)
                .unwrap()
                .is_none()
        );
        assert!(coordinator.pending().unwrap().is_empty());
    }

    #[test]
    fn disabling_notifications_cancels_pending_intent_without_a_state_change() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_schema(&conn).unwrap();
        let mut coordinator = NotificationCoordinator::new(&mut conn);
        coordinator
            .observe(observation(AgentState::Running, 1_000))
            .unwrap();
        coordinator
            .observe(observation(AgentState::NeedsInput, 2_000))
            .unwrap();
        let mut disabled = observation(AgentState::NeedsInput, 3_000);
        disabled.config.enabled = false;

        coordinator.observe(disabled).unwrap();

        assert!(coordinator.pending().unwrap().is_empty());
    }
}
