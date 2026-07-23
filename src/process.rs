use std::env;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::process::{ExitStatus, Output};
use std::time::{Duration, Instant};

use crate::observability::{self, LogLevel};

pub struct ProcessOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

pub fn run_capture(command: &mut Command) -> Result<String, String> {
    let command_display = observability::command_display(command);
    let output = run_output(command)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "{command_display}: {}",
            process_failure_message(&output)
        ))
    }
}

pub fn run_status(command: &mut Command) -> Result<(), String> {
    let command_display = observability::command_display(command);
    let output = run_output(command)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{command_display}: {}",
            process_failure_message(&output)
        ))
    }
}

pub fn run_output(command: &mut Command) -> Result<ProcessOutput, String> {
    run_output_with_failure_level(command, LogLevel::Error, None)
}

pub fn run_output_allow_failure(command: &mut Command) -> Result<ProcessOutput, String> {
    run_output_with_failure_level(command, LogLevel::Debug, None)
}

pub fn run_output_allow_failure_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> Result<ProcessOutput, String> {
    run_output_with_failure_level(command, LogLevel::Debug, Some(timeout))
}

fn run_output_with_failure_level(
    command: &mut Command,
    failure_level: LogLevel,
    timeout: Option<Duration>,
) -> Result<ProcessOutput, String> {
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
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = match timeout {
        Some(timeout) => output_with_timeout(command, timeout),
        None => command.output(),
    }
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
    let elapsed_ms = started.elapsed().as_millis() as i64;
    let status = output.status;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let process_output = ProcessOutput {
        status,
        stdout,
        stderr,
    };
    let (level, error) = if process_output.status.success() {
        (LogLevel::Debug, None)
    } else {
        (
            failure_level,
            Some(process_failure_message(&process_output)),
        )
    };
    operation.finish(
        level,
        "process",
        "exit",
        if process_output.status.success() {
            "subprocess exited successfully".to_string()
        } else {
            format!("subprocess failed: {}", process_output.status)
        },
        Some(observability::command_data_json(
            command,
            include_argv,
            Some(elapsed_ms),
            Some(&process_output.status.to_string()),
            error.as_deref(),
        )),
    );
    Ok(process_output)
}

fn output_with_timeout(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("stderr unavailable"))?;
    let stdout_reader = std::thread::spawn(move || read_all(stdout));
    let stderr_reader = std::thread::spawn(move || read_all(stderr));
    let started = Instant::now();

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("subprocess timed out after {} ms", timeout.as_millis()),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    Ok(Output {
        status,
        stdout: join_reader(stdout_reader)?,
        stderr: join_reader(stderr_reader)?,
    })
}

fn read_all(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_reader(reader: std::thread::JoinHandle<io::Result<Vec<u8>>>) -> io::Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| io::Error::other("subprocess output reader panicked"))?
}

pub fn run_status_with_stdin(command: &mut Command, stdin: &str) -> Result<(), String> {
    let command_display = observability::command_display(command);
    let include_argv = observability::enabled(LogLevel::Trace);
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
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
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
    let mut child_stdin = child.stdin.take().ok_or_else(|| {
        let elapsed_ms = started.elapsed().as_millis() as i64;
        let error = "stdin unavailable".to_string();
        operation.finish(
            LogLevel::Error,
            "process",
            "error",
            format!("subprocess {error}"),
            Some(observability::command_data_json(
                command,
                include_argv,
                Some(elapsed_ms),
                None,
                Some(&error),
            )),
        );
        format!("{command_display}: {error}")
    })?;
    child_stdin.write_all(stdin.as_bytes()).map_err(|error| {
        let elapsed_ms = started.elapsed().as_millis() as i64;
        let error = error.to_string();
        operation.finish(
            LogLevel::Error,
            "process",
            "error",
            format!("subprocess stdin write failed: {error}"),
            Some(observability::command_data_json(
                command,
                include_argv,
                Some(elapsed_ms),
                None,
                Some(&error),
            )),
        );
        format!("{command_display}: {error}")
    })?;
    drop(child_stdin);
    let output = child.wait_with_output().map_err(|error| {
        let elapsed_ms = started.elapsed().as_millis() as i64;
        let error = error.to_string();
        operation.finish(
            LogLevel::Error,
            "process",
            "error",
            format!("subprocess wait failed: {error}"),
            Some(observability::command_data_json(
                command,
                include_argv,
                Some(elapsed_ms),
                None,
                Some(&error),
            )),
        );
        format!("{command_display}: {error}")
    })?;
    let elapsed_ms = started.elapsed().as_millis() as i64;
    let process_output = ProcessOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    };
    if process_output.status.success() {
        operation.finish(
            LogLevel::Debug,
            "process",
            "exit",
            "subprocess exited successfully",
            Some(observability::command_data_json(
                command,
                include_argv,
                Some(elapsed_ms),
                Some(&process_output.status.to_string()),
                None,
            )),
        );
        Ok(())
    } else {
        let message = process_failure_message(&process_output);
        operation.finish(
            LogLevel::Error,
            "process",
            "exit",
            format!("subprocess failed: {}", process_output.status),
            Some(observability::command_data_json(
                command,
                include_argv,
                Some(elapsed_ms),
                Some(&process_output.status.to_string()),
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

fn process_failure_message(output: &ProcessOutput) -> String {
    let stderr = first_non_empty_line(&output.stderr);
    let stdout = first_non_empty_line(&output.stdout);
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
    let output = run_output_allow_failure(Command::new(program).arg("--version")).ok()?;
    if !output.status.success() {
        return None;
    }
    output
        .stdout
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
                    None => {
                        return Err("command ends with an incomplete escape".to_string());
                    }
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
    fn split_command_words_falls_back_for_incomplete_input() {
        assert_eq!(
            split_command_words("my-agent --mode 'incomplete"),
            ["my-agent", "--mode", "'incomplete"]
        );
    }

    #[test]
    fn parse_command_words_rejects_incomplete_input() {
        assert!(parse_command_words("agent '").is_err());
        assert!(parse_command_words("agent \\").is_err());
        assert!(parse_command_words("   ").is_err());
    }

    #[test]
    fn parse_command_words_preserves_empty_and_single_quoted_arguments() {
        assert_eq!(
            parse_command_words(r#"agent --empty "" '\d+'"#).unwrap(),
            ["agent", "--empty", "", "\\d+"]
        );
        assert_eq!(
            parse_command_words(r#"agent "\d+""#).unwrap(),
            ["agent", "\\d+"]
        );
    }

    #[test]
    fn first_non_empty_line_trims_and_discards_later_lines() {
        assert_eq!(
            first_non_empty_line("\n  first line  \nsecond line"),
            "first line"
        );
    }

    #[test]
    fn output_timeout_terminates_long_running_process() {
        let error = run_output_allow_failure_with_timeout(
            Command::new("sh").args(["-c", "exec sleep 1"]),
            Duration::from_millis(20),
        )
        .err()
        .expect("long-running process should time out");

        assert!(error.contains("subprocess timed out"), "{error}");
    }
}
