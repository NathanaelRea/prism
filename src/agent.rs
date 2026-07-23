use crate::config::{AGENT_CANDIDATES, Config};
use crate::process::command_exists;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptMode {
    Interactive,
    Stdin,
    Argument,
    TempFile,
}

impl PromptMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "interactive" => Some(Self::Interactive),
            "stdin" => Some(Self::Stdin),
            "argument" | "arg" => Some(Self::Argument),
            "temp-file" | "temp_file" | "file" => Some(Self::TempFile),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Stdin => "stdin",
            Self::Argument => "argument",
            Self::TempFile => "temp-file",
        }
    }
}

pub fn builtin_prompt_mode(agent: &str) -> PromptMode {
    match agent {
        "opencode" => PromptMode::Argument,
        _ => PromptMode::Interactive,
    }
}

pub fn detected_agents(config: &Config) -> Vec<String> {
    AGENT_CANDIDATES
        .iter()
        .filter(|agent| command_exists(&config.tool(agent)))
        .map(|agent| (*agent).to_string())
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Attached,
    Running,
    ExitedOk,
    ExitedError,
    NeedsRestart,
    NeedsInput,
}

impl AgentState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Attached => "process running",
            Self::Running => "running",
            Self::ExitedOk => "done",
            Self::ExitedError => "failed",
            Self::NeedsRestart => "needs restart",
            Self::NeedsInput => "needs input",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "idle" => Some(Self::Idle),
            "process running" | "attached" => Some(Self::Attached),
            "running" => Some(Self::Running),
            "done" => Some(Self::ExitedOk),
            "failed" => Some(Self::ExitedError),
            "needs restart" | "needs-restart" => Some(Self::NeedsRestart),
            "needs input" | "needs-input" => Some(Self::NeedsInput),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_uses_argument_prompt_for_json_run_mode() {
        assert_eq!(builtin_prompt_mode("opencode"), PromptMode::Argument);
    }

    #[test]
    fn agent_state_labels_parse_back_to_same_state() {
        for state in [
            AgentState::Idle,
            AgentState::Attached,
            AgentState::Running,
            AgentState::ExitedOk,
            AgentState::ExitedError,
            AgentState::NeedsRestart,
            AgentState::NeedsInput,
        ] {
            assert_eq!(AgentState::parse(state.label()), Some(state));
        }
    }
}
