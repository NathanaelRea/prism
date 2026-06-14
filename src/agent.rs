use std::collections::VecDeque;
use std::fs;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{AGENT_CANDIDATES, Config};
use crate::observability::{self, LogLevel};
use crate::process::{command_exists, split_command_words};
use crate::terminal::{WNOHANG, set_nonblocking};
use crate::util::timestamp_nanos;

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

pub fn agent_command_exists(config: &Config, agent: &str) -> bool {
    split_command_words(&config.agent_command(agent))
        .first()
        .map(|command| command_exists(command))
        .unwrap_or(false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    Idle,
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
            "running" => Some(Self::NeedsRestart),
            "done" => Some(Self::ExitedOk),
            "failed" => Some(Self::ExitedError),
            "needs restart" | "needs-restart" => Some(Self::NeedsRestart),
            "needs input" | "needs-input" => Some(Self::NeedsInput),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct AgentAdapter {
    command: String,
    pub prompt_mode: PromptMode,
}

#[derive(Debug)]
pub struct AgentLaunch {
    pub argv: Vec<String>,
    pub stdin_prompt: Option<String>,
    pub prompt_file: Option<PathBuf>,
}

impl AgentAdapter {
    pub fn from_config(config: &Config, name: &str) -> Self {
        Self {
            command: config.agent_command(name),
            prompt_mode: config.agent_prompt_mode(name),
        }
    }

    pub fn prepare_launch(&self, prompt: &str) -> Result<AgentLaunch, String> {
        let mut argv = split_command_words(&self.command);
        let mut prompt_file = None;
        let mut stdin_prompt = None;

        if prompt.is_empty() {
            return Ok(AgentLaunch {
                argv,
                stdin_prompt,
                prompt_file,
            });
        }

        match self.prompt_mode {
            PromptMode::Interactive => {}
            PromptMode::Stdin => {
                stdin_prompt = Some(prompt.to_string());
            }
            PromptMode::Argument => {
                if replace_argv_placeholder(&mut argv, "{prompt}", prompt) == 0 {
                    argv.push(prompt.to_string());
                }
            }
            PromptMode::TempFile => {
                let path = write_temp_prompt_file(prompt)?;
                let value = path.display().to_string();
                if replace_argv_placeholder(&mut argv, "{prompt_file}", &value) == 0 {
                    argv.push(value);
                }
                prompt_file = Some(path);
            }
        }

        Ok(AgentLaunch {
            argv,
            stdin_prompt,
            prompt_file,
        })
    }
}

fn replace_argv_placeholder(argv: &mut [String], needle: &str, replacement: &str) -> usize {
    let mut count = 0;
    for arg in argv {
        if arg.contains(needle) {
            *arg = arg.replace(needle, replacement);
            count += 1;
        }
    }
    count
}

fn write_temp_prompt_file(prompt: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("prism-prompts");
    fs::create_dir_all(&dir).map_err(|error| format!("create prompt temp dir: {error}"))?;
    let path = dir.join(format!(
        "prompt-{}-{}.md",
        std::process::id(),
        timestamp_nanos()
    ));
    fs::write(&path, prompt).map_err(|error| format!("write prompt temp file: {error}"))?;
    Ok(path)
}

#[derive(Debug)]
pub struct AgentProcess {
    pid: i32,
    master_fd: RawFd,
    prompt_file: Option<PathBuf>,
}

impl AgentProcess {
    pub fn spawn(
        argv: &[String],
        workdir: &Path,
        prompt_file: Option<PathBuf>,
    ) -> Result<Self, String> {
        let operation = observability::begin_operation(
            LogLevel::Debug,
            "agent",
            "spawn",
            "starting agent process",
            Some(observability::agent_spawn_data_json(argv, workdir)),
        );
        let mut master_fd = 0;
        let pid = unsafe {
            forkpty(
                &mut master_fd,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if pid < 0 {
            operation.finish(
                LogLevel::Error,
                "agent",
                "spawn_error",
                "forkpty failed",
                Some(observability::agent_spawn_data_json(argv, workdir)),
            );
            return Err("forkpty failed".to_string());
        }
        if pid == 0 {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]).current_dir(workdir);
            let error = command.exec();
            eprintln!("exec {}: {error}", argv[0]);
            std::process::exit(127);
        }
        if let Err(error) = set_nonblocking(master_fd) {
            operation.finish(
                LogLevel::Error,
                "agent",
                "spawn_error",
                format!("set nonblocking failed: {error}"),
                Some(observability::agent_spawn_data_json(argv, workdir)),
            );
            return Err(error);
        }
        operation.finish(
            LogLevel::Debug,
            "agent",
            "spawned",
            format!("agent process started pid={pid}"),
            Some(observability::agent_spawn_data_json(argv, workdir)),
        );
        Ok(Self {
            pid,
            master_fd,
            prompt_file,
        })
    }

    pub fn write_all(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut written = 0;
        while written < bytes.len() {
            let count = unsafe {
                write(
                    self.master_fd,
                    bytes[written..].as_ptr().cast(),
                    bytes.len() - written,
                )
            };
            if count < 0 {
                return Err("write to agent PTY failed".to_string());
            }
            written += count as usize;
        }
        Ok(())
    }

    pub fn drain_output(&mut self) -> Vec<String> {
        let mut chunks = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = unsafe { read(self.master_fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if count > 0 {
                chunks.push(String::from_utf8_lossy(&buffer[..count as usize]).to_string());
            } else {
                break;
            }
        }
        chunks
    }

    pub fn try_wait(&mut self) -> Option<AgentState> {
        let mut status = 0;
        let result = unsafe { waitpid(self.pid, &mut status, WNOHANG) };
        if result == 0 {
            None
        } else if result == self.pid && exited_successfully(status) {
            Some(AgentState::ExitedOk)
        } else if result == self.pid {
            Some(AgentState::ExitedError)
        } else {
            Some(AgentState::NeedsRestart)
        }
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        let _ = unsafe { close(self.master_fd) };
        if let Some(path) = &self.prompt_file {
            let _ = fs::remove_file(path);
        }
    }
}

pub fn output_tail(output: &VecDeque<String>) -> String {
    output
        .back()
        .map(|chunk| chunk.replace('\r', "").replace('\n', " "))
        .unwrap_or_default()
}

fn exited_successfully(status: i32) -> bool {
    status & 0x7f == 0 && ((status >> 8) & 0xff) == 0
}

#[repr(C)]
struct Termios {
    data: [u8; 60],
}

#[repr(C)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

#[link(name = "util")]
unsafe extern "C" {
    fn forkpty(
        amaster: *mut i32,
        name: *mut i8,
        termp: *const Termios,
        winp: *const Winsize,
    ) -> i32;
}

unsafe extern "C" {
    fn read(fd: i32, buf: *mut std::ffi::c_void, count: usize) -> isize;
    fn write(fd: i32, buf: *const std::ffi::c_void, count: usize) -> isize;
    fn close(fd: i32) -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_adapter_appends_argument_prompt_without_placeholder() {
        let adapter = AgentAdapter {
            command: "my-agent run".to_string(),
            prompt_mode: PromptMode::Argument,
        };
        let launch = adapter.prepare_launch("fix this").unwrap();
        assert_eq!(launch.argv, vec!["my-agent", "run", "fix this"]);
        assert!(launch.stdin_prompt.is_none());
    }

    #[test]
    fn agent_adapter_replaces_argument_placeholder() {
        let adapter = AgentAdapter {
            command: "my-agent --prompt {prompt}".to_string(),
            prompt_mode: PromptMode::Argument,
        };
        let launch = adapter.prepare_launch("fix this").unwrap();
        assert_eq!(launch.argv, vec!["my-agent", "--prompt", "fix this"]);
    }

    #[test]
    fn agent_adapter_uses_stdin_prompt() {
        let adapter = AgentAdapter {
            command: "my-agent".to_string(),
            prompt_mode: PromptMode::Stdin,
        };
        let launch = adapter.prepare_launch("fix this").unwrap();
        assert_eq!(launch.argv, vec!["my-agent"]);
        assert_eq!(launch.stdin_prompt.as_deref(), Some("fix this"));
    }

    #[test]
    fn opencode_uses_argument_prompt_for_json_run_mode() {
        assert_eq!(builtin_prompt_mode("opencode"), PromptMode::Argument);
    }
}
