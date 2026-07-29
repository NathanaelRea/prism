use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(crate) type JobId = u64;

#[derive(Clone, Debug)]
pub(crate) struct JobMetadata<K, Q> {
    pub id: JobId,
    pub kind: K,
    pub key: Q,
    pub generation: u64,
    pub started_at: Instant,
    pub deadline: Option<Instant>,
}

#[derive(Debug)]
pub(crate) enum JobOutcome<P> {
    Completed(Result<Option<P>, String>),
    Panicked(String),
    Canceled,
    DeadlineExceeded,
}

pub(crate) enum JobMessage<K, Q, P> {
    Payload {
        metadata: JobMetadata<K, Q>,
        payload: P,
    },
    Terminal {
        metadata: JobMetadata<K, Q>,
        outcome: JobOutcome<P>,
    },
}

struct CancellationState {
    canceled: AtomicBool,
    mutex: Mutex<()>,
    wake: Condvar,
}

pub(crate) struct JobContext<K, Q, P> {
    metadata: JobMetadata<K, Q>,
    cancellation: Arc<CancellationState>,
    tx: mpsc::Sender<JobMessage<K, Q, P>>,
}

impl<K: Clone, Q: Clone, P> Clone for JobContext<K, Q, P> {
    fn clone(&self) -> Self {
        Self {
            metadata: self.metadata.clone(),
            cancellation: self.cancellation.clone(),
            tx: self.tx.clone(),
        }
    }
}

impl<K: Clone, Q: Clone, P> JobContext<K, Q, P> {
    pub(crate) fn is_canceled(&self) -> bool {
        self.cancellation.canceled.load(Ordering::Acquire)
            || self
                .metadata
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub(crate) fn send(&self, payload: P) -> Result<(), String> {
        if self.is_canceled() {
            return Err("job canceled".to_string());
        }
        self.tx
            .send(JobMessage::Payload {
                metadata: self.metadata.clone(),
                payload,
            })
            .map_err(|_| "job delivery receiver disconnected".to_string())
    }

    pub(crate) fn wait(&self, duration: Duration) -> bool {
        if self.is_canceled() {
            return true;
        }
        let wait = self
            .metadata
            .deadline
            .map(|deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(duration)
            })
            .unwrap_or(duration);
        let guard = self
            .cancellation
            .mutex
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _ = self
            .cancellation
            .wake
            .wait_timeout_while(guard, wait, |_| !self.is_canceled());
        self.is_canceled()
    }
}

struct JobEntry<K, Q> {
    metadata: JobMetadata<K, Q>,
    cancellation: Arc<CancellationState>,
    handle: JoinHandle<()>,
}

pub(crate) struct JobRegistry<K, Q, P> {
    next_id: JobId,
    accepting: bool,
    jobs: BTreeMap<JobId, JobEntry<K, Q>>,
    tx: mpsc::Sender<JobMessage<K, Q, P>>,
    rx: mpsc::Receiver<JobMessage<K, Q, P>>,
    #[cfg(test)]
    fail_next_spawn: bool,
}

impl<K, Q, P> Default for JobRegistry<K, Q, P> {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            next_id: 1,
            accepting: true,
            jobs: BTreeMap::new(),
            tx,
            rx,
            #[cfg(test)]
            fail_next_spawn: false,
        }
    }
}

