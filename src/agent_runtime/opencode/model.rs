//! Provider-neutral OpenCode runtime and observation models.

use crate::agent::AgentState;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpencodeRuntime {
    pub repo_root: String,
    pub harness_id: String,
    pub branch: String,
    pub worktree_path: String,
    pub server_port: u16,
    pub server_url: String,
    pub server_pid: Option<u32>,
    pub server_process_identity: Option<u64>,
    pub opencode_session_id: Option<String>,
    pub generation: u64,
    pub updated_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortStatus {
    Free,
    OpenCode,
    Occupied,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpencodeSession {
    pub id: String,
    pub directory: Option<String>,
    pub title: Option<String>,
    pub time_updated: Option<String>,
    pub parent_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpencodeState {
    Unknown,
    Starting,
    Idle,
    Done,
    Busy,
    Retry,
    NeedsInput,
    Error,
    Offline,
}

impl OpencodeState {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::Done => "done",
            Self::Busy => "busy",
            Self::Retry => "retry",
            Self::NeedsInput => "needs input",
            Self::Error => "error",
            Self::Offline => "offline",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "unknown" => Some(Self::Unknown),
            "starting" | "loading" => Some(Self::Starting),
            "idle" | "ready" => Some(Self::Idle),
            "done" | "completed" => Some(Self::Done),
            "busy" | "running" | "working" => Some(Self::Busy),
            "retry" | "retrying" => Some(Self::Retry),
            "needs input" | "needs-input" | "permission" => Some(Self::NeedsInput),
            "error" | "failed" => Some(Self::Error),
            "offline" | "disconnected" => Some(Self::Offline),
            _ => None,
        }
    }

    pub fn agent_state(self) -> AgentState {
        match self {
            Self::Unknown => AgentState::NeedsRestart,
            Self::Starting => AgentState::Running,
            Self::Idle => AgentState::Idle,
            Self::Done => AgentState::ExitedOk,
            Self::Busy | Self::Retry => AgentState::Running,
            Self::NeedsInput => AgentState::NeedsInput,
            Self::Error => AgentState::ExitedError,
            Self::Offline => AgentState::NeedsRestart,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpencodeTodo {
    pub text: String,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpencodeStatus {
    pub server_url: Option<String>,
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub state: OpencodeState,
    pub detail: Option<String>,
    pub latest_message: Option<String>,
    pub latest_user_message: Option<String>,
    pub recent_messages: Vec<String>,
    pub active_tool: Option<String>,
    pub todos: Vec<OpencodeTodo>,
    pub last_updated_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpencodeEvent {
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub state: Option<OpencodeState>,
    pub detail: Option<String>,
    pub latest_message: Option<String>,
    pub active_tool: Option<String>,
    pub todos: Option<Vec<OpencodeTodo>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpencodeSnapshotFacet {
    Status,
    Message,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_labels_parsing_and_agent_mapping_are_exhaustive() {
        let cases = [
            (OpencodeState::Unknown, "unknown", AgentState::NeedsRestart),
            (OpencodeState::Starting, "starting", AgentState::Running),
            (OpencodeState::Idle, "idle", AgentState::Idle),
            (OpencodeState::Done, "done", AgentState::ExitedOk),
            (OpencodeState::Busy, "busy", AgentState::Running),
            (OpencodeState::Retry, "retry", AgentState::Running),
            (
                OpencodeState::NeedsInput,
                "needs input",
                AgentState::NeedsInput,
            ),
            (OpencodeState::Error, "error", AgentState::ExitedError),
            (OpencodeState::Offline, "offline", AgentState::NeedsRestart),
        ];
        for (state, label, agent) in cases {
            assert_eq!(state.label(), label);
            assert_eq!(OpencodeState::parse(label), Some(state));
            assert_eq!(state.agent_state(), agent);
        }
        for (alias, expected) in [
            ("loading", OpencodeState::Starting),
            ("ready", OpencodeState::Idle),
            ("completed", OpencodeState::Done),
            ("running", OpencodeState::Busy),
            ("working", OpencodeState::Busy),
            ("retrying", OpencodeState::Retry),
            ("needs-input", OpencodeState::NeedsInput),
            ("permission", OpencodeState::NeedsInput),
            ("failed", OpencodeState::Error),
            ("disconnected", OpencodeState::Offline),
        ] {
            assert_eq!(OpencodeState::parse(alias), Some(expected));
        }
        assert_eq!(OpencodeState::parse("  READY  "), Some(OpencodeState::Idle));
        assert_eq!(OpencodeState::parse("bogus"), None);
    }
}
