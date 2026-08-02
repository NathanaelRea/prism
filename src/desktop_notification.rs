use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::agent::AgentState;
use crate::config::NotificationConfig;
use crate::session::WorktreeSessionKey;

const QUEUE_CAPACITY: usize = 32;
const FAILURE_COOLDOWN: Duration = Duration::from_secs(60);
const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum NotificationKind {
    NeedsInput,
    Completed,
    Failed,
    NeedsRestart,
}

impl NotificationKind {
    fn label(self) -> &'static str {
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Delivery {
    kind: NotificationKind,
    title: String,
    body: String,
}

enum DispatchMessage {
    Delivery(Delivery),
    #[cfg(test)]
    Barrier(std::sync::mpsc::Sender<()>),
}

pub(crate) struct AgentObservation<'a> {
    pub session: &'a WorktreeSessionKey,
    pub repo_label: &'a str,
    pub branch: &'a str,
    pub state: AgentState,
    pub config: NotificationConfig,
}

pub(crate) struct DesktopNotifier {
    states: BTreeMap<WorktreeSessionKey, AgentState>,
    sender: Option<SyncSender<DispatchMessage>>,
    dispatcher: Option<JoinHandle<()>>,
    last_queue_failures: BTreeMap<(NotificationKind, &'static str), Instant>,
}

impl DesktopNotifier {
    pub(crate) fn new() -> Self {
        Self::with_delivery(|delivery| {
            notify_rust::Notification::new()
                .summary(&delivery.title)
                .body(&delivery.body)
                .show()
                .map(|_| ())
                .map_err(|_| "backend")
        })
    }

    #[cfg(test)]
    pub(crate) fn recording() -> (Self, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let deliveries = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = std::sync::Arc::clone(&deliveries);
        let notifier = Self::with_delivery(move |delivery| {
            recorded.lock().unwrap().push(delivery.body.clone());
            Ok(())
        });
        (notifier, deliveries)
    }

    fn with_delivery<F>(deliver: F) -> Self
    where
        F: Fn(&Delivery) -> Result<(), &'static str> + Send + 'static,
    {
        Self::with_delivery_capacity(QUEUE_CAPACITY, deliver)
    }