impl<K, Q, P> JobRegistry<K, Q, P>
where
    K: Clone + Send + 'static,
    Q: Clone + Send + 'static,
    P: Send + 'static,
{
    pub(crate) fn spawn<F>(
        &mut self,
        kind: K,
        key: Q,
        generation: u64,
        timeout: Option<Duration>,
        name: String,
        job: F,
    ) -> JobId
    where
        F: FnOnce(JobContext<K, Q, P>) -> Result<Option<P>, String> + Send + 'static,
    {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let started_at = Instant::now();
        let metadata = JobMetadata {
            id,
            kind,
            key,
            generation,
            started_at,
            deadline: timeout.map(|timeout| started_at + timeout),
        };
        if !self.accepting {
            let _ = self.tx.send(JobMessage::Terminal {
                metadata,
                outcome: JobOutcome::Completed(Err("TUI is shutting down".to_string())),
            });
            return id;
        }

        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_spawn) {
            let _ = self.tx.send(JobMessage::Terminal {
                metadata,
                outcome: JobOutcome::Completed(Err("injected thread spawn failure".to_string())),
            });
            return id;
        }

        let cancellation = Arc::new(CancellationState {
            canceled: AtomicBool::new(false),
            mutex: Mutex::new(()),
            wake: Condvar::new(),
        });
        let context = JobContext {
            metadata: metadata.clone(),
            cancellation: cancellation.clone(),
            tx: self.tx.clone(),
        };
        let terminal_tx = self.tx.clone();
        let terminal_metadata = metadata.clone();
        let thread = thread::Builder::new().name(name).spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| job(context.clone())));
            let outcome = match result {
                Err(payload) => JobOutcome::Panicked(panic_message(payload)),
                Ok(_)
                    if terminal_metadata
                        .deadline
                        .is_some_and(|deadline| Instant::now() >= deadline) =>
                {
                    JobOutcome::DeadlineExceeded
                }
                Ok(_) if context.is_canceled() => JobOutcome::Canceled,
                Ok(result) => JobOutcome::Completed(result),
            };
            let _ = terminal_tx.send(JobMessage::Terminal {
                metadata: terminal_metadata,
                outcome,
            });
        });
        match thread {
            Ok(handle) => {
                self.jobs.insert(
                    id,
                    JobEntry {
                        metadata,
                        cancellation,
                        handle,
                    },
                );
            }
            Err(error) => {
                let _ = self.tx.send(JobMessage::Terminal {
                    metadata,
                    outcome: JobOutcome::Completed(Err(format!("spawn job thread: {error}"))),
                });
            }
        }
        id
    }

    pub(crate) fn drain(&mut self) -> Vec<JobMessage<K, Q, P>> {
        self.cancel_expired();
        let mut messages = self.rx.try_iter().collect::<Vec<_>>();
        for message in &mut messages {
            if let JobMessage::Terminal { metadata, outcome } = message
                && let Some(entry) = self.jobs.remove(&metadata.id)
            {
                debug_assert_eq!(entry.metadata.id, metadata.id);
                if let Err(payload) = entry.handle.join() {
                    *outcome = JobOutcome::Panicked(panic_message(payload));
                }
            }
        }
        messages
    }

    pub(crate) fn stop_accepting(&mut self) {
        self.accepting = false;
    }

    pub(crate) fn cancel_all(&self) {
        for entry in self.jobs.values() {
            entry.cancellation.canceled.store(true, Ordering::Release);
            entry.cancellation.wake.notify_all();
        }
    }

    pub(crate) fn cancel(&self, id: JobId) {
        if let Some(entry) = self.jobs.get(&id) {
            entry.cancellation.canceled.store(true, Ordering::Release);
            entry.cancellation.wake.notify_all();
        }
    }

    pub(crate) fn active_metadata(&self) -> Vec<JobMetadata<K, Q>> {
        self.jobs
            .values()
            .map(|entry| entry.metadata.clone())
            .collect()
    }

    pub(crate) fn has_jobs(&self) -> bool {
        !self.jobs.is_empty()
    }

    pub(crate) fn abandon_unfinished(&mut self) -> usize {
        let count = self.jobs.len();
        self.jobs.clear();
        count
    }

    fn cancel_expired(&self) {
        let now = Instant::now();
        for entry in self.jobs.values() {
            if entry
                .metadata
                .deadline
                .is_some_and(|deadline| now >= deadline)
            {
                entry.cancellation.canceled.store(true, Ordering::Release);
                entry.cancellation.wake.notify_all();
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_next_spawn(&mut self) {
        self.fail_next_spawn = true;
    }
}

impl<K, Q, P> Drop for JobRegistry<K, Q, P> {
    fn drop(&mut self) {
        self.accepting = false;
        for entry in self.jobs.values() {
            entry.cancellation.canceled.store(true, Ordering::Release);
            entry.cancellation.wake.notify_all();
        }
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{JobMessage, JobOutcome, JobRegistry};

    #[test]
    fn panic_emits_one_terminal_outcome_and_a_later_job_can_start() {
        let mut jobs = JobRegistry::<&'static str, String, ()>::default();
        jobs.spawn(
            "poll",
            "same-key".to_string(),
            1,
            Some(Duration::from_secs(1)),
            "panic-job".to_string(),
            |_| panic!("before result"),
        );
        let terminal = wait_for_terminal(&mut jobs);
        assert!(matches!(terminal, JobOutcome::Panicked(message) if message == "before result"));
        assert!(!jobs.has_jobs());

        jobs.spawn(
            "poll",
            "same-key".to_string(),
            1,
            Some(Duration::from_secs(1)),
            "replacement-job".to_string(),
            |_| Ok(None),
        );
        assert!(matches!(
            wait_for_terminal(&mut jobs),
            JobOutcome::Completed(Ok(None))
        ));
    }

    #[test]
    fn spawn_failure_emits_one_terminal_outcome_and_a_later_job_can_start() {
        let mut jobs = JobRegistry::<&'static str, String, ()>::default();
        jobs.fail_next_spawn();
        jobs.spawn(
            "poll",
            "same-key".to_string(),
            1,
            None,
            "failed-job".to_string(),
            |_| Ok(None),
        );
        let terminal = wait_for_terminal(&mut jobs);
        assert!(
            matches!(terminal, JobOutcome::Completed(Err(message)) if message.contains("spawn failure"))
        );

        jobs.spawn(
            "poll",
            "same-key".to_string(),
            2,
            None,
            "replacement-job".to_string(),
            |_| Ok(None),
        );
        assert!(matches!(
            wait_for_terminal(&mut jobs),
            JobOutcome::Completed(Ok(None))
        ));
    }

    #[test]
    fn dropping_registry_interrupts_a_listener_wait() {
        let (stopped_tx, stopped_rx) = std::sync::mpsc::sync_channel(0);
        let mut jobs = JobRegistry::<&'static str, String, ()>::default();
        jobs.spawn(
            "listener",
            "server".to_string(),
            0,
            None,
            "listener-job".to_string(),
            move |context| {
                while !context.wait(Duration::from_secs(60)) {}
                stopped_tx.send(()).unwrap();
                Ok(None)
            },
        );

        drop(jobs);

        stopped_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    fn wait_for_terminal(jobs: &mut JobRegistry<&'static str, String, ()>) -> JobOutcome<()> {
        let started = std::time::Instant::now();
        loop {
            if let Some(outcome) = jobs.drain().into_iter().find_map(|message| match message {
                JobMessage::Terminal { outcome, .. } => Some(outcome),
                JobMessage::Payload { .. } => None,
            }) {
                return outcome;
            }
            assert!(started.elapsed() < Duration::from_secs(1));
            std::thread::yield_now();
        }
    }
}
