//! Async process execution built directly on ProcessKit.

mod capture;
#[cfg(unix)]
mod detached;
mod execution;
mod identity;
mod interactive;
mod live;
mod policy;
mod telemetry;

use std::env;
use std::future::Future;
use std::path::Path;

#[cfg(unix)]
pub(crate) use detached::{VerifiedDetachedProcess, spawn_verified_detached};
#[cfg(all(test, unix))]
pub use execution::run_output;
pub use execution::{
    ProcessCompletion, ProcessInput, ProcessOutput, execute_prefix_bounded, is_cancellation_error,
    run_capture, run_capture_named, run_output_allow_failure, run_output_allow_failure_named,
    run_output_named, run_status, run_status_named, run_status_with_stdin_named,
};
pub use identity::{
    ProcessIdentity, ProcessLifecycleError, ProcessObservation, RecordedProcess,
    TerminationOutcome, observe_process, process_arguments, record_process,
    terminate_recorded_process,
};
pub use interactive::{
    run_status_attached_named, run_status_inherited, run_status_inherited_named,
};
#[cfg(any(test, windows))]
pub use live::spawn_owned;
pub(crate) use live::spawn_streaming_configured;
pub use live::{LiveProcessCompletion, ProcessControl, StreamingProcess, spawn_streaming};
pub use policy::{ProcessDescriptor, ProcessPolicy};
pub use processkit::{CancellationToken, Command, Outcome, Stdin};

// Tokio task-local propagation is used only as a subsystem boundary convenience;
// execution APIs also accept an explicit token.
tokio::task_local! {
    static CURRENT_CANCELLATION: CancellationToken;
}

pub async fn with_cancellation<F>(token: CancellationToken, operation: F) -> F::Output
where
    F: Future,
{
    CURRENT_CANCELLATION.scope(token, operation).await
}

pub fn current_cancellation() -> Option<CancellationToken> {
    CURRENT_CANCELLATION.try_with(Clone::clone).ok()
}

pub(crate) fn process_failure_message(output: &ProcessOutput) -> String {
    let stderr_text = String::from_utf8_lossy(&output.stderr);
    let stdout_text = String::from_utf8_lossy(&output.stdout);
    let stderr = first_non_empty_line(&stderr_text);
    let stdout = first_non_empty_line(&stdout_text);
    if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("exited with {}", output.status)
    }
}

fn first_non_empty_line(output: &str) -> String {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

pub fn command_exists(command: &str) -> bool {
    if command.contains('/') || command.contains('\\') {
        return executable_path_exists(Path::new(command));
    }
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|directory| executable_path_exists(&directory.join(command)))
}

fn executable_path_exists(path: &Path) -> bool {
    if path.is_file() {
        return true;
    }
    #[cfg(windows)]
    if path.extension().is_none() {
        let extensions = env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        return extensions
            .split(';')
            .filter(|extension| !extension.is_empty())
            .any(|extension| {
                path.with_extension(extension.trim_start_matches('.'))
                    .is_file()
            });
    }
    false
}

pub async fn command_version(command: &str) -> Option<String> {
    let argv = split_command_words(command);
    let program = argv.first()?;
    if !command_exists(program) {
        return None;
    }
    let output = run_output_allow_failure(
        Command::new(program).arg("--version"),
        ProcessPolicy::Metadata,
    )
    .await
    .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
}

pub fn split_command_words(command: &str) -> Vec<String> {
    parse_command_words(command).unwrap_or_else(|_| {
        command
            .split_whitespace()
            .map(ToString::to_string)
            .collect()
    })
}

pub fn parse_command_words(command: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut word_started = false;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else if ch == '\\' && active_quote == '"' {
                match chars.peek().copied() {
                    Some(next @ ('\\' | '"' | '$' | '`')) => {
                        chars.next();
                        current.push(next);
                    }
                    Some('\n') => {
                        chars.next();
                    }
                    Some(_) => current.push('\\'),
                    None => return Err("command ends with an incomplete escape".to_string()),
                }
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\\' => {
                word_started = true;
                current.push(
                    chars
                        .next()
                        .ok_or_else(|| "command ends with an incomplete escape".to_string())?,
                );
            }
            '\'' | '"' => {
                word_started = true;
                quote = Some(ch);
            }
            ch if ch.is_whitespace() => {
                if word_started {
                    words.push(std::mem::take(&mut current));
                    word_started = false;
                }
            }
            ch => {
                word_started = true;
                current.push(ch);
            }
        }
    }
    if quote.is_some() {
        return Err("command contains an unterminated quote".to_string());
    }
    if word_started {
        words.push(current);
    }
    if words.is_empty() {
        Err("command cannot be empty".to_string())
    } else {
        Ok(words)
    }
}