    fn with_delivery_capacity<F>(capacity: usize, deliver: F) -> Self
    where
        F: Fn(&Delivery) -> Result<(), &'static str> + Send + 'static,
    {
        let (sender, receiver) = sync_channel(capacity);
        let dispatcher = thread::Builder::new()
            .name("prism-desktop-notifications".to_string())
            .spawn(move || {
                let mut last_failures =
                    BTreeMap::<(NotificationKind, &'static str), Instant>::new();
                while let Ok(message) = receiver.recv() {
                    match message {
                        DispatchMessage::Delivery(delivery) => match deliver(&delivery) {
                            Ok(()) => last_failures.retain(|(kind, _), _| *kind != delivery.kind),
                            Err(category) => {
                                let now = Instant::now();
                                let key = (delivery.kind, category);
                                let should_report = last_failures.get(&key).is_none_or(|at| {
                                    now.saturating_duration_since(*at) >= FAILURE_COOLDOWN
                                });
                                if should_report {
                                    emit_failure("delivery_failed", category, delivery.kind);
                                    last_failures.insert(key, now);
                                }
                            }
                        },
                        #[cfg(test)]
                        DispatchMessage::Barrier(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            })
            .ok();
        let sender = dispatcher.as_ref().map(|_| sender);
        if dispatcher.is_none() {
            emit_failure(
                "dispatcher_start_failed",
                "thread_spawn",
                NotificationKind::Failed,
            );
        }
        Self {
            states: BTreeMap::new(),
            sender,
            dispatcher,
            last_queue_failures: BTreeMap::new(),
        }
    }

    pub(crate) fn seed<'a>(
        &mut self,
        observations: impl IntoIterator<Item = AgentObservation<'a>>,
    ) {
        for observation in observations {
            self.states
                .insert(observation.session.clone(), observation.state);
        }
    }

    pub(crate) fn retain(&mut self, live: &BTreeSet<WorktreeSessionKey>) {
        self.states.retain(|session, _| live.contains(session));
    }

    pub(crate) fn baseline(&mut self, observation: AgentObservation<'_>) {
        self.states
            .insert(observation.session.clone(), observation.state);
    }

    pub(crate) fn observe(&mut self, observation: AgentObservation<'_>) {
        let previous = self
            .states
            .insert(observation.session.clone(), observation.state);
        let Some(previous) = previous else {
            return;
        };
        let Some(kind) = transition(previous, observation.state) else {
            return;
        };
        if !enabled(observation.config, kind) {
            return;
        }
        let subject = if observation.repo_label.is_empty() {
            observation.branch.to_string()
        } else {
            format!("{}: {}", observation.repo_label, observation.branch)
        };
        let suffix = match kind {
            NotificationKind::NeedsInput => "is waiting for input",
            NotificationKind::Completed => "finished",
            NotificationKind::Failed => "failed",
            NotificationKind::NeedsRestart => "needs to be restarted",
        };
        self.enqueue(Delivery {
            kind,
            title: format!("Prism: {}", kind.title()),
            body: format!("{subject} {suffix}"),
        });
    }

    fn enqueue(&mut self, delivery: Delivery) {
        let kind = delivery.kind;
        let failure = match self.sender.as_ref() {
            Some(sender) => match sender.try_send(DispatchMessage::Delivery(delivery)) {
                Ok(()) => None,
                Err(TrySendError::Full(_)) => Some("queue_full"),
                Err(TrySendError::Disconnected(_)) => Some("dispatcher_disconnected"),
            },
            None => Some("dispatcher_unavailable"),
        };
        if let Some(action) = failure {
            let now = Instant::now();
            let key = (kind, action);
            if self
                .last_queue_failures
                .get(&key)
                .is_none_or(|at| now.saturating_duration_since(*at) >= FAILURE_COOLDOWN)
            {
                emit_failure(action, "channel", kind);
                self.last_queue_failures.insert(key, now);
            }
        } else {
            self.last_queue_failures
                .retain(|(failed_kind, _), _| *failed_kind != kind);
        }
    }

    #[cfg(test)]
    pub(crate) fn flush(&self) {
        let (tx, rx) = std::sync::mpsc::channel();
        if self
            .sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(DispatchMessage::Barrier(tx)).is_ok())
        {
            let _ = rx.recv_timeout(Duration::from_secs(1));
        }
    }
}

impl Drop for DesktopNotifier {
    fn drop(&mut self) {
        self.sender.take();
        let Some(dispatcher) = self.dispatcher.take() else {
            return;
        };
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while !dispatcher.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if dispatcher.is_finished() {
            let _ = dispatcher.join();
        }
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

fn emit_failure(action: &'static str, category: &'static str, kind: NotificationKind) {
    crate::observability::emit_deferred(crate::observability::EventInput {
        level: crate::observability::LogLevel::Warn,
        target: "desktop_notification",
        action,
        operation_id: None,
        parent_operation_id: None,
        branch: None,
        session: None,
        message: format!(
            "desktop notification {} for {}",
            action.replace('_', " "),
            kind.label()
        ),
        data_json: Some(format!(
            "{{\"platform\":\"{}\",\"category\":\"{}\",\"kind\":\"{}\"}}",
            std::env::consts::OS,
            category,
            kind.label()
        )),
    });
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::session::{WorktreeRepositoryKey, WorktreeSessionKey};

    fn key(repo: &str, branch: &str) -> WorktreeSessionKey {
        WorktreeSessionKey {
            repository: WorktreeRepositoryKey::new(format!("/tmp/{repo}").into()),
            path: format!("/tmp/{repo}/{branch}").into(),
            branch: branch.to_string(),
            incarnation: "1".to_string(),
        }
    }

    fn config() -> NotificationConfig {
        NotificationConfig {
            enabled: true,
            ..NotificationConfig::default()
        }
    }

    fn notifier() -> (DesktopNotifier, Arc<Mutex<Vec<Delivery>>>) {
        let deliveries = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&deliveries);
        let notifier = DesktopNotifier::with_delivery(move |delivery| {
            recorded.lock().unwrap().push(delivery.clone());
            Ok(())
        });
        (notifier, deliveries)
    }

    fn observation<'a>(
        session: &'a WorktreeSessionKey,
        state: AgentState,
        config: NotificationConfig,
    ) -> AgentObservation<'a> {
        AgentObservation {
            session,
            repo_label: "repo",
            branch: &session.branch,
            state,
            config,
        }
    }

    #[test]
    fn notifies_once_for_active_to_attention_and_rearms() {
        let session = key("one", "feature/auth");
        let (mut notifier, deliveries) = notifier();
        notifier.seed([observation(&session, AgentState::Running, config())]);
        notifier.observe(observation(&session, AgentState::NeedsInput, config()));
        notifier.observe(observation(&session, AgentState::NeedsInput, config()));
        notifier.observe(observation(&session, AgentState::Running, config()));
        notifier.observe(observation(&session, AgentState::NeedsInput, config()));
        notifier.flush();
        let deliveries = deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 2);
        assert_eq!(
            deliveries[0].body,
            "repo: feature/auth is waiting for input"
        );
    }

