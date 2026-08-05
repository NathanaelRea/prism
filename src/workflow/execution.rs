use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) use crate::persistence::workflow as persistence;

static ID_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowKind {
    Auto,
    Plan,
}

impl WorkflowKind {
    pub fn as_str(self) -> &'static str {
        self.label()
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Plan => "plan",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "plan" => Ok(Self::Plan),
            other => Err(format!("unknown workflow kind: {other}")),
        }
    }
}

impl std::fmt::Display for WorkflowKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

impl PartialEq<&str> for WorkflowKind {
    fn eq(&self, other: &&str) -> bool {
        self.label() == *other
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    Queued,
    Claimed,
    RecoveryPending,
    Paused,
    Terminal,
}

impl DispatchState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Claimed => "claimed",
            Self::RecoveryPending => "recovery_pending",
            Self::Paused => "paused",
            Self::Terminal => "terminal",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "queued" => Ok(Self::Queued),
            "claimed" => Ok(Self::Claimed),
            "recovery_pending" => Ok(Self::RecoveryPending),
            "paused" => Ok(Self::Paused),
            "terminal" => Ok(Self::Terminal),
            other => Err(format!("unknown dispatch state: {other}")),
        }
    }
}

impl std::ops::Deref for DispatchState {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.label()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkflowIdentity {
    pub kind: WorkflowKind,
    pub run_id: String,
}

impl WorkflowIdentity {
    pub fn new(kind: WorkflowKind, run_id: impl Into<String>) -> Self {
        Self {
            kind,
            run_id: run_id.into(),
        }
    }
}

pub fn new_instance_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}-{}",
        std::process::id(),
        now_ms(),
        ID_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

pub fn enqueue(path: &Path, workflow: &WorkflowIdentity) -> Result<(), String> {
    store(path)?.enqueue(workflow, now_ms()).map_err(to_string)
}

pub fn dispatch_state(
    path: &Path,
    workflow: &WorkflowIdentity,
) -> Result<Option<DispatchState>, String> {
    store(path)?.dispatch_state(workflow).map_err(to_string)
}

pub fn mark_abandoned(path: &Path, daemon_instance_id: &str) -> Result<usize, String> {
    store(path)?
        .mark_abandoned(daemon_instance_id, now_ms())
        .map_err(to_string)
}

fn store(path: &Path) -> Result<persistence::WorkflowStore, String> {
    persistence::WorkflowStore::open(path).map_err(to_string)
}

fn to_string(error: persistence::WorkflowError) -> String {
    error.to_string()
}

pub fn is_stale_claim_error(error: &str) -> bool {
    error == "execution claim is stale"
}

pub(crate) fn claim_write_error(context: &str, error: impl std::fmt::Display) -> String {
    let error = error.to_string();
    if is_stale_claim_error(&error) {
        error
    } else {
        format!("{context}: {error}")
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
