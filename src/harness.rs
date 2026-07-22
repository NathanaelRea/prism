use std::collections::BTreeMap;
use std::io::Write;
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
    pub interactive_prompt_transport: Option<PromptTransport>,
    pub headless_command: Option<Vec<String>>,
    pub headless_prompt_transport: Option<PromptTransport>,
    pub output_format: OutputFormat,
    pub environment: BTreeMap<String, String>,
}

impl HarnessConfig {
    pub fn opencode(program: impl Into<String>) -> Self {
        Self {
            adapter: "opencode".to_string(),
            interactive_command: vec![program.into()],
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
        if self.adapter != "generic" && self.adapter != "opencode" {
            return Err(format!(
                "harness '{id}' uses unsupported adapter '{}'; supported adapters: opencode, generic",
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
        if self.adapter == "opencode" {
            if self.headless_command.is_some()
                || self.headless_prompt_transport.is_some()
                || self.interactive_prompt_transport.is_some()
            {
                return Err(format!(
                    "harness '{id}' uses the opencode adapter; Prism owns its prompt transport and headless protocol arguments"
                ));
            }
            return Ok(());
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
    pub observe: bool,
    pub submit: bool,
    pub cancel_session: bool,
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
        Ok(command)
    }

    pub fn cleanup(&self) {
        if let Some(path) = &self.prompt_file {
            let _ = std::fs::remove_file(path);
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
        let opencode = self.config.adapter == "opencode";
        HarnessDescription {
            id: self.id.clone(),
            adapter: self.config.adapter.clone(),
            interactive: true,
            initial_prompt: opencode || self.config.interactive_prompt_transport.is_some(),
            headless: opencode || self.config.headless_command.is_some(),
            structured_events: opencode,
            persistent_sessions: opencode,
            observe: opencode,
            submit: opencode,
            cancel_session: opencode,
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
            let mut argv = self.config.interactive_command.clone();
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
            let mut argv = self.config.interactive_command.clone();
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
            interactive_prompt_transport: None,
            headless_command: Some(command.into_iter().map(str::to_string).collect()),
            headless_prompt_transport: Some(transport),
            output_format: OutputFormat::Text,
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
}