    #[test]
    fn seed_terminal_states_and_disabled_config_emit_nothing() {
        let completed = key("one", "completed");
        let failed = key("one", "failed");
        let (mut notifier, deliveries) = notifier();
        notifier.seed([
            observation(&completed, AgentState::ExitedOk, config()),
            observation(&failed, AgentState::ExitedError, config()),
        ]);
        let running = key("one", "running");
        notifier.seed([observation(&running, AgentState::Running, config())]);
        notifier.observe(observation(
            &running,
            AgentState::ExitedOk,
            NotificationConfig::default(),
        ));
        notifier.flush();
        assert!(deliveries.lock().unwrap().is_empty());
    }

    #[test]
    fn switches_control_each_notification_kind() {
        for (state, field) in [
            (AgentState::NeedsInput, "needs_input"),
            (AgentState::ExitedOk, "completed"),
            (AgentState::ExitedError, "failed"),
            (AgentState::NeedsRestart, "failed"),
        ] {
            let session = key("one", field);
            let (mut notifier, deliveries) = notifier();
            notifier.seed([observation(&session, AgentState::Attached, config())]);
            let mut disabled = config();
            match field {
                "needs_input" => disabled.needs_input = false,
                "completed" => disabled.completed = false,
                _ => disabled.failed = false,
            }
            notifier.observe(observation(&session, state, disabled));
            notifier.flush();
            assert!(deliveries.lock().unwrap().is_empty(), "{state:?}");
        }
    }

    #[test]
    fn inactive_and_attention_transitions_do_not_notify() {
        let session = key("one", "feature");
        let (mut notifier, deliveries) = notifier();
        notifier.seed([observation(&session, AgentState::Idle, config())]);
        notifier.observe(observation(&session, AgentState::ExitedOk, config()));
        notifier.observe(observation(&session, AgentState::ExitedError, config()));
        notifier.flush();
        assert!(deliveries.lock().unwrap().is_empty());
    }

    #[test]
    fn stable_session_keys_keep_repositories_independent() {
        let first = key("one", "feature");
        let second = key("two", "feature");
        let (mut notifier, deliveries) = notifier();
        notifier.seed([
            observation(&first, AgentState::Running, config()),
            observation(&second, AgentState::Running, config()),
        ]);
        notifier.observe(observation(&first, AgentState::ExitedOk, config()));
        notifier.observe(observation(&second, AgentState::ExitedOk, config()));
        notifier.flush();
        assert_eq!(deliveries.lock().unwrap().len(), 2);
    }

    #[test]
    fn reseeding_current_state_does_not_replay_it() {
        let session = key("one", "feature");
        let (mut notifier, deliveries) = notifier();
        notifier.seed([observation(&session, AgentState::Running, config())]);
        notifier.seed([observation(&session, AgentState::ExitedOk, config())]);
        notifier.flush();
        assert!(deliveries.lock().unwrap().is_empty());
    }

    #[test]
    fn queue_saturation_drops_without_blocking() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let mut notifier = DesktopNotifier::with_delivery_capacity(1, move |_| {
            let _ = started_tx.send(());
            let _ = release_rx.lock().unwrap().recv();
            Ok(())
        });
        let sessions = [key("one", "a"), key("one", "b"), key("one", "c")];
        notifier.seed(
            sessions
                .iter()
                .map(|session| observation(session, AgentState::Running, config())),
        );

        notifier.observe(observation(&sessions[0], AgentState::ExitedOk, config()));
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        notifier.observe(observation(&sessions[1], AgentState::ExitedOk, config()));
        let started = Instant::now();
        notifier.observe(observation(&sessions[2], AgentState::ExitedOk, config()));
        assert!(started.elapsed() < Duration::from_millis(50));

        let _ = release_tx.send(());
        let _ = release_tx.send(());
        notifier.flush();
    }

    #[test]
    fn delivery_failure_is_non_fatal() {
        let session = key("one", "feature");
        let mut notifier = DesktopNotifier::with_delivery(|_| Err("backend"));
        notifier.seed([observation(&session, AgentState::Running, config())]);
        notifier.observe(observation(&session, AgentState::ExitedError, config()));
        notifier.flush();
    }
}
