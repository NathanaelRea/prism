use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptTransport {
    Argument,
    Stdin,
    TempFile,
}

impl PromptTransport {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "argument" => Some(Self::Argument),
            "stdin" => Some(Self::Stdin),
            "temp-file" => Some(Self::TempFile),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Argument => "argument",
            Self::Stdin => "stdin",
            Self::TempFile => "temp-file",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    JsonLines,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessConfig {
    pub adapter: String,
    pub interactive_command: Vec<String>,
    pub arguments: Vec<String>,
    pub interactive_prompt_transport: Option<PromptTransport>,
    pub headless_command: Option<Vec<String>>,
    pub headless_prompt_transport: Option<PromptTransport>,
    pub output_format: OutputFormat,
    pub environment: BTreeMap<String, String>,
}

impl HarnessConfig {
    pub fn opencode(program: impl Into<String>) -> Self {
        Self::builtin("opencode", program)
    }

    pub fn builtin(adapter: &str, program: impl Into<String>) -> Self {
        Self {
            adapter: adapter.to_string(),
            interactive_command: vec![program.into()],
            arguments: Vec::new(),
            interactive_prompt_transport: None,
            headless_command: None,
            headless_prompt_transport: None,
            output_format: OutputFormat::JsonLines,
            environment: BTreeMap::new(),
        }
    }

