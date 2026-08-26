//! Verified warm-daemon spawning for the Unix lifecycle exceptions.

use std::io;

use processkit::StdioMode;

use super::{Command, ProcessLifecycleError, RecordedProcess, record_process};

/// A session-isolated child whose startup remains reversible until committed.
///
/// Holding the child without polling it keeps an exited leader waitable while
/// startup is in progress. That prevents its PID/session identifier from being
/// reused before failure cleanup can signal every descendant in the session.
#[derive(Debug)]
pub(crate) struct VerifiedDetachedProcess {
    recorded: RecordedProcess,
    child: Option<tokio::process::Child>,
}

impl VerifiedDetachedProcess {
    pub(crate) fn pid(&self) -> u32 {
        self.recorded.pid
    }

    pub(crate) fn identity(&self) -> Option<u64> {
        self.recorded
            .identity
            .map(super::ProcessIdentity::stored_value)
    }

    #[cfg(test)]
    pub(crate) fn recorded(&self) -> RecordedProcess {
        self.recorded
    }

    /// Confirm that the persisted leader is still the warm server process.
    ///
    /// If it exited after starting descendants, kill the session immediately;
    /// persisting only the dead leader would otherwise lose the authority to
    /// clean those descendants up safely.
    pub(crate) fn ensure_leader_running(&mut self) -> Result<(), String> {
        let Some(child) = self.child.as_mut() else {
            return Err("detached process startup capability was already released".to_string());
        };
        let status = child
            .try_wait()
            .map_err(|error| format!("inspect detached process {}: {error}", self.recorded.pid))?;
        let Some(status) = status else {
            return Ok(());
        };

        let cleanup = signal_session(self.recorded.pid);
        // try_wait reaped the leader. Signal the session before releasing the
        // handle so any surviving process-group identifier cannot outlive this
        // startup capability.
        self.child.take();
        match cleanup {
            Ok(()) => Err(format!(
                "detached process {} exited during startup with {status}",
                self.recorded.pid
            )),
            Err(cleanup) => Err(format!(
                "detached process {} exited during startup with {status}; cleanup failed: {cleanup}",
                self.recorded.pid
            )),
        }
    }

    pub(crate) async fn shutdown(mut self) -> Result<(), String> {
        let signal_result = signal_session(self.recorded.pid);
        let wait_result =
            if let Some(mut child) = self.child.take() {
                if signal_result.is_err() {
                    let _ = child.start_kill();
                }
                child.wait().await.map(|_| ()).map_err(|error| {
                    format!("reap detached process {}: {error}", self.recorded.pid)
                })
            } else {
                Ok(())
            };
        signal_result?;
        wait_result
    }

    /// Commit the warm daemon and reap its leader in the background while this
    /// Prism process remains alive. Runtime shutdown deliberately leaves the
    /// verified daemon running.
    pub(crate) fn detach(mut self) -> RecordedProcess {
        if let Some(mut child) = self.child.take() {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        self.recorded
    }
}

impl Drop for VerifiedDetachedProcess {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = signal_session(self.recorded.pid);
        }
    }
}

/// Spawn a session-isolated child that may outlive Prism, but retain teardown
/// authority until a reusable process identity has been captured and startup
/// has been fully committed by the caller.
///
/// Most Prism children must use ProcessKit containment. Warm OpenCode servers
/// are an intentional exception: they survive Prism exit and are recovered by
/// persisted PID plus reusable identity. ProcessKit's public `DetachedChild`
/// releases the only teardown capability before returning, so this narrow seam
/// keeps the Tokio child until every fallible startup step has succeeded.
pub(crate) async fn spawn_verified_detached(
    command: Command,
) -> Result<VerifiedDetachedProcess, String> {
    spawn_verified_detached_with(command, record_process).await
}

async fn spawn_verified_detached_with(
    command: Command,
    recorder: impl FnOnce(u32) -> Result<RecordedProcess, ProcessLifecycleError>,
) -> Result<VerifiedDetachedProcess, String> {
    let display = crate::observability::sanitize_command_text(&command.command_line());
    let command = command
        .stdout(StdioMode::Null)
        .stderr(StdioMode::Null)
        .setsid();
    let mut command = command
        .to_tokio_command()
        .map_err(|error| format!("{display}: {error}"))?;
    command.kill_on_drop(false);
    let child = command
        .spawn()
        .map_err(|error| format!("{display}: {error}"))?;
    let pid = child
        .id()
        .ok_or_else(|| format!("{display}: spawned process has no process id"))?;
    let mut process = VerifiedDetachedProcess {
        recorded: RecordedProcess::from_stored(pid, None),
        child: Some(child),
    };

    let identity_error = match recorder(pid) {
        Ok(recorded) if recorded.identity.is_some() => {
            process.recorded = recorded;
            return Ok(process);
        }
        Ok(_) => {
            format!("{display}: record process {pid} identity: reusable identity is unavailable")
        }
        Err(error) => format!("{display}: record process {pid} identity: {error}"),
    };
    match process.shutdown().await {
        Ok(()) => Err(identity_error),
        Err(cleanup) => Err(format!("{identity_error}; cleanup failed: {cleanup}")),
    }
}

