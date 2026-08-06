#[cfg(test)]
use std::collections::{BTreeMap, BTreeSet};
#[cfg(target_os = "macos")]
use std::io::Write;
#[cfg(test)]
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
#[cfg(test)]
use std::thread::{self, JoinHandle};
#[cfg(test)]
use std::time::{Duration, Instant};

#[cfg(test)]
use crate::agent::AgentState;
#[cfg(test)]
use crate::config::NotificationConfig;
#[cfg(test)]
use crate::session::WorktreeSessionKey;

#[cfg(test)]
const QUEUE_CAPACITY: usize = 32;
#[cfg(test)]
const FAILURE_COOLDOWN: Duration = Duration::from_secs(60);
#[cfg(test)]
const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum NotificationKind {
    NeedsInput,
    Completed,
    Failed,
    NeedsRestart,
}

#[cfg(test)]
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

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct Delivery {
    kind: NotificationKind,
    title: String,
    body: String,
}

#[cfg(test)]
enum DispatchMessage {
    Delivery(Delivery),
    #[cfg(test)]
    Barrier(std::sync::mpsc::Sender<()>),
}

#[cfg(test)]
pub(crate) struct AgentObservation<'a> {
    pub session: &'a WorktreeSessionKey,
    pub repo_label: &'a str,
    pub branch: &'a str,
    pub state: AgentState,
    pub config: NotificationConfig,
}

#[cfg(test)]
pub(crate) struct DesktopNotifier {
    states: BTreeMap<WorktreeSessionKey, AgentState>,
    suppressed_returns: BTreeMap<WorktreeSessionKey, AgentState>,
    #[cfg(test)]
    sender: Option<SyncSender<DispatchMessage>>,
    #[cfg(test)]
    dispatcher: Option<JoinHandle<()>>,
    #[cfg(test)]
    last_queue_failures: BTreeMap<(NotificationKind, &'static str), Instant>,
}

#[cfg(test)]
impl DesktopNotifier {
    pub(crate) fn new() -> Self {
        Self {
            states: BTreeMap::new(),
            suppressed_returns: BTreeMap::new(),
            #[cfg(test)]
            sender: None,
            #[cfg(test)]
            dispatcher: None,
            #[cfg(test)]
            last_queue_failures: BTreeMap::new(),
        }
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

    #[cfg(test)]
    fn with_delivery<F>(deliver: F) -> Self
    where
        F: Fn(&Delivery) -> Result<(), &'static str> + Send + 'static,
    {
        Self::with_delivery_capacity(QUEUE_CAPACITY, deliver)
    }

    #[cfg(test)]
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
            suppressed_returns: BTreeMap::new(),
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
            self.suppressed_returns.remove(observation.session);
            self.states
                .insert(observation.session.clone(), observation.state);
        }
    }

    pub(crate) fn retain(&mut self, live: &BTreeSet<WorktreeSessionKey>) {
        self.states.retain(|session, _| live.contains(session));
        self.suppressed_returns
            .retain(|session, _| live.contains(session));
    }

    pub(crate) fn baseline(&mut self, observation: AgentObservation<'_>) {
        self.suppressed_returns.remove(observation.session);
        self.states
            .insert(observation.session.clone(), observation.state);
    }

    pub(crate) fn observe_attached_liveness(&mut self, observation: AgentObservation<'_>) {
        debug_assert_eq!(observation.state, AgentState::Attached);
        let previous = self
            .states
            .insert(observation.session.clone(), observation.state);
        if let Some(previous) = previous
            && previous != AgentState::Attached
        {
            self.suppressed_returns
                .insert(observation.session.clone(), previous);
        }
    }

    pub(crate) fn observe(&mut self, observation: AgentObservation<'_>) {
        let suppressed_return = self.suppressed_returns.remove(observation.session);
        let previous = self
            .states
            .insert(observation.session.clone(), observation.state);
        let Some(previous) = previous else {
            return;
        };
        if suppressed_return == Some(observation.state) {
            return;
        }
        let Some(kind) = transition(previous, observation.state) else {
            return;
        };
        if !enabled(observation.config, kind) {
            return;
        }
        #[cfg(test)]
        {
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
    }

    #[cfg(test)]
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

#[cfg(target_os = "linux")]
pub(crate) fn deliver_native_notification(title: &str, body: &str) -> Result<(), &'static str> {
    notify_rust::Notification::new()
        .summary(title)
        .body(body)
        .show()
        .map(|_| ())
        .map_err(|_| "backend")
}

#[cfg(target_os = "macos")]
pub(crate) fn deliver_terminal_notification(title: &str, body: &str) -> Result<(), &'static str> {
    let mut terminal = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .map_err(|_| "terminal_open")?;
    terminal
        .write_all(&terminal_notification_payload(title, body))
        .map_err(|_| "terminal_write")
}

#[cfg(any(target_os = "macos", test))]
fn terminal_notification_payload(title: &str, body: &str) -> Vec<u8> {
    let text = format!("{title}: {body}");
    let sanitized = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    format!("\x1b]9;{sanitized}\x1b\\").into_bytes()
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
fn enabled(config: NotificationConfig, kind: NotificationKind) -> bool {
    config.enabled
        && match kind {
            NotificationKind::NeedsInput => config.needs_input,
            NotificationKind::Completed => config.completed,
            NotificationKind::Failed | NotificationKind::NeedsRestart => config.failed,
        }
}

#[cfg(test)]
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
            completed: true,
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
        let disabled = NotificationConfig {
            enabled: false,
            ..NotificationConfig::default()
        };
        notifier.observe(observation(&running, AgentState::ExitedOk, disabled));
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
    fn attached_liveness_does_not_hide_a_real_completion() {
        let session = key("one", "feature");
        let (mut notifier, deliveries) = notifier();
        notifier.seed([observation(&session, AgentState::Running, config())]);

        notifier.observe(observation(&session, AgentState::Attached, config()));
        notifier.observe(observation(&session, AgentState::ExitedOk, config()));

        notifier.flush();
        assert_eq!(deliveries.lock().unwrap().len(), 1);
    }

    #[test]
    fn genuine_attached_state_rearms_a_known_session() {
        let session = key("one", "feature");
        let (mut notifier, deliveries) = notifier();
        notifier.seed([observation(&session, AgentState::ExitedOk, config())]);

        notifier.observe(observation(&session, AgentState::Attached, config()));
        notifier.observe(observation(&session, AgentState::ExitedError, config()));

        notifier.flush();
        assert_eq!(deliveries.lock().unwrap().len(), 1);
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

    #[test]
    fn macos_delivery_uses_a_sanitized_terminal_notification() {
        let payload =
            terminal_notification_payload("Prism: Failed", "repo: branch failed\u{1b}]9;injected");

        assert_eq!(
            payload,
            b"\x1b]9;Prism: Failed: repo: branch failed ]9;injected\x1b\\"
        );
    }
}