    pub fn validate(&self, id: &str) -> Result<(), String> {
        if self.interactive_command.is_empty() || self.interactive_command[0].trim().is_empty() {
            return Err(format!(
                "harness '{id}' requires a non-empty interactive_command"
            ));
        }
        if !matches!(
            self.adapter.as_str(),
            "generic" | "opencode" | "codex" | "claude" | "pi"
        ) {
            return Err(format!(
                "harness '{id}' uses unsupported adapter '{}'; supported adapters: opencode, codex, claude, pi, generic",
                self.adapter
            ));
        }
        if let Some(key) = self
            .environment
            .keys()
            .find(|key| !valid_environment_name(key))
        {
            return Err(format!(
                "harness '{id}' has invalid environment variable name '{key}'"
            ));
        }
        if self.adapter != "generic" {
            if self.headless_command.is_some()
                || self.headless_prompt_transport.is_some()
                || self.interactive_prompt_transport.is_some()
            {
                return Err(format!(
                    "harness '{id}' uses the {} adapter; Prism owns its prompt transport and headless protocol arguments",
                    self.adapter
                ));
            }
            validate_builtin_arguments(id, &self.adapter, &self.arguments)?;
            return Ok(());
        }
        if !self.arguments.is_empty() {
            return Err(format!(
                "generic harness '{id}' configures commands directly and cannot use arguments"
            ));
        }
        validate_transport(
            id,
            "interactive",
            &self.interactive_command,
            self.interactive_prompt_transport,
            false,
        )?;
        match (&self.headless_command, self.headless_prompt_transport) {
            (Some(command), Some(transport)) => {
                validate_transport(id, "headless", command, Some(transport), true)?
            }
            (Some(_), None) => {
                return Err(format!(
                    "harness '{id}' configures headless_command but not headless_prompt_transport"
                ));
            }
            (None, Some(_)) => {
                return Err(format!(
                    "harness '{id}' configures headless_prompt_transport but not headless_command"
                ));
            }
            (None, None) => {}
        }
        Ok(())
    }
}

fn validate_builtin_arguments(id: &str, adapter: &str, arguments: &[String]) -> Result<(), String> {
    let reserved: &[&str] = match adapter {
        "opencode" => &[
            "run",
            "attach",
            "--format",
            "--dir",
            "--title",
            "--attach",
            "--session",
        ],
        "codex" => &[
            "exec",
            "resume",
            "--json",
            "--output-schema",
            "-o",
            "--output-last-message",
        ],
        "claude" => &[
            "-p",
            "--print",
            "--output-format",
            "--resume",
            "-r",
            "--continue",
            "-c",
        ],
        "pi" => &["-p", "--print", "--mode", "--session", "-c", "-r"],
        _ => &[],
    };
    if let Some(argument) = arguments.iter().find(|argument| {
        reserved.iter().any(|reserved| {
            argument.as_str() == *reserved || argument.starts_with(&format!("{reserved}="))
        })
    }) {
        return Err(format!(
            "harness '{id}' argument '{argument}' is protocol-critical for the {adapter} adapter"
        ));
    }
    if arguments
        .iter()
        .any(|argument| argument.contains("{prompt") || argument.contains("{session"))
    {
        return Err(format!(
            "harness '{id}' built-in arguments cannot contain prompt or session placeholders"
        ));
    }
    Ok(())
}

fn valid_environment_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn validate_transport(
    id: &str,
    operation: &str,
    command: &[String],
    transport: Option<PromptTransport>,
    stdin_allowed: bool,
) -> Result<(), String> {
    let prompt_count = command
        .iter()
        .filter(|arg| arg.as_str() == "{prompt}")
        .count();
    let file_count = command
        .iter()
        .filter(|arg| arg.as_str() == "{prompt_file}")
        .count();
    let valid = match transport {
        None => prompt_count == 0 && file_count == 0,
        Some(PromptTransport::Argument) => prompt_count == 1 && file_count == 0,
        Some(PromptTransport::TempFile) => prompt_count == 0 && file_count == 1,
        Some(PromptTransport::Stdin) => stdin_allowed && prompt_count == 0 && file_count == 0,
    };
    if valid {
        Ok(())
    } else {
        Err(format!(
            "harness '{id}' has an invalid {operation} prompt transport or placeholder"
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessDescription {
    pub id: String,
    pub adapter: String,
    pub interactive: bool,
    pub initial_prompt: bool,
    pub headless: bool,
    pub structured_events: bool,
    pub persistent_sessions: bool,
    pub interactive_resume: bool,
    pub observe: bool,
    pub submit: bool,
    pub cancel_session: bool,
    pub supported_version: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecutionRef {
    pub state: Option<String>,
    pub process_id: Option<u32>,
    pub process_start_time_ticks: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionRef {
    pub adapter_id: Option<String>,
    pub endpoint: Option<String>,
    pub id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTodo {
    pub title: String,
    pub status: String,
}

impl AgentTodo {
    pub fn new(title: impl Into<String>, status: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            status: status.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEvent {
    SessionIdentified {
        session_id: String,
        title: Option<String>,
    },
    StateChanged {
        state: String,
    },
    AssistantText {
        text: String,
    },
    ToolStarted {
        id: Option<String>,
        name: String,
        args_summary: Option<String>,
    },
    ToolOutput {
        id: Option<String>,
        text: String,
    },
    ToolFinished {
        id: Option<String>,
        status: String,
    },
    TodoUpdated {
        todos: Vec<AgentTodo>,
    },
    DiffUpdated {
        summary: String,
        patch: Option<String>,
    },
    Error {
        message: String,
    },
    Raw {
        event_type: String,
        json: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub argv: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub stdin: Option<String>,
    pub prompt_file: Option<PathBuf>,
    pub structured_events: bool,
    pub attach: bool,
}

impl Invocation {
    pub fn command(&self, cwd: &Path) -> Result<Command, String> {
        let (program, args) = self
            .argv
            .split_first()
            .ok_or_else(|| "harness invocation is empty".to_string())?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .envs(&self.environment)
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        Ok(command)
    }

    pub fn cleanup(&self) {
        if let Some(path) = &self.prompt_file {
            let _ = std::fs::remove_file(path);
        }
    }

    pub fn spawn(&self, cwd: &Path) -> Result<std::process::Child, String> {
        let mut child = self
            .command(cwd)?
            .spawn()
            .map_err(|error| format!("start harness '{}': {error}", self.argv[0]))?;
        if let Some(input) = self.stdin.as_deref() {
            let result = child
                .stdin
                .take()
                .ok_or_else(|| "open harness stdin".to_string())
                .and_then(|mut stdin| {
                    stdin
                        .write_all(input.as_bytes())
                        .map_err(|error| format!("write harness prompt to stdin: {error}"))
                });
            if let Err(error) = result {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
        Ok(child)
    }
}

pub const MAX_OUTPUT_LINE_BYTES: usize = 1024 * 1024;

pub fn read_bounded_lines(
    reader: impl Read,
    mut emit: impl FnMut(String) -> bool,
) -> Result<(), String> {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    let mut truncated = false;
    loop {
        let available = reader
            .fill_buf()
            .map_err(|error| format!("read harness output: {error}"))?;
        if available.is_empty() {
            if !line.is_empty() || truncated {
                let mut text = String::from_utf8_lossy(&line).into_owned();
                if truncated {
                    text.push_str(" [line truncated]");
                }
                let _ = emit(text);
            }
            return Ok(());
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let chunk = &available[..consumed];
        let content = chunk.strip_suffix(b"\n").unwrap_or(chunk);
        if line.len() < MAX_OUTPUT_LINE_BYTES {
            let remaining = MAX_OUTPUT_LINE_BYTES - line.len();
            line.extend_from_slice(&content[..content.len().min(remaining)]);
            truncated |= content.len() > remaining;
        } else if !content.is_empty() {
            truncated = true;
        }
        let complete = chunk.ends_with(b"\n");
        reader.consume(consumed);
        if complete {
            if line.ends_with(b"\r") {
                line.pop();
            }
            let mut text = String::from_utf8_lossy(&line).into_owned();
            if truncated {
                text.push_str(" [line truncated]");
            }
            if !emit(text) {
                return Ok(());
            }
            line.clear();
            truncated = false;
        }
    }
}

pub struct Harness<'a> {
    id: String,
    config: &'a HarnessConfig,
}

impl<'a> Harness<'a> {
    pub fn new(id: &str, config: &'a HarnessConfig) -> Self {
        Self {
            id: id.to_string(),
            config,
        }
    }

    pub fn describe(&self) -> HarnessDescription {
        let adapter = self.config.adapter.as_str();
        let structured = matches!(adapter, "opencode" | "codex" | "claude" | "pi");
        let sessions = matches!(adapter, "opencode" | "codex" | "claude" | "pi");
        HarnessDescription {
            id: self.id.clone(),
            adapter: self.config.adapter.clone(),
            interactive: true,
            initial_prompt: adapter != "generic"
                || self.config.interactive_prompt_transport.is_some(),
            headless: adapter != "generic" || self.config.headless_command.is_some(),
            structured_events: structured,
            persistent_sessions: sessions,
            interactive_resume: sessions,
            observe: adapter == "opencode",
            submit: adapter == "opencode",
            cancel_session: adapter == "opencode",
            supported_version: match adapter {
                "codex" => "0.145.0+",
                "claude" => "2.1.214+",
                "pi" => "0.81.1+",
                "opencode" => "current stable",
                _ => "user-defined",
            },
        }
    }

    pub fn interactive_argv(
        &self,
        prompt: Option<&str>,
        server_url: Option<&str>,
        session_id: Option<&str>,
        cwd: &Path,
    ) -> Result<Invocation, String> {
        if self.config.adapter == "opencode" {
            if prompt.is_some() {
                return Err(
                    "OpenCode initial prompts are submitted through its session API".to_string(),
                );
            }
            let mut argv = self.builtin_prefix();
            if let Some(server_url) = server_url {
                argv.extend(["attach".to_string(), server_url.to_string()]);
                argv.extend(["--dir".to_string(), cwd.display().to_string()]);
                if let Some(session_id) = session_id {
                    argv.extend(["--session".to_string(), session_id.to_string()]);
                }
            }
            return Ok(Invocation {
                argv,
                environment: self.config.environment.clone(),
                stdin: None,
                prompt_file: None,
                structured_events: false,
                attach: server_url.is_some(),
            });
        }
        if self.config.adapter != "generic" {
            let mut argv = self.builtin_prefix();
            if let Some(session_id) = session_id {
                match self.config.adapter.as_str() {
                    "codex" => argv.extend(["resume".to_string(), session_id.to_string()]),
                    "claude" => argv.extend(["--resume".to_string(), session_id.to_string()]),
                    "pi" => argv.extend(["--session".to_string(), session_id.to_string()]),
                    _ => {
                        return Err(format!(
                            "harness '{}' does not support interactive session resume",
                            self.id
                        ));
                    }
                }
            }
            if let Some(prompt) = prompt {
                argv.push(prompt.to_string());
            }
            return Ok(Invocation {
                argv,
                environment: self.config.environment.clone(),
                stdin: None,
                prompt_file: None,
                structured_events: false,
                attach: session_id.is_some(),
            });
        }
        if session_id.is_some() {
            return Err(format!(
                "harness '{}' does not support interactive session resume",
                self.id
            ));
        }
        invocation_from_template(
            &self.config.interactive_command,
            self.config.interactive_prompt_transport,
            prompt,
            &self.config.environment,
        )
    }

    pub fn headless(
        &self,
        prompt: &str,
        cwd: &Path,
        title: &str,
        server_url: Option<&str>,
        variant: Option<&str>,
        attach: bool,
    ) -> Result<Invocation, String> {
        if self.config.adapter == "opencode" {
            let mut argv = self.builtin_prefix();
            argv.push("run".to_string());
            if attach && let Some(server_url) = server_url {
                argv.extend(["--attach".to_string(), server_url.to_string()]);
            }
            if let Some(variant) = variant {
                argv.extend(["--variant".to_string(), variant.to_string()]);
            }
            argv.extend([
                "--format".to_string(),
                "json".to_string(),
                "--dir".to_string(),
                cwd.display().to_string(),
                "--title".to_string(),
                title.to_string(),
                prompt.to_string(),
            ]);
            return Ok(Invocation {
                argv,
                environment: self.config.environment.clone(),
                stdin: None,
                prompt_file: None,
                structured_events: true,
                attach: attach && server_url.is_some(),
            });
        }
        if self.config.adapter != "generic" {
            let mut argv = self.builtin_prefix();
            let structured_events = match self.config.adapter.as_str() {
                "codex" => {
                    argv.extend(["exec".to_string(), "--json".to_string(), prompt.to_string()]);
                    true
                }
                "claude" => {
                    argv.extend([
                        "--print".to_string(),
                        "--output-format".to_string(),
                        "stream-json".to_string(),
                        "--verbose".to_string(),
                        prompt.to_string(),
                    ]);
                    true
                }
                "pi" => {
                    argv.extend([
                        "--mode".to_string(),
                        "json".to_string(),
                        "--print".to_string(),
                        prompt.to_string(),
                    ]);
                    true
                }
                _ => unreachable!("validated built-in adapter"),
            };
            return Ok(Invocation {
                argv,
                environment: self.config.environment.clone(),
                stdin: None,
                prompt_file: None,
                structured_events,
                attach: false,
            });
        }
        let template = self.config.headless_command.as_deref().ok_or_else(|| {
            format!(
                "harness '{}' does not support managed headless execution",
                self.id
            )
        })?;
        invocation_from_template(
            template,
            self.config.headless_prompt_transport,
            Some(prompt),
            &self.config.environment,
        )
    }

    fn builtin_prefix(&self) -> Vec<String> {
        let mut argv = self.config.interactive_command.clone();
        argv.extend(self.config.arguments.clone());
        argv
    }

    pub fn prepare_server(
        &self,
        repo: &crate::repo::Repository,
        config: &crate::config::Config,
        branch: &str,
        worktree: &Path,
    ) -> Result<Option<crate::opencode::OpencodeRuntime>, String> {
        if self.config.adapter != "opencode" {
            return Ok(None);
        }
        let program = self
            .config
            .interactive_command
            .first()
            .ok_or_else(|| format!("harness '{}' has no program", self.id))?;
        crate::opencode::ensure_opencode_server_with_program(
            repo, config, &self.id, branch, worktree, program,
        )
        .map(Some)
    }

    pub fn prepare_session(
        &self,
        repo: &crate::repo::Repository,
        config: &crate::config::Config,
        branch: &str,
        worktree: &Path,
    ) -> Result<Option<crate::opencode::OpencodeRuntime>, String> {
        if self.config.adapter != "opencode" {
            return Ok(None);
        }
        let program = self
            .config
            .interactive_command
            .first()
            .ok_or_else(|| format!("harness '{}' has no program", self.id))?;
        crate::opencode::ensure_opencode_session_with_program(
            repo, config, &self.id, branch, worktree, program,
        )
        .map(Some)
    }
}

pub fn list_sessions(endpoint: &str) -> Result<Vec<crate::opencode::OpencodeSession>, String> {
    crate::opencode::list_sessions(endpoint)
}

pub fn inspect_session(
    endpoint: &str,
    session_id: &str,
) -> Result<crate::opencode::OpencodeStatus, String> {
    crate::opencode::poll_session_status(endpoint, session_id)
}

pub fn submit_session(endpoint: &str, session_id: &str, prompt: &str) -> Result<(), String> {
    crate::opencode::submit_prompt(endpoint, session_id, prompt)
}

pub fn cancel_native_session(session: &SessionRef) -> Result<bool, String> {
    match session.adapter_id.as_deref() {
        Some("opencode") => {
            let endpoint = session
                .endpoint
                .as_deref()
                .ok_or_else(|| "OpenCode session has no endpoint".to_string())?;
            let session_id = session
                .id
                .as_deref()
                .ok_or_else(|| "OpenCode session has no ID".to_string())?;
            crate::opencode::abort_session(endpoint, session_id)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

pub fn process_start_time_ticks(process_id: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{process_id}/stat")).ok()?;
        let fields_after_comm = stat.rsplit_once(") ")?.1;
        fields_after_comm.split_whitespace().nth(19)?.parse().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = process_id;
        None
    }
}

#[cfg(unix)]
pub fn terminate_process(
    process_id: u32,
    _expected_start_time_ticks: Option<u64>,
) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if process_start_time_ticks(process_id) != _expected_start_time_ticks
        || _expected_start_time_ticks.is_none()
    {
        return Err(format!(
            "refusing to terminate harness process {process_id}: process identity changed or was not recorded"
        ));
    }
    let process_group = -(process_id as libc::pid_t);
    let result = unsafe { libc::kill(process_group, libc::SIGTERM) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "terminate harness process {process_id}: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(not(unix))]
pub fn terminate_process(
    process_id: u32,
    _expected_start_time_ticks: Option<u64>,
) -> Result<(), String> {
    Command::new("taskkill")
        .args(["/PID", &process_id.to_string(), "/T", "/F"])
        .status()
        .map_err(|error| format!("terminate harness process {process_id}: {error}"))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(format!("terminate harness process {process_id}: {status}"))
            }
        })
}

fn invocation_from_template(
    template: &[String],
    transport: Option<PromptTransport>,
    prompt: Option<&str>,
    environment: &BTreeMap<String, String>,
) -> Result<Invocation, String> {
    let mut argv = template.to_vec();
    let mut stdin = None;
    let mut prompt_file = None;
    if let Some(prompt) = prompt {
        match transport.ok_or_else(|| "harness does not support an initial prompt".to_string())? {
            PromptTransport::Argument => replace_arg(&mut argv, "{prompt}", prompt)?,
            PromptTransport::Stdin => stdin = Some(prompt.to_string()),
            PromptTransport::TempFile => {
                let path = temporary_prompt_file(prompt)?;
                replace_arg(&mut argv, "{prompt_file}", &path.display().to_string())?;
                prompt_file = Some(path);
            }
        }
    } else {
        match transport {
            Some(PromptTransport::Argument) => argv.retain(|arg| arg != "{prompt}"),
            Some(PromptTransport::TempFile) => argv.retain(|arg| arg != "{prompt_file}"),
            Some(PromptTransport::Stdin) | None => {}
        }
    }
    Ok(Invocation {
        argv,
        environment: environment.clone(),
        stdin,
        prompt_file,
        structured_events: false,
        attach: false,
    })
}

fn replace_arg(argv: &mut [String], placeholder: &str, value: &str) -> Result<(), String> {
    let arg = argv
        .iter_mut()
        .find(|arg| arg.as_str() == placeholder)
        .ok_or_else(|| format!("missing {placeholder} argument"))?;
    *arg = value.to_string();
    Ok(())
}

fn temporary_prompt_file(prompt: &str) -> Result<PathBuf, String> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    for _ in 0..100 {
        let path = std::env::temp_dir().join(format!(
            "prism-harness-prompt-{}-{}.txt",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(prompt.as_bytes())
                    .map_err(|error| format!("write prompt file: {error}"))?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create prompt file: {error}")),
        }
    }
    Err("create unique prompt file".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generic(command: Vec<&str>, transport: PromptTransport) -> HarnessConfig {
        HarnessConfig {
            adapter: "generic".to_string(),
            interactive_command: vec!["agent".to_string()],
            arguments: Vec::new(),
            interactive_prompt_transport: None,
            headless_command: Some(command.into_iter().map(str::to_string).collect()),
            headless_prompt_transport: Some(transport),
            output_format: OutputFormat::Text,
            environment: BTreeMap::new(),
        }
    }

    fn builtin(adapter: &str) -> HarnessConfig {
        HarnessConfig {
            adapter: adapter.to_string(),
            interactive_command: vec![adapter.to_string()],
            arguments: Vec::new(),
            interactive_prompt_transport: None,
            headless_command: None,
            headless_prompt_transport: None,
            output_format: OutputFormat::JsonLines,
            environment: BTreeMap::new(),
        }
    }

    #[test]
    fn argument_transport_preserves_prompt_as_one_argument() {
        let config = generic(vec!["agent", "run", "{prompt}"], PromptTransport::Argument);
        config.validate("test").unwrap();
        let invocation = Harness::new("test", &config)
            .headless(
                "quotes ' and $HOME\nnext",
                Path::new("/tmp"),
                "ignored",
                None,
                None,
                false,
            )
            .unwrap();
        assert_eq!(invocation.argv[2], "quotes ' and $HOME\nnext");
    }

    #[test]
    fn stdin_transport_does_not_modify_arguments() {
        let config = generic(vec!["agent", "run"], PromptTransport::Stdin);
        config.validate("test").unwrap();
        let invocation = Harness::new("test", &config)
            .headless("hello", Path::new("/tmp"), "ignored", None, None, false)
            .unwrap();
        assert_eq!(invocation.argv, ["agent", "run"]);
        assert_eq!(invocation.stdin.as_deref(), Some("hello"));
    }

    #[test]
    fn rejects_partial_or_repeated_placeholders() {
        let config = generic(
            vec!["agent", "--prompt={prompt}"],
            PromptTransport::Argument,
        );
        assert!(config.validate("test").is_err());
        let config = generic(
            vec!["agent", "{prompt}", "{prompt}"],
            PromptTransport::Argument,
        );
        assert!(config.validate("test").is_err());
    }

    #[test]
    fn rejects_environment_names_that_could_change_the_tmux_shell_command() {
        let mut config = generic(vec!["agent", "run"], PromptTransport::Stdin);
        config
            .environment
            .insert("SAFE; touch /tmp/injected".to_string(), "value".to_string());
        assert!(config.validate("test").is_err());
    }

    #[test]
    fn built_in_adapters_own_headless_protocol_arguments() {
        let cases = [
            ("codex", vec!["codex", "exec", "--json", "hello"], true),
            (
                "claude",
                vec![
                    "claude",
                    "--print",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "hello",
                ],
                true,
            ),
            ("pi", vec!["pi", "--mode", "json", "--print", "hello"], true),
        ];
        for (adapter, expected, structured) in cases {
            let config = builtin(adapter);
            config.validate(adapter).unwrap();
            let invocation = Harness::new(adapter, &config)
                .headless("hello", Path::new("/tmp"), "title", None, None, false)
                .unwrap();
            assert_eq!(invocation.argv, expected, "{adapter}");
            assert_eq!(invocation.structured_events, structured, "{adapter}");
        }
    }

    #[test]
    fn built_in_adapters_reject_protocol_critical_overrides() {
        let mut config = builtin("codex");
        config.arguments = vec!["--json".to_string()];
        assert!(config.validate("codex").is_err());
        config.arguments = vec!["--sandbox".to_string(), "workspace-write".to_string()];
        config.validate("codex").unwrap();
    }

    #[test]
    fn resumable_adapters_own_interactive_resume_syntax() {
        for (adapter, expected) in [
            ("codex", vec!["codex", "resume", "session-1"]),
            ("claude", vec!["claude", "--resume", "session-1"]),
            ("pi", vec!["pi", "--session", "session-1"]),
        ] {
            let config = builtin(adapter);
            let invocation = Harness::new(adapter, &config)
                .interactive_argv(None, None, Some("session-1"), Path::new("/tmp"))
                .unwrap();
            assert_eq!(invocation.argv, expected);
            assert!(invocation.attach);
            assert!(Harness::new(adapter, &config).describe().interactive_resume);
        }
    }

    #[test]
    fn output_reader_bounds_individual_lines() {
        let input = format!("{}\nnext\n", "x".repeat(MAX_OUTPUT_LINE_BYTES + 10));
        let mut lines = Vec::new();
        read_bounded_lines(input.as_bytes(), |line| {
            lines.push(line);
            true
        })
        .unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with(" [line truncated]"));
        assert!(lines[0].len() <= MAX_OUTPUT_LINE_BYTES + " [line truncated]".len());
        assert_eq!(lines[1], "next");
    }

    #[test]
    fn temporary_file_transport_writes_and_cleans_up_prompt() {
        let config = generic(
            vec!["agent", "run", "{prompt_file}"],
            PromptTransport::TempFile,
        );
        let invocation = Harness::new("test", &config)
            .headless(
                "line one\n$HOME",
                Path::new("/tmp"),
                "title",
                None,
                None,
                false,
            )
            .unwrap();
        let path = invocation.prompt_file.clone().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "line one\n$HOME");
        assert_eq!(invocation.argv[2], path.display().to_string());
        invocation.cleanup();
        assert!(!path.exists());
    }

    #[test]
    fn interactive_prompt_placeholder_is_omitted_when_opening_without_prompt() {
        for (transport, placeholder) in [
            (PromptTransport::Argument, "{prompt}"),
            (PromptTransport::TempFile, "{prompt_file}"),
        ] {
            let mut config = generic(vec!["agent", "run"], PromptTransport::Stdin);
            config.headless_command = None;
            config.headless_prompt_transport = None;
            config.interactive_command = vec!["agent".to_string(), placeholder.to_string()];
            config.interactive_prompt_transport = Some(transport);
            config.validate("test").unwrap();
            let invocation = Harness::new("test", &config)
                .interactive_argv(None, None, None, Path::new("/tmp"))
                .unwrap();
            assert_eq!(invocation.argv, ["agent"]);
        }
    }

    #[test]
    fn generic_adapter_reports_unsupported_managed_operations() {
        let mut config = generic(vec!["agent", "run"], PromptTransport::Stdin);
        config.headless_command = None;
        config.headless_prompt_transport = None;
        let harness = Harness::new("test", &config);
        assert!(
            harness
                .headless("prompt", Path::new("/tmp"), "title", None, None, false)
                .unwrap_err()
                .contains("does not support managed headless execution")
        );
        assert!(
            harness
                .interactive_argv(None, None, Some("session-1"), Path::new("/tmp"))
                .unwrap_err()
                .contains("does not support interactive session resume")
        );
    }

    #[test]
    #[cfg(unix)]
    fn managed_process_cancellation_sends_sigterm() {
        let invocation = Invocation {
            argv: vec![
                "sh".to_string(),
                "-c".to_string(),
                "exec sleep 30".to_string(),
            ],
            environment: BTreeMap::new(),
            stdin: None,
            prompt_file: None,
            structured_events: false,
            attach: false,
        };
        let mut child = invocation.spawn(Path::new("/tmp")).unwrap();
        terminate_process(child.id(), process_start_time_ticks(child.id())).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "process ignored SIGTERM"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    #[cfg(unix)]
    fn managed_process_cancellation_terminates_wrapper_descendants() {
        let invocation = Invocation {
            argv: vec![
                "sh".to_string(),
                "-c".to_string(),
                "sleep 30 & child=$!; printf '%s\\n' \"$child\"; wait".to_string(),
            ],
            environment: BTreeMap::new(),
            stdin: None,
            prompt_file: None,
            structured_events: false,
            attach: false,
        };
        let mut child = invocation.spawn(Path::new("/tmp")).unwrap();
        let descendant_id = {
            let stdout = child.stdout.as_mut().unwrap();
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).unwrap();
            line.trim().parse::<libc::pid_t>().unwrap()
        };

        terminate_process(child.id(), process_start_time_ticks(child.id())).unwrap();
        child.wait().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let result = unsafe { libc::kill(descendant_id, 0) };
            if result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "wrapper descendant survived process-group cancellation"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn managed_process_cancellation_rejects_reused_process_identity() {
        let invocation = Invocation {
            argv: vec!["sleep".to_string(), "30".to_string()],
            environment: BTreeMap::new(),
            stdin: None,
            prompt_file: None,
            structured_events: false,
            attach: false,
        };
        let mut child = invocation.spawn(Path::new("/tmp")).unwrap();
        let start = process_start_time_ticks(child.id()).unwrap();

        assert!(terminate_process(child.id(), Some(start + 1)).is_err());
        assert!(child.try_wait().unwrap().is_none());
        terminate_process(child.id(), Some(start)).unwrap();
        child.wait().unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn named_adapter_shims_receive_prompt_exactly_once() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "prism-harness-shim-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();

        for adapter in ["codex", "claude", "pi"] {
            let mut config = builtin(adapter);
            config.interactive_command = vec![path.display().to_string()];
            let invocation = Harness::new(adapter, &config)
                .headless(
                    "prompt with spaces $HOME",
                    Path::new("/tmp"),
                    "title",
                    None,
                    None,
                    false,
                )
                .unwrap();
            let output = invocation
                .command(Path::new("/tmp"))
                .unwrap()
                .output()
                .unwrap();
            assert!(output.status.success());
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert_eq!(
                stdout.matches("prompt with spaces $HOME").count(),
                1,
                "{adapter}"
            );
            let interactive = Harness::new(adapter, &config)
                .interactive_argv(
                    Some("interactive prompt $HOME"),
                    None,
                    None,
                    Path::new("/tmp"),
                )
                .unwrap();
            let output = interactive
                .command(Path::new("/tmp"))
                .unwrap()
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert_eq!(
                stdout.matches("interactive prompt $HOME").count(),
                1,
                "{adapter}"
            );
        }
        let _ = std::fs::remove_file(path);
    }
}
