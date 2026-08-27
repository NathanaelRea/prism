//! Attached-terminal execution is intentionally isolated from one-shot execution.
//!
//! Full-screen tools inherit the real terminal. Unix needs a narrow raw Tokio
//! escape hatch to establish and foreground a child process group. Windows can
//! use ProcessKit directly while Prism owns console-interrupt routing.

use std::time::{Duration, Instant};

#[cfg(windows)]
use super::capture::CapturedBytes;
#[cfg(windows)]
use super::telemetry::LiveTermination;
use super::telemetry::ProcessTelemetry;
use super::{Command, ProcessDescriptor, current_cancellation};
use processkit::StdioMode;
#[cfg(any(test, windows))]
use processkit::{ErrorKind, Outcome};

const ATTACHED_TERMINATION_GRACE: Duration = Duration::from_secs(1);

pub async fn run_status_inherited(command: Command) -> Result<(), String> {
    run_status_inherited_named(command, ProcessDescriptor::new("process.interactive")).await
}

pub async fn run_status_inherited_named(
    command: Command,
    descriptor: ProcessDescriptor,
) -> Result<(), String> {
    run_status_attached_named(command, descriptor).await
}

#[cfg(windows)]
pub async fn run_status_attached_named(
    command: Command,
    descriptor: ProcessDescriptor,
) -> Result<(), String> {
    let display = crate::observability::sanitize_command_text(&command.command_line());
    let started = Instant::now();
    let mut telemetry = ProcessTelemetry::begin_attached(&command, descriptor);
    let _interrupt_owner = match crate::system::windows_console::attached_child_owns_interrupt() {
        Ok(owner) => owner,
        Err(error) => {
            telemetry.finish_supervision_message(started.elapsed(), 0, &error.to_string());
            return Err(format!("{display}: prepare attached console: {error}"));
        }
    };
    let mut configured = command
        .no_timeout()
        .inherit_stdin()
        .stdout(StdioMode::Inherit)
        .stderr(StdioMode::Inherit)
        .windows_graceful_ctrl_break()
        .cancel_grace(ATTACHED_TERMINATION_GRACE);
    let cancellation = current_cancellation();
    if let Some(cancellation) = cancellation.as_ref() {
        configured = configured.cancel_on(cancellation.clone());
    }
    let process = match configured.start().await {
        Ok(process) => process,
        Err(error) => {
            telemetry.finish_error(&error, started.elapsed(), None, 0, 0, false, false);
            return Err(format!("{display}: {error}"));
        }
    };
    let pid = process.pid().unwrap_or_default();
    let empty = CapturedBytes {
        bytes: Vec::new(),
        total_bytes: 0,
        truncated: false,
        complete: true,
    };
    let result = process.finish().await;
    let canceled = attached_completion_was_canceled(
        cancellation
            .as_ref()
            .is_some_and(|token| token.is_cancelled()),
        match &result {
            Ok(finished) => Ok(&finished.outcome),
            Err(error) => Err(error.kind()),
        },
    );
    let termination = if canceled {
        LiveTermination::Canceled
    } else {
        LiveTermination::Natural
    };
    match result {
        Ok(finished) => {
            telemetry.finish_live_outcome(
                &finished.outcome,
                started.elapsed(),
                pid,
                &empty,
                &empty,
                termination,
            );
            if canceled {
                Err(format!("{display}: interactive subprocess canceled"))
            } else {
                match finished.outcome {
                    Outcome::Exited(0) => Ok(()),
                    outcome => Err(format!("{display}: completed with {}", outcome.name())),
                }
            }
        }
        Err(error) => {
            telemetry.finish_live_error(
                &error,
                started.elapsed(),
                pid,
                &empty,
                &empty,
                termination,
            );
            if canceled {
                Err(format!("{display}: interactive subprocess canceled"))
            } else {
                Err(format!("{display}: {error}"))
            }
        }
    }
}

#[cfg(any(test, windows))]
fn attached_completion_was_canceled(
    cancellation_requested: bool,
    completion: Result<&Outcome, ErrorKind>,
) -> bool {
    cancellation_requested || matches!(completion, Err(ErrorKind::Cancelled))
}

