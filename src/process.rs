use std::env;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::observability::{self, LogLevel};

pub fn run_capture(command: &mut Command) -> Result<String, String> {
    let include_argv = observability::enabled(LogLevel::Trace);
    let command_display = observability::command_display(command);
    let operation = observability::begin_operation(
        LogLevel::Debug,
        "process",
        "start",
        "starting subprocess",
        Some(observability::command_data_json(
            command,
            include_argv,
            None,
            None,
            None,
        )),
    );
    let started = Instant::now();
    let output = command.stderr(Stdio::piped()).output().map_err(|error| {
        let elapsed_ms = started.elapsed().as_millis() as i64;
        operation.finish(
            LogLevel::Error,
            "process",
            "error",
            format!("subprocess failed to start: {error}"),
            Some(observability::command_data_json(
                command,
                include_argv,
                Some(elapsed_ms),
                None,
                Some(&error.to_string()),
            )),
        );
        format!("{command_display}: {error}")
    })?;
    let elapsed_ms = started.elapsed().as_millis() as i64;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            format!("exited with {}", output.status)
        } else {
            stderr
        };
        operation.finish(
            LogLevel::Error,
            "process",
            "exit",
            format!("subprocess failed: {}", output.status),
            Some(observability::command_data_json(
                command,
                include_argv,
                Some(elapsed_ms),
                Some(&output.status.to_string()),
                Some(&message),
            )),
        );
        return Err(format!("{command_display}: {message}"));
    }
    operation.finish(
        LogLevel::Debug,
        "process",
        "exit",
        "subprocess exited successfully",
        Some(observability::command_data_json(
            command,
            include_argv,
            Some(elapsed_ms),
            Some(&output.status.to_string()),
            None,
        )),
    );
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn run_status(command: &mut Command) -> Result<(), String> {
    let include_argv = observability::enabled(LogLevel::Trace);
    let command_display = observability::command_display(command);
    let operation = observability::begin_operation(
        LogLevel::Debug,
        "process",
        "start",
        "starting subprocess",
        Some(observability::command_data_json(
            command,
            include_argv,
            None,
            None,
            None,
        )),
    );
    let started = Instant::now();
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            let elapsed_ms = started.elapsed().as_millis() as i64;
            operation.finish(
                LogLevel::Error,
                "process",
                "error",
                format!("subprocess failed to start: {error}"),
                Some(observability::command_data_json(
                    command,
                    include_argv,
                    Some(elapsed_ms),
                    None,
                    Some(&error.to_string()),
                )),
            );
            format!("{command_display}: {error}")
        })?;
    let status = output.status;
    let elapsed_ms = started.elapsed().as_millis() as i64;
    if status.success() {
        operation.finish(
            LogLevel::Debug,
            "process",
            "exit",
            "subprocess exited successfully",
            Some(observability::command_data_json(
                command,
                include_argv,
                Some(elapsed_ms),
                Some(&status.to_string()),
                None,
            )),
        );
        Ok(())
    } else {
        let stderr = first_non_empty_line(&String::from_utf8_lossy(&output.stderr));
        let stdout = first_non_empty_line(&String::from_utf8_lossy(&output.stdout));
        let message = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("exited with {status}")
        };
        operation.finish(
            LogLevel::Error,
            "process",
            "exit",
            format!("subprocess failed: {status}"),
            Some(observability::command_data_json(
                command,
                include_argv,
                Some(elapsed_ms),
                Some(&status.to_string()),
                Some(&message),
            )),
        );
        Err(format!("{command_display}: {message}"))
    }
}

pub fn run_status_inherited(command: &mut Command) -> Result<(), String> {
    let include_argv = observability::enabled(LogLevel::Trace);
    let command_display = observability::command_display(command);
    let operation = observability::begin_operation(
        LogLevel::Debug,
        "process",
        "start",
        "starting subprocess",
        Some(observability::command_data_json(
            command,
            include_argv,
            None,
            None,
            None,
        )),
    );
    let started = Instant::now();
    let status = command.status().map_err(|error| {
        let elapsed_ms = started.elapsed().as_millis() as i64;
        operation.finish(
            LogLevel::Error,
            "process",
            "error",
            format!("subprocess failed to start: {error}"),
            Some(observability::command_data_json(
                command,
                include_argv,
                Some(elapsed_ms),
                None,
                Some(&error.to_string()),
            )),
        );
        format!("{command_display}: {error}")
    })?;
    let elapsed_ms = started.elapsed().as_millis() as i64;
    if status.success() {
        operation.finish(
            LogLevel::Debug,
            "process",
            "exit",
            "subprocess exited successfully",
            Some(observability::command_data_json(
                command,
                include_argv,
                Some(elapsed_ms),
                Some(&status.to_string()),
                None,
            )),
        );
        Ok(())
    } else {
        let message = format!("exited with {status}");
        operation.finish(
            LogLevel::Error,
            "process",
            "exit",
            format!("subprocess failed: {status}"),
            Some(observability::command_data_json(
                command,
                include_argv,
                Some(elapsed_ms),
                Some(&status.to_string()),
                Some(&message),
            )),
        );
        Err(format!("{command_display}: {message}"))
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
    if command.contains('/') {
        return Path::new(command).is_file();
    }
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| dir.join(command).is_file())
}

pub fn command_version(command: &str) -> Option<String> {
    let argv = split_command_words(command);
    let program = argv.first()?;
    if !command_exists(program) {
        return None;
    }
    let output = Command::new(program).arg("--version").output().ok()?;
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
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;

    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            ch if ch.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

pub fn run_configured_commands(commands: &[String], cwd: &Path, label: &str) -> Result<(), String> {
    for command in commands {
        let argv = split_command_words(command);
        let Some(program) = argv.first() else {
            continue;
        };
        run_status(Command::new(program).args(&argv[1..]).current_dir(cwd))
            .map_err(|error| format!("{label} check `{command}` failed: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_command_words_handles_quotes() {
        let words = split_command_words(r#"my-agent --mode "two words" 'three words'"#);
        assert_eq!(
            words,
            vec!["my-agent", "--mode", "two words", "three words"]
        );
    }

    #[test]
    fn first_non_empty_line_trims_and_discards_later_lines() {
        assert_eq!(
            first_non_empty_line("\n  first line  \nsecond line"),
            "first line"
        );
    }
}