fn signal_session(pid: u32) -> Result<(), String> {
    let native_pid = i32::try_from(pid)
        .map_err(|_| format!("cannot terminate detached process group for invalid pid {pid}"))?;
    // SAFETY: the child was born through `setsid`, making its PID the process
    // group ID. Before startup commit the retained, unpolled child handle keeps
    // an exited leader waitable; surviving group members retain the group ID.
    let result = unsafe { libc::kill(-native_pid, libc::SIGKILL) };
    if result == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(format!("terminate detached process group {pid}: {error}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{ProcessObservation, observe_process};
    use std::fs;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after the Unix epoch")
                .as_nanos()
        ))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn identity_capture_failure_tears_down_the_detached_session() {
        let temp = unique_temp_dir("prism-verified-detached-failure");
        fs::create_dir_all(&temp).expect("create detached failure fixture directory");
        let descendant_path = temp.join("descendant.pid");
        let observed_descendant = Arc::new(Mutex::new(None));
        let recorder_observation = Arc::clone(&observed_descendant);
        let recorder_path = descendant_path.clone();
        let command = Command::new("sh")
            .arg("-c")
            .arg("sleep 30 & printf '%s\\n' \"$!\" > \"$1\"; wait")
            .arg("verified-detached-fixture")
            .arg(&descendant_path);

        let error = spawn_verified_detached_with(command, move |pid| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !recorder_path.exists() {
                assert!(
                    Instant::now() < deadline,
                    "fixture descendant did not start"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            let descendant_pid = fs::read_to_string(&recorder_path)
                .expect("read detached failure descendant pid")
                .trim()
                .parse::<u32>()
                .expect("parse detached failure descendant pid");
            *recorder_observation
                .lock()
                .expect("lock detached failure observation") =
                Some(record_process(descendant_pid).expect("record detached failure descendant"));
            Err(ProcessLifecycleError::Inspect {
                pid,
                source: io::Error::other("injected identity failure"),
            })
        })
        .await
        .expect_err("injected identity capture should fail");

        assert!(error.contains("injected identity failure"));
        let descendant = observed_descendant
            .lock()
            .expect("lock detached failure result")
            .expect("detached failure descendant was recorded");
        let deadline = Instant::now() + Duration::from_secs(2);
        while observe_process(descendant).expect("observe detached failure descendant")
            != ProcessObservation::Missing
        {
            assert!(
                Instant::now() < deadline,
                "detached descendant survived failed identity capture"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        fs::remove_dir_all(temp).expect("remove detached failure fixture directory");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exited_leader_cleans_up_its_surviving_startup_descendant() {
        let temp = unique_temp_dir("prism-verified-detached-exited-leader");
        fs::create_dir_all(&temp).expect("create exited leader fixture directory");
        let descendant_path = temp.join("descendant.pid");
        let command = Command::new("sh")
            .arg("-c")
            .arg("sleep 30 & printf '%s\\n' \"$!\" > \"$1\"; exit 0")
            .arg("verified-detached-exit-fixture")
            .arg(&descendant_path);
        let mut process = spawn_verified_detached(command)
            .await
            .expect("spawn exited leader fixture");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !descendant_path.exists() {
            assert!(
                Instant::now() < deadline,
                "fixture descendant did not start"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let descendant_pid = fs::read_to_string(&descendant_path)
            .expect("read exited leader descendant pid")
            .trim()
            .parse::<u32>()
            .expect("parse exited leader descendant pid");
        let descendant = record_process(descendant_pid).expect("record exited leader descendant");
        loop {
            match process.ensure_leader_running() {
                Ok(()) => {
                    assert!(Instant::now() < deadline, "fixture leader did not exit");
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(error) => {
                    assert!(error.contains("exited during startup"), "{error}");
                    break;
                }
            }
        }
        while observe_process(descendant).expect("observe exited leader descendant")
            != ProcessObservation::Missing
        {
            assert!(
                Instant::now() < deadline,
                "detached descendant survived leader exit cleanup"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        fs::remove_dir_all(temp).expect("remove exited leader fixture directory");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn verified_detached_process_remains_alive_until_identity_qualified_shutdown() {
        let process = spawn_verified_detached(Command::new("sh").args(["-c", "sleep 30"]))
            .await
            .expect("spawn verified detached fixture");
        let recorded = process.recorded();
        assert!(recorded.identity.is_some());
        assert_eq!(
            observe_process(recorded).expect("observe verified detached fixture"),
            ProcessObservation::RunningSameProcess
        );
        process.detach();
        crate::process::terminate_recorded_process(recorded, Duration::from_millis(100))
            .await
            .expect("terminate verified detached fixture");
    }
}