#[cfg(unix)]
pub async fn run_status_attached_named(
    command: Command,
    descriptor: ProcessDescriptor,
) -> Result<(), String> {
    use std::os::unix::process::CommandExt as _;

    let display = crate::observability::sanitize_command_text(&command.command_line());
    let started = Instant::now();
    let mut telemetry = ProcessTelemetry::begin_attached(&command, descriptor);
    let command = command
        .no_timeout()
        .inherit_stdin()
        .stdout(StdioMode::Inherit)
        .stderr(StdioMode::Inherit);
    let mut command = match command.to_tokio_command() {
        Ok(command) => command,
        Err(error) => {
            telemetry.finish_supervision_message(started.elapsed(), 0, &error.to_string());
            return Err(format!("{display}: prepare attached subprocess: {error}"));
        }
    };
    command.as_std_mut().process_group(0);
    command.kill_on_drop(true);

    let original_group = match foreground_process_group() {
        Ok(group) => group,
        Err(error) => {
            telemetry.finish_supervision_message(started.elapsed(), 0, &error.to_string());
            return Err(format!(
                "{display}: inspect controlling-terminal foreground group: {error}"
            ));
        }
    };
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            telemetry.finish_spawn_message(started.elapsed(), &error.to_string());
            return Err(format!("{display}: spawn attached subprocess: {error}"));
        }
    };
    let process_group = match child.id() {
        Some(pid) => pid as libc::pid_t,
        None => {
            telemetry.finish_supervision_message(
                started.elapsed(),
                0,
                "attached subprocess has no process id",
            );
            return Err(format!("{display}: attached subprocess has no process id"));
        }
    };
    let mut terminal = AttachedUnixGroup::new(process_group, original_group);
    if original_group.is_some() {
        if let Err(error) = set_foreground_process_group(process_group) {
            let _ = terminate_group(&mut child, process_group).await;
            terminal.completed = true;
            telemetry.finish_supervision_message(
                started.elapsed(),
                process_group as u32,
                &error.to_string(),
            );
            return Err(format!(
                "{display}: give attached subprocess the controlling terminal: {error}"
            ));
        }
        terminal.foreground_transferred = true;
        // If the child raced to read before the handoff, SIGTTIN may have
        // stopped it. Foregrounding alone does not resume a stopped group.
        unsafe {
            libc::kill(-process_group, libc::SIGCONT);
        }
    }

    let (cleanup_tx, mut cleanup_rx) = tokio::sync::oneshot::channel::<()>();
    let mut wait_task = tokio::spawn(async move {
        tokio::select! {
            biased;
            status = child.wait() => status.map(|status| (status, false)),
            _ = &mut cleanup_rx => terminate_group(&mut child, process_group)
                .await
                .map(|status| (status, true)),
        }
    });
    let wait_result = if let Some(cancellation) = current_cancellation() {
        tokio::select! {
            biased;
            result = &mut wait_task => result,
            () = cancellation.cancelled() => {
                let _ = cleanup_tx.send(());
                wait_task.await
            }
        }
    } else {
        wait_task.await
    };

    let (status, canceled) = match wait_result {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            telemetry.finish_supervision_message(
                started.elapsed(),
                process_group as u32,
                &error.to_string(),
            );
            return Err(format!("{display}: wait for attached subprocess: {error}"));
        }
        Err(error) => {
            telemetry.finish_supervision_message(
                started.elapsed(),
                process_group as u32,
                &error.to_string(),
            );
            return Err(format!(
                "{display}: attached subprocess task failed: {error}"
            ));
        }
    };

    terminal.restore();
    terminal.completed = true;
    telemetry.finish_attached_status(started.elapsed(), process_group as u32, &status, canceled);
    if canceled {
        return Err(format!("{display}: interactive subprocess canceled"));
    }
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("{display}: exited with {status}"))
}

#[cfg(unix)]
fn foreground_process_group() -> std::io::Result<Option<libc::pid_t>> {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return Ok(None);
    }
    let group = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
    if group == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(Some(group))
    }
}

#[cfg(unix)]
struct AttachedUnixGroup {
    process_group: libc::pid_t,
    original_group: Option<libc::pid_t>,
    foreground_transferred: bool,
    completed: bool,
}

