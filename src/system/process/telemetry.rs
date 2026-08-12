//! Mapping ProcessKit outcomes onto Prism's exactly-once external-call telemetry.

use std::time::Duration;

use processkit::{Command, Error, ErrorKind, Outcome};

use super::capture::CapturedBytes;
use super::{ProcessCompletion, ProcessDescriptor, ProcessOutput, ProcessPolicy};
use crate::flight_recorder::{self, ExternalCallCategory, ExternalCallOutcome, ExternalCallTrace};
use crate::observability::{self, LogLevel, Operation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LiveTermination {
    Natural,
    Canceled,
    Dropped,
}

pub(crate) struct ProcessTelemetry {
    trace: ExternalCallTrace,
    operation: Operation,
    deadline: Option<Duration>,
    command_data: serde_json::Value,
}

impl ProcessTelemetry {
    pub(crate) fn begin(
        command: &Command,
        policy: ProcessPolicy,
        descriptor: ProcessDescriptor,
        deadline: Duration,
    ) -> Self {
        Self::begin_with_contract(command, policy.label(), descriptor, Some(deadline))
    }

    pub(crate) fn begin_attached(command: &Command, descriptor: ProcessDescriptor) -> Self {
        Self::begin_with_contract(command, "attached", descriptor, None)
    }

    #[cfg(any(test, windows))]
    pub(crate) fn begin_owned(command: &Command, descriptor: ProcessDescriptor) -> Self {
        Self::begin_with_contract(command, "owned", descriptor, None)
    }

    fn begin_with_contract(
        command: &Command,
        policy: &'static str,
        descriptor: ProcessDescriptor,
        deadline: Option<Duration>,
    ) -> Self {
        let include_argv = observability::enabled(LogLevel::Trace);
        let argv = include_argv.then(|| {
            std::iter::once(command.program())
                .chain(
                    command
                        .arguments()
                        .iter()
                        .map(|argument| argument.as_os_str()),
                )
                .map(|argument| observability::sanitize_command_text(&argument.to_string_lossy()))
                .collect::<Vec<_>>()
        });
        let command_data = serde_json::json!({
            "program": command.program().to_string_lossy(),
            "arg_count": command.arguments().len() + 1,
            "argv": argv,
            "cwd": command.working_dir().map(|path| path.display().to_string()),
            "policy": policy,
            "deadline_ms": deadline.map(|deadline| deadline.as_millis().min(i64::MAX as u128) as i64),
        });
        let operation = observability::begin_operation(
            LogLevel::Debug,
            "process",
            "start",
            "starting subprocess",
            Some(command_data.to_string()),
        );
        let trace = ExternalCallTrace::begin(ExternalCallCategory::Process, descriptor.name, {
            let mut fields = vec![flight_recorder::text("policy", policy)];
            if let Some(deadline) = deadline {
                fields.push(flight_recorder::unsigned(
                    "deadline_ms",
                    deadline.as_millis(),
                ));
            }
            fields
        });
        Self {
            trace,
            operation,
            deadline,
            command_data,
        }
    }

    pub(crate) fn finish_output(&mut self, output: &ProcessOutput, failure_level: LogLevel) {
        let outcome = match output.completion {
            ProcessCompletion::DeadlineExceeded => ExternalCallOutcome::TimedOut,
            ProcessCompletion::Exited | ProcessCompletion::Signaled => {
                if output.status.success() {
                    ExternalCallOutcome::Success
                } else {
                    ExternalCallOutcome::Failed
                }
            }
        };
        let mut fields = vec![
            flight_recorder::text("completion", output.completion.label()),
            flight_recorder::text("termination_stage", output.termination_stage.label()),
            flight_recorder::unsigned("stdout_bytes", output.stdout_total_bytes),
            flight_recorder::unsigned("stderr_bytes", output.stderr_total_bytes),
            flight_recorder::boolean("stdout_truncated", output.stdout_truncated),
            flight_recorder::boolean("stderr_truncated", output.stderr_truncated),
        ];
        if let Some(code) = output.status.code() {
            fields.push(flight_recorder::unsigned("exit_code", code));
        }
        if let Some(signal) = output.status.signal() {
            fields.push(flight_recorder::unsigned("signal", signal));
        }
        self.trace.finish(outcome, fields);

        let error = match output.completion {
            ProcessCompletion::DeadlineExceeded => Some(format!(
                "subprocess timed out after {} ms",
                self.deadline.unwrap_or_default().as_millis()
            )),
            _ if !output.status.success() => Some(super::process_failure_message(output)),
            _ => None,
        };
        let mut data = self.command_data.clone();
        if let Some(object) = data.as_object_mut() {
            object.insert(
                "elapsed_ms".into(),
                serde_json::json!(output.elapsed.as_millis()),
            );
            object.insert("child_pid".into(), serde_json::json!(output.child_pid));
            object.insert(
                "status".into(),
                serde_json::json!(output.status.to_string()),
            );
            object.insert(
                "completion".into(),
                serde_json::json!(output.completion.label()),
            );
            object.insert(
                "termination_stage".into(),
                serde_json::json!(output.termination_stage.label()),
            );
            object.insert(
                "stdout_bytes".into(),
                serde_json::json!(output.stdout_total_bytes),
            );
            object.insert(
                "stderr_bytes".into(),
                serde_json::json!(output.stderr_total_bytes),
            );
            object.insert(
                "stdout_truncated".into(),
                serde_json::json!(output.stdout_truncated),
            );
            object.insert(
                "stderr_truncated".into(),
                serde_json::json!(output.stderr_truncated),
            );
            if let Some(error) = error.as_deref() {
                object.insert(
                    "error".into(),
                    serde_json::json!(observability::redact_freeform(error, 500)),
                );
            }
        }
        self.operation.finish(
            if error.is_none() {
                LogLevel::Debug
            } else {
                failure_level
            },
            "process",
            "exit",
            if error.is_none() {
                "subprocess exited successfully".to_string()
            } else {
                format!("subprocess failed: {}", output.completion.label())
            },
            Some(data.to_string()),
        );
    }

    pub(crate) fn finish_live_outcome(
        &mut self,
        outcome: &Outcome,
        elapsed: Duration,
        child_pid: u32,
        stdout: &CapturedBytes,
        stderr: &CapturedBytes,
        termination: LiveTermination,
    ) {
        let (telemetry_outcome, completion, error) = if termination == LiveTermination::Dropped {
            (
                ExternalCallOutcome::Dropped,
                "dropped",
                Some("live process owner dropped".to_string()),
            )
        } else if termination == LiveTermination::Canceled {
            (
                ExternalCallOutcome::Canceled,
                "canceled",
                Some("live process cancellation requested".to_string()),
            )
        } else {
            match outcome {
                Outcome::Exited(0) => (ExternalCallOutcome::Success, "exited", None),
                Outcome::Exited(code) => (
                    ExternalCallOutcome::Failed,
                    "exited",
                    Some(format!("subprocess exited with code {code}")),
                ),
                Outcome::Signalled(signal) => (
                    ExternalCallOutcome::Failed,
                    "signaled",
                    Some(signal.map_or_else(
                        || "subprocess was signaled".to_string(),
                        |signal| format!("subprocess exited on signal {signal}"),
                    )),
                ),
                Outcome::TimedOut | Outcome::InactivityTimedOut => (
                    ExternalCallOutcome::TimedOut,
                    "deadline_exceeded",
                    Some("subprocess timed out".to_string()),
                ),
                other => (
                    ExternalCallOutcome::Failed,
                    other.name(),
                    Some(format!("subprocess completed with {}", other.name())),
                ),
            }
        };
        let mut fields = vec![
            flight_recorder::text("completion", completion),
            flight_recorder::text(
                "termination_stage",
                if matches!(outcome, Outcome::TimedOut | Outcome::InactivityTimedOut)
                    || termination != LiveTermination::Natural
                {
                    "managed"
                } else {
                    "none"
                },
            ),
            flight_recorder::unsigned("child_pid", child_pid),
            flight_recorder::unsigned("stdout_bytes", stdout.total_bytes),
            flight_recorder::unsigned("stderr_bytes", stderr.total_bytes),
            flight_recorder::boolean("stdout_truncated", stdout.truncated),
            flight_recorder::boolean("stderr_truncated", stderr.truncated),
        ];
        if let Outcome::Exited(code) = outcome {
            fields.push(flight_recorder::unsigned("exit_code", *code));
        }
        if let Outcome::Signalled(Some(signal)) = outcome {
            fields.push(flight_recorder::unsigned("signal", *signal));
        }
        self.trace.finish(telemetry_outcome, fields);
        self.finish_live_operation(
            elapsed,
            child_pid,
            completion,
            stdout,
            stderr,
            error.as_deref(),
        );
    }

    pub(crate) fn finish_live_error(
        &mut self,
        error: &Error,
        elapsed: Duration,
        child_pid: u32,
        stdout: &CapturedBytes,
        stderr: &CapturedBytes,
        termination: LiveTermination,
    ) {
        if termination == LiveTermination::Dropped {
            self.trace.finish(
                ExternalCallOutcome::Dropped,
                vec![
                    flight_recorder::text("completion", "dropped"),
                    flight_recorder::text("termination_stage", "managed"),
                    flight_recorder::unsigned("child_pid", child_pid),
                    flight_recorder::unsigned("stdout_bytes", stdout.total_bytes),
                    flight_recorder::unsigned("stderr_bytes", stderr.total_bytes),
                    flight_recorder::boolean("stdout_truncated", stdout.truncated),
                    flight_recorder::boolean("stderr_truncated", stderr.truncated),
                ],
            );
            self.finish_live_operation(
                elapsed,
                child_pid,
                "dropped",
                stdout,
                stderr,
                Some("live process owner dropped"),
            );
        } else if termination == LiveTermination::Canceled {
            self.trace.finish(
                ExternalCallOutcome::Canceled,
                vec![
                    flight_recorder::text("completion", "canceled"),
                    flight_recorder::text("termination_stage", "managed"),
                    flight_recorder::unsigned("child_pid", child_pid),
                    flight_recorder::unsigned("stdout_bytes", stdout.total_bytes),
                    flight_recorder::unsigned("stderr_bytes", stderr.total_bytes),
                    flight_recorder::boolean("stdout_truncated", stdout.truncated),
                    flight_recorder::boolean("stderr_truncated", stderr.truncated),
                ],
            );
            self.finish_live_operation(
                elapsed,
                child_pid,
                "canceled",
                stdout,
                stderr,
                Some("live process cancellation requested"),
            );
        } else {
            self.finish_error(
                error,
                elapsed,
                Some(child_pid),
                stdout.total_bytes,
                stderr.total_bytes,
                stdout.truncated,
                stderr.truncated,
            );
        }
    }

    #[cfg(unix)]
    pub(crate) fn finish_attached_status(
        &mut self,
        elapsed: Duration,
        child_pid: u32,
        status: &std::process::ExitStatus,
        canceled: bool,
    ) {
        let empty = CapturedBytes {
            bytes: Vec::new(),
            total_bytes: 0,
            truncated: false,
            complete: true,
        };
        let (outcome, completion, error) = if canceled {
            (
                ExternalCallOutcome::Canceled,
                "canceled",
                Some("attached subprocess canceled".to_string()),
            )
        } else if status.success() {
            (ExternalCallOutcome::Success, "exited", None)
        } else {
            (
                ExternalCallOutcome::Failed,
                "exited",
                Some(format!("attached subprocess exited with {status}")),
            )
        };
        let mut fields = vec![
            flight_recorder::text("completion", completion),
            flight_recorder::text(
                "termination_stage",
                if canceled { "managed" } else { "none" },
            ),
            flight_recorder::unsigned("child_pid", child_pid),
            flight_recorder::unsigned("stdout_bytes", 0_u64),
            flight_recorder::unsigned("stderr_bytes", 0_u64),
            flight_recorder::boolean("stdout_truncated", false),
            flight_recorder::boolean("stderr_truncated", false),
        ];
        if let Some(code) = status.code() {
            fields.push(flight_recorder::unsigned("exit_code", code));
        }
        self.trace.finish(outcome, fields);
        self.finish_live_operation(
            elapsed,
            child_pid,
            completion,
            &empty,
            &empty,
            error.as_deref(),
        );
    }

    pub(crate) fn finish_supervision_message(
        &mut self,
        elapsed: Duration,
        child_pid: u32,
        error: &str,
    ) {
        self.trace.finish(
            ExternalCallOutcome::Failed,
            vec![
                flight_recorder::text("completion", "supervision_error"),
                flight_recorder::text("termination_stage", "none"),
                flight_recorder::unsigned("child_pid", child_pid),
                flight_recorder::unsigned("stdout_bytes", 0_u64),
                flight_recorder::unsigned("stderr_bytes", 0_u64),
                flight_recorder::boolean("stdout_truncated", false),
                flight_recorder::boolean("stderr_truncated", false),
            ],
        );
        let empty = CapturedBytes {
            bytes: Vec::new(),
            total_bytes: 0,
            truncated: false,
            complete: true,
        };
        self.finish_live_operation(
            elapsed,
            child_pid,
            "supervision_error",
            &empty,
            &empty,
            Some(error),
        );
    }

    #[cfg(unix)]
    pub(crate) fn finish_spawn_message(&mut self, elapsed: Duration, error: &str) {
        self.trace.finish(
            ExternalCallOutcome::SpawnFailed,
            vec![
                flight_recorder::text("completion", "spawn_failed"),
                flight_recorder::text("termination_stage", "none"),
                flight_recorder::unsigned("stdout_bytes", 0_u64),
                flight_recorder::unsigned("stderr_bytes", 0_u64),
                flight_recorder::boolean("stdout_truncated", false),
                flight_recorder::boolean("stderr_truncated", false),
            ],
        );
        let empty = CapturedBytes {
            bytes: Vec::new(),
            total_bytes: 0,
            truncated: false,
            complete: true,
        };
        self.finish_live_operation(elapsed, 0, "spawn_failed", &empty, &empty, Some(error));
    }

    fn finish_live_operation(
        &mut self,
        elapsed: Duration,
        child_pid: u32,
        completion: &str,
        stdout: &CapturedBytes,
        stderr: &CapturedBytes,
        error: Option<&str>,
    ) {
        let mut data = self.command_data.clone();
        if let Some(object) = data.as_object_mut() {
            object.insert("elapsed_ms".into(), serde_json::json!(elapsed.as_millis()));
            if child_pid != 0 {
                object.insert("child_pid".into(), serde_json::json!(child_pid));
            }
            object.insert("completion".into(), serde_json::json!(completion));
            object.insert("stdout_bytes".into(), serde_json::json!(stdout.total_bytes));
            object.insert("stderr_bytes".into(), serde_json::json!(stderr.total_bytes));
            object.insert(
                "stdout_truncated".into(),
                serde_json::json!(stdout.truncated),
            );
            object.insert(
                "stderr_truncated".into(),
                serde_json::json!(stderr.truncated),
            );
            if let Some(error) = error {
                object.insert(
                    "error".into(),
                    serde_json::json!(observability::redact_freeform(error, 500)),
                );
            }
        }
        self.operation.finish(
            if error.is_some() {
                LogLevel::Error
            } else {
                LogLevel::Debug
            },
            "process",
            if error.is_some() { "error" } else { "exit" },
            if error.is_some() {
                format!("subprocess failed: {completion}")
            } else {
                "subprocess exited successfully".to_string()
            },
            Some(data.to_string()),
        );
    }

    pub(crate) fn finish_capture_error(
        &mut self,
        elapsed: Duration,
        child_pid: Option<u32>,
        stdout_bytes: u64,
        stderr_bytes: u64,
        stdout_truncated: bool,
        stderr_truncated: bool,
    ) {
        let message = "subprocess output pipes did not fully drain before completion";
        self.trace.finish(
            ExternalCallOutcome::Failed,
            vec![
                flight_recorder::text("completion", "supervision_error"),
                flight_recorder::text("error_kind", "incomplete_capture"),
                flight_recorder::text("termination_stage", "none"),
                flight_recorder::unsigned("stdout_bytes", stdout_bytes),
                flight_recorder::unsigned("stderr_bytes", stderr_bytes),
                flight_recorder::boolean("stdout_truncated", stdout_truncated),
                flight_recorder::boolean("stderr_truncated", stderr_truncated),
            ],
        );
        let mut data = self.command_data.clone();
        if let Some(object) = data.as_object_mut() {
            object.insert("elapsed_ms".into(), serde_json::json!(elapsed.as_millis()));
            object.insert("outcome".into(), serde_json::json!("supervision_error"));
            object.insert(
                "error_category".into(),
                serde_json::json!("incomplete_capture"),
            );
            object.insert("stdout_bytes".into(), serde_json::json!(stdout_bytes));
            object.insert("stderr_bytes".into(), serde_json::json!(stderr_bytes));
            object.insert(
                "stdout_truncated".into(),
                serde_json::json!(stdout_truncated),
            );
            object.insert(
                "stderr_truncated".into(),
                serde_json::json!(stderr_truncated),
            );
            object.insert("error".into(), serde_json::json!(message));
            if let Some(pid) = child_pid {
                object.insert("child_pid".into(), serde_json::json!(pid));
            }
        }
        self.operation.finish(
            LogLevel::Error,
            "process",
            "error",
            message.to_string(),
            Some(data.to_string()),
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_error(
        &mut self,
        error: &Error,
        elapsed: Duration,
        child_pid: Option<u32>,
        stdout_bytes: u64,
        stderr_bytes: u64,
        stdout_truncated: bool,
        stderr_truncated: bool,
    ) {
        let kind = error.kind();
        let (outcome, completion, termination_stage) = match kind {
            ErrorKind::NotFound | ErrorKind::Spawn | ErrorKind::PermissionDenied
                if child_pid.is_none() =>
            {
                (ExternalCallOutcome::SpawnFailed, "spawn_failed", "none")
            }
            ErrorKind::Cancelled => (
                ExternalCallOutcome::Canceled,
                "canceled",
                if child_pid.is_some() {
                    "managed"
                } else {
                    "none"
                },
            ),
            ErrorKind::Timeout => (
                ExternalCallOutcome::TimedOut,
                "deadline_exceeded",
                if child_pid.is_some() {
                    "managed"
                } else {
                    "none"
                },
            ),
            _ => (ExternalCallOutcome::Failed, "supervision_error", "none"),
        };
        self.trace.finish(
            outcome,
            vec![
                flight_recorder::text("completion", completion),
                flight_recorder::text("error_kind", kind.name()),
                flight_recorder::text("termination_stage", termination_stage),
                flight_recorder::unsigned("stdout_bytes", stdout_bytes),
                flight_recorder::unsigned("stderr_bytes", stderr_bytes),
                flight_recorder::boolean("stdout_truncated", stdout_truncated),
                flight_recorder::boolean("stderr_truncated", stderr_truncated),
            ],
        );
        let mut data = self.command_data.clone();
        if let Some(object) = data.as_object_mut() {
            object.insert("elapsed_ms".into(), serde_json::json!(elapsed.as_millis()));
            object.insert("outcome".into(), serde_json::json!(completion));
            object.insert("error_category".into(), serde_json::json!(kind.name()));
            object.insert("stdout_bytes".into(), serde_json::json!(stdout_bytes));
            object.insert("stderr_bytes".into(), serde_json::json!(stderr_bytes));
            object.insert(
                "stdout_truncated".into(),
                serde_json::json!(stdout_truncated),
            );
            object.insert(
                "stderr_truncated".into(),
                serde_json::json!(stderr_truncated),
            );
            object.insert(
                "error".into(),
                serde_json::json!(observability::redact_freeform(&error.to_string(), 500)),
            );
            if let Some(pid) = child_pid {
                object.insert("child_pid".into(), serde_json::json!(pid));
            }
        }
        self.operation.finish(
            LogLevel::Error,
            "process",
            "error",
            if child_pid.is_none() {
                format!("subprocess failed to start: {error}")
            } else {
                format!("subprocess supervision failed: {error}")
            },
            Some(data.to_string()),
        );
    }
}
