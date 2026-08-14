use std::collections::BTreeMap;
use std::io::IsTerminal;

use crate::platform::SupportedOs;

const POSIX_DEFAULT_SHELL: &str = "/bin/sh";
const WINDOWS_DEFAULT_SHELL: &str = "pwsh.exe";

pub fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

pub(crate) fn shell_program_for(os: SupportedOs, value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(match os {
            SupportedOs::Linux | SupportedOs::MacOs => POSIX_DEFAULT_SHELL,
            SupportedOs::Windows => WINDOWS_DEFAULT_SHELL,
        })
        .to_string()
}

pub(crate) fn shell_program(value: Option<&str>) -> String {
    shell_program_for(crate::platform::current_os(), value)
}

pub(crate) fn shell_program_from_env() -> String {
    let configured = std::env::var("PRISM_SHELL").ok();
    #[cfg(unix)]
    let configured = configured.or_else(|| std::env::var("SHELL").ok());
    shell_program(configured.as_deref())
}

pub(crate) fn editor_argv(
    visual: Option<&str>,
    editor: Option<&str>,
    mut command_exists: impl FnMut(&str) -> bool,
) -> Result<Option<Vec<String>>, String> {
    if let Some(value) = visual
        .filter(|value| !value.trim().is_empty())
        .or_else(|| editor.filter(|value| !value.trim().is_empty()))
    {
        return crate::process::parse_command_words(value).map(Some);
    }

    Ok(["nvim", "vim", "vi"]
        .into_iter()
        .find(|candidate| command_exists(candidate))
        .map(|candidate| vec![candidate.to_string()]))
}

pub(crate) fn editor_argv_from_env() -> Result<Option<Vec<String>>, String> {
    editor_argv(
        std::env::var("VISUAL").ok().as_deref(),
        std::env::var("EDITOR").ok().as_deref(),
        crate::process::command_exists,
    )
}

pub(crate) fn posix_shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':' | '='))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Serializes one PowerShell single-quoted literal. PowerShell escapes a single quote by
/// doubling it; interpolation, command substitution, and newlines remain data.
pub(crate) fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub(crate) fn shell_command_for(
    os: SupportedOs,
    argv: &[String],
    environment: &BTreeMap<String, String>,
    cleanup_file: Option<&Path>,
) -> Result<String, String> {
    let (program, arguments) = argv
        .split_first()
        .ok_or_else(|| "shell command has no executable".to_string())?;
    for key in environment.keys() {
        let mut characters = key.chars();
        let valid_start = characters
            .next()
            .is_some_and(|character| character == '_' || character.is_ascii_alphabetic());
        if !valid_start
            || !characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
        {
            return Err(format!("invalid shell environment variable name `{key}`"));
        }
    }
    match os {
        SupportedOs::Linux | SupportedOs::MacOs => {
            let command = argv
                .iter()
                .map(|argument| posix_shell_quote(argument))
                .collect::<Vec<_>>()
                .join(" ");
            let command = if environment.is_empty() {
                command
            } else {
                let assignments = environment
                    .iter()
                    .map(|(key, value)| format!("{key}={}", posix_shell_quote(value)))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("env {assignments} {command}")
            };
            Ok(cleanup_file.map_or(command.clone(), |path| {
                format!(
                    "{command}; prism_status=$?; rm -f {}; exit $prism_status",
                    posix_shell_quote(&path.display().to_string())
                )
            }))
        }
        SupportedOs::Windows => {
            let mut parts = environment
                .iter()
                .map(|(key, value)| format!("$env:{key} = {}", powershell_quote(value)))
                .collect::<Vec<_>>();
            let mut invocation = format!("& {}", powershell_quote(program));
            for argument in arguments {
                invocation.push(' ');
                invocation.push_str(&powershell_quote(argument));
            }
            parts.push(invocation);
            if let Some(path) = cleanup_file {
                parts.push("$prismStatus = $LASTEXITCODE".to_string());
                parts.push(format!(
                    "Remove-Item -LiteralPath {} -Force -ErrorAction SilentlyContinue",
                    powershell_quote(&path.display().to_string())
                ));
                parts.push("exit $prismStatus".to_string());
            }
            Ok(parts.join("; "))
        }
    }
}

use std::path::Path;