#[cfg(unix)]
impl AttachedUnixGroup {
    fn new(process_group: libc::pid_t, original_group: Option<libc::pid_t>) -> Self {
        Self {
            process_group,
            original_group,
            foreground_transferred: false,
            completed: false,
        }
    }

    fn restore(&mut self) {
        if self.foreground_transferred {
            if let Some(original_group) = self.original_group {
                let _ = set_foreground_process_group(original_group);
            }
            self.foreground_transferred = false;
        }
    }
}

#[cfg(unix)]
impl Drop for AttachedUnixGroup {
    fn drop(&mut self) {
        self.restore();
        if !self.completed {
            // Drop is the cancellation/panic backstop. The async paths perform a
            // graceful stop and reap; unwinding must at least remove the group.
            unsafe {
                libc::kill(-self.process_group, libc::SIGKILL);
            }
        }
    }
}

#[cfg(unix)]
async fn terminate_group(
    child: &mut tokio::process::Child,
    process_group: libc::pid_t,
) -> std::io::Result<std::process::ExitStatus> {
    unsafe {
        libc::kill(-process_group, libc::SIGTERM);
    }
    match tokio::time::timeout(ATTACHED_TERMINATION_GRACE, child.wait()).await {
        Ok(status) => status,
        Err(_) => {
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
            child.wait().await
        }
    }
}

#[cfg(unix)]
fn set_foreground_process_group(process_group: libc::pid_t) -> std::io::Result<()> {
    unsafe {
        let mut blocked = std::mem::zeroed::<libc::sigset_t>();
        let mut previous = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGTTOU);
        let block_result = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous);
        if block_result != 0 {
            return Err(std::io::Error::from_raw_os_error(block_result));
        }
        let result = libc::tcsetpgrp(libc::STDIN_FILENO, process_group);
        let error = (result == -1).then(std::io::Error::last_os_error);
        let restore_result =
            libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        if let Some(error) = error {
            Err(error)
        } else if restore_result != 0 {
            Err(std::io::Error::from_raw_os_error(restore_result))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod classification_tests {
    use super::*;

    #[test]
    fn cancellation_request_wins_over_signal_shaped_finished_result() {
        assert!(attached_completion_was_canceled(
            true,
            Ok(&Outcome::Signalled(Some(15)))
        ));
    }

    #[test]
    fn processkit_cancellation_error_is_classified_as_canceled() {
        assert!(attached_completion_was_canceled(
            false,
            Err(ErrorKind::Cancelled)
        ));
        assert!(!attached_completion_was_canceled(
            false,
            Ok(&Outcome::Exited(0))
        ));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn attached_command_uses_inherited_terminal_and_reports_status() {
        run_status_attached_named(
            Command::new("sh").args(["-c", "exit 0"]),
            ProcessDescriptor::new("test.attached.success"),
        )
        .await
        .unwrap();

        let error = run_status_attached_named(
            Command::new("sh").args(["-c", "exit 7"]),
            ProcessDescriptor::new("test.attached.failure"),
        )
        .await
        .unwrap_err();
        assert!(error.contains("exited with"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn attached_command_emits_one_start_and_one_terminal_telemetry_event() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "prism-attached-telemetry-{}-{}",
            std::process::id(),
            crate::util::timestamp_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("attached-telemetry-fixture");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let _ = crate::observability::take_captured_events();

        run_status_attached_named(
            Command::new(&executable),
            ProcessDescriptor::new("test.attached.telemetry"),
        )
        .await
        .unwrap();
        let marker = executable.display().to_string();
        let events = crate::observability::take_captured_events()
            .into_iter()
            .filter(|event| {
                event.target == "process"
                    && event
                        .data_json
                        .as_deref()
                        .is_some_and(|data| data.contains(&marker))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.action == "start")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.action.as_str(), "exit" | "error"))
                .count(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn attached_command_cancellation_terminates_and_reaps_leader() {
        let cancellation = crate::process::CancellationToken::new();
        let future = crate::process::with_cancellation(
            cancellation.clone(),
            run_status_attached_named(
                Command::new("sh").args(["-c", "exec sleep 30"]),
                ProcessDescriptor::new("test.attached.canceled"),
            ),
        );
        let task = tokio::spawn(future);
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
        let error = task.await.unwrap().unwrap_err();
        assert!(error.contains("canceled"));
    }
}
