use std::collections::BTreeSet;
use std::path::Path;

use crate::agent::AgentState;
use crate::config::NotificationConfig;
use crate::persistence::notification::{NotificationStore, ObserveInput, OutboxInput, PendingRow};
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

impl From<PendingRow> for PendingNotification {
    fn from(row: PendingRow) -> Self {
        Self {
            id: row.id,
            title: row.title,
            body: row.body,
        }
    }
}

pub(crate) struct NotificationCoordinator {
    store: NotificationStore,
}

impl NotificationCoordinator {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        NotificationStore::open(path)
            .map(|store| Self { store })
            .map_err(|error| error.to_string())
    }

    pub(crate) fn observe(
        &self,
        observation: NotificationObservation<'_>,
    ) -> Result<ObserveResult, String> {
        let path = observation.session.path.display().to_string();
        let branch = observation.session.branch.as_str();
        let incarnation = observation.session.incarnation.as_str();
        let previous = self.last_state(observation.session)?;
        let kind = previous
            .and_then(|previous| transition(previous, observation.state))
            .filter(|kind| enabled(observation.config, *kind));
        let subject = if observation.repo_label.is_empty() {
            branch.to_string()
        } else {
            format!("{}: {branch}", observation.repo_label)
        };
        let title = kind.map(|kind| format!("Prism: {}", kind.title()));
        let body = kind.map(|kind| format!("{subject} {}", kind.suffix()));
        let outbox = kind.map(|kind| OutboxInput {
            kind: kind.label(),
            title: title.as_deref().expect("notification title follows kind"),
            body: body.as_deref().expect("notification body follows kind"),
            expires_unix_ms: observation
                .observed_unix_ms
                .saturating_add(DELIVERY_LIFETIME_MS),
        });
        let disabled = [
            (!observation.config.needs_input).then_some(NotificationKind::NeedsInput.label()),
            (!observation.config.completed).then_some(NotificationKind::Completed.label()),
            (!observation.config.failed).then_some(NotificationKind::Failed.label()),
            (!observation.config.failed).then_some(NotificationKind::NeedsRestart.label()),
        ];
        let disabled = disabled.into_iter().flatten().collect::<Vec<_>>();
        self.store
            .observe(ObserveInput {
                path: &path,
                branch,
                incarnation,
                state: observation.state.label(),
                observed_unix_ms: observation.observed_unix_ms,
                disabled_kinds: &disabled,
                notifications_enabled: observation.config.enabled,
                expected_previous_state: previous.map(AgentState::label),
                outbox,
            })
            .map(|event_id| ObserveResult { event_id })
            .map_err(|error| error.to_string())
    }

    pub(crate) fn last_state(
        &self,
        session: &WorktreeSessionKey,
    ) -> Result<Option<AgentState>, String> {
        self.store
            .last_state(
                &session.path.display().to_string(),
                &session.branch,
                &session.incarnation,
            )
            .map_err(|error| error.to_string())
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> Result<Vec<PendingNotification>, String> {
        self.store
            .pending()
            .map(|rows| rows.into_iter().map(Into::into).collect())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn claim_next(
        &self,
        now_unix_ms: i64,
    ) -> Result<Option<PendingNotification>, String> {
        self.store
            .claim_next(now_unix_ms)
            .map(|row| row.map(Into::into))
            .map_err(|error| error.to_string())
    }

    pub(crate) fn expire_pending(&self, now_unix_ms: i64) -> Result<usize, String> {
        self.store
            .expire_pending(now_unix_ms)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn mark_accepted(&self, id: i64, accepted_unix_ms: i64) -> Result<(), String> {
        self.store
            .mark_accepted(id, accepted_unix_ms)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn retry(
        &self,
        id: i64,
        available_unix_ms: i64,
        category: &'static str,
    ) -> Result<(), String> {
        self.store
            .retry(id, available_unix_ms, category)
            .map_err(|error| error.to_string())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn mark_uncertain(
        &self,
        id: i64,
        at_unix_ms: i64,
        category: &'static str,
    ) -> Result<(), String> {
        self.store
            .mark_uncertain(id, at_unix_ms, category)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn abandon_uncertain(&self, now_unix_ms: i64) -> Result<usize, String> {
        self.store
            .abandon_uncertain(now_unix_ms)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn retain<'a>(
        &self,
        live: impl IntoIterator<Item = &'a WorktreeSessionKey>,
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
        self.store
            .retain(
                &live,
                now_unix_ms,
                now_unix_ms.saturating_sub(DELIVERY_HISTORY_RETENTION_MS),
            )
            .map_err(|error| error.to_string())
    }
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
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::session::{WorktreeRepositoryKey, WorktreeSessionKey};

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);

    struct TestStore {
        path: std::path::PathBuf,
        coordinator: NotificationCoordinator,
    }

    impl TestStore {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "prism-notification-{}-{}.db",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let coordinator = NotificationCoordinator::open(&path).unwrap();
            Self { path, coordinator }
        }
    }

    impl Drop for TestStore {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_file(format!("{}-wal", self.path.display()));
            let _ = fs::remove_file(format!("{}-shm", self.path.display()));
        }
    }

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
    fn baseline_transition_and_duplicate_are_atomic() {
        let store = TestStore::new();
        let coordinator = &store.coordinator;
        assert_eq!(
            coordinator
                .observe(observation(AgentState::Running, 1_000))
                .unwrap()
                .event_id,
            None
        );
        assert!(
            coordinator
                .observe(observation(AgentState::NeedsInput, 2_000))
                .unwrap()
                .event_id
                .is_some()
        );
        assert_eq!(
            coordinator
                .observe(observation(AgentState::NeedsInput, 3_000))
                .unwrap()
                .event_id,
            None
        );
        assert_eq!(coordinator.pending().unwrap().len(), 1);
    }

    #[test]
    fn obsolete_delivery_is_superseded_before_rearming() {
        let store = TestStore::new();
        let coordinator = &store.coordinator;
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
    fn delivery_state_survives_reopening_the_file() {
        let store = TestStore::new();
        let coordinator = &store.coordinator;
        coordinator
            .observe(observation(AgentState::Running, 1_000))
            .unwrap();
        coordinator
            .observe(observation(AgentState::NeedsInput, 2_000))
            .unwrap();
        let claimed = coordinator.claim_next(2_000).unwrap().unwrap();
        coordinator.mark_accepted(claimed.id, 2_100).unwrap();
        let reopened = NotificationCoordinator::open(&store.path).unwrap();
        assert!(reopened.claim_next(2_200).unwrap().is_none());
    }

    #[test]
    fn stale_notification_expires_instead_of_replaying() {
        let store = TestStore::new();
        let coordinator = &store.coordinator;
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
    fn interrupted_dispatch_is_not_replayed() {
        let store = TestStore::new();
        let coordinator = &store.coordinator;
        coordinator
            .observe(observation(AgentState::Running, 1_000))
            .unwrap();
        coordinator
            .observe(observation(AgentState::NeedsInput, 2_000))
            .unwrap();
        coordinator.claim_next(2_000).unwrap().unwrap();

        assert_eq!(coordinator.abandon_uncertain(2_100).unwrap(), 1);
        assert!(coordinator.claim_next(2_200).unwrap().is_none());
    }

    #[test]
    fn disabling_notifications_supersedes_pending_intent() {
        let store = TestStore::new();
        let coordinator = &store.coordinator;
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

    #[test]
    fn retiring_a_session_supersedes_delivery_and_rebaselines() {
        let store = TestStore::new();
        let coordinator = &store.coordinator;
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
}
