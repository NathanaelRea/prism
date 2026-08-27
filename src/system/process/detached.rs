//! Verified warm-daemon spawning for lifecycle exceptions that must survive Prism.

#[cfg(unix)]
use std::io;

#[cfg(unix)]
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
    #[cfg(unix)]
    child: Option<tokio::process::Child>,
    #[cfg(windows)]
    process_handle: Option<std::os::windows::io::OwnedHandle>,
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

    #[cfg(all(test, unix))]
    pub(crate) fn recorded(&self) -> RecordedProcess {
        self.recorded
    }

    /// Confirm that the persisted leader is still the warm server process.
    #[cfg(unix)]
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
        // handle so surviving group members cannot outlive this startup
        // capability.
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

    #[cfg(windows)]
    pub(crate) fn ensure_leader_running(&mut self) -> Result<(), String> {
        let Some(handle) = self.process_handle.as_ref() else {
            return Err("detached process startup capability was already released".to_string());
        };
        if windows_process_running(handle)? {
            Ok(())
        } else {
            self.process_handle.take();
            Err(format!(
                "detached process {} exited during startup",
                self.recorded.pid
            ))
        }
    }

    #[cfg(unix)]
    pub(crate) async fn shutdown(mut self) -> Result<(), String> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let signal_result = signal_session(self.recorded.pid);
        if signal_result.is_err() {
            let _ = child.start_kill();
        }
        let wait_result = child
            .wait()
            .await
            .map(|_| ())
            .map_err(|error| format!("reap detached process {}: {error}", self.recorded.pid));
        signal_result?;
        wait_result
    }

    #[cfg(windows)]
    pub(crate) async fn shutdown(mut self) -> Result<(), String> {
        use std::os::windows::io::AsRawHandle as _;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Threading::TerminateProcess;

        let Some(handle) = self.process_handle.take() else {
            return Ok(());
        };
        if windows_process_running(&handle)? {
            // SAFETY: this retained handle was returned by CreateProcessW with
            // PROCESS_TERMINATE access and remains valid for this scope.
            unsafe {
                TerminateProcess(HANDLE(handle.as_raw_handle()), 1).map_err(|error| {
                    format!("terminate detached process {}: {error}", self.recorded.pid)
                })?;
            }
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        while windows_process_running(&handle)? {
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "detached process {} did not stop within one second",
                    self.recorded.pid
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(())
    }

    /// Commit the warm daemon after every fallible startup check has passed.
    #[cfg(unix)]
    pub(crate) fn detach(mut self) -> Result<RecordedProcess, String> {
        if let Some(mut child) = self.child.take() {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        Ok(self.recorded)
    }

    #[cfg(windows)]
    pub(crate) fn detach(mut self) -> Result<RecordedProcess, String> {
        self.process_handle.take();
        Ok(self.recorded)
    }
}

#[cfg(unix)]
impl Drop for VerifiedDetachedProcess {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = signal_session(self.recorded.pid);
        }
    }
}

#[cfg(windows)]
impl Drop for VerifiedDetachedProcess {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle as _;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Threading::TerminateProcess;

        if let Some(handle) = self.process_handle.as_ref() {
            // SAFETY: the retained CreateProcessW handle remains valid here.
            let _ = unsafe { TerminateProcess(HANDLE(handle.as_raw_handle()), 1) };
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

#[cfg(unix)]
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

#[cfg(windows)]
async fn spawn_verified_detached_with(
    command: Command,
    recorder: impl FnOnce(u32) -> Result<RecordedProcess, ProcessLifecycleError>,
) -> Result<VerifiedDetachedProcess, String> {
    let display = crate::observability::sanitize_command_text(&command.command_line());
    let (pid, process_handle) = spawn_windows_without_inherited_handles(&command)
        .map_err(|error| format!("{display}: {error}"))?;
    let mut process = VerifiedDetachedProcess {
        recorded: RecordedProcess::from_stored(pid, None),
        process_handle: Some(process_handle),
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

#[cfg(windows)]
fn spawn_windows_without_inherited_handles(
    command: &Command,
) -> Result<(u32, std::os::windows::io::OwnedHandle), String> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::FromRawHandle as _;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION,
        STARTUPINFOW,
    };
    use windows::core::{PCWSTR, PWSTR};

    if command.configured_arg0().is_some() || !command.env_overrides().is_empty() {
        return Err(
            "detached Windows launch does not support argv[0] or environment overrides".to_string(),
        );
    }

    let mut application = command.program().encode_wide().collect::<Vec<_>>();
    if application.contains(&0) {
        return Err("detached Windows program contains a NUL code unit".to_string());
    }
    application.push(0);

    let mut command_line = Vec::new();
    push_windows_quoted_argument(&mut command_line, command.program())?;
    for argument in command.arguments() {
        command_line.push(u16::from(b' '));
        push_windows_quoted_argument(&mut command_line, argument)?;
    }
    command_line.push(0);

    let current_directory = command
        .working_dir()
        .map(|path| {
            let mut value = path.as_os_str().encode_wide().collect::<Vec<_>>();
            if value.contains(&0) {
                return Err(
                    "detached Windows working directory contains a NUL code unit".to_string(),
                );
            }
            value.push(0);
            Ok(value)
        })
        .transpose()?;
    let current_directory = current_directory
        .as_ref()
        .map_or(PCWSTR::null(), |value| PCWSTR(value.as_ptr()));

    let startup = STARTUPINFOW {
        cb: u32::try_from(std::mem::size_of::<STARTUPINFOW>())
            .expect("STARTUPINFOW size fits in u32"),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: every string is NUL-terminated and remains live across the call;
    // the output structure is valid. Handle inheritance is deliberately false
    // so a warm server cannot keep a caller's PowerShell capture pipe open.
    unsafe {
        CreateProcessW(
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
            None,
            current_directory,
            &startup,
            &mut process,
        )
        .map_err(|error| format!("CreateProcessW failed: {error}"))?;
    }
    // SAFETY: CreateProcessW returned two owned handles. The process handle is
    // transferred to OwnedHandle; the initial thread handle is no longer needed.
    let process_handle = unsafe {
        let handle = std::os::windows::io::OwnedHandle::from_raw_handle(process.hProcess.0);
        let _ = CloseHandle(process.hThread);
        handle
    };
    Ok((process.dwProcessId, process_handle))
}

#[cfg(windows)]
fn push_windows_quoted_argument(
    output: &mut Vec<u16>,
    argument: &std::ffi::OsStr,
) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt as _;

    let argument = argument.encode_wide().collect::<Vec<_>>();
    if argument.contains(&0) {
        return Err("detached Windows argument contains a NUL code unit".to_string());
    }

    output.push(u16::from(b'"'));
    let mut backslashes = 0_usize;
    for unit in argument {
        match unit {
            value if value == u16::from(b'\\') => backslashes += 1,
            value if value == u16::from(b'"') => {
                output.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2 + 1));
                output.push(value);
                backslashes = 0;
            }
            value => {
                output.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
                output.push(value);
                backslashes = 0;
            }
        }
    }
    output.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    output.push(u16::from(b'"'));
    Ok(())
}

#[cfg(windows)]
fn windows_process_running(handle: &std::os::windows::io::OwnedHandle) -> Result<bool, String> {
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::WaitForSingleObject;

    // SAFETY: the retained CreateProcessW handle remains valid for this call.
    let status = unsafe { WaitForSingleObject(HANDLE(handle.as_raw_handle()), 0) };
    match status {
        WAIT_OBJECT_0 => Ok(false),
        WAIT_TIMEOUT => Ok(true),
        WAIT_FAILED => Err(format!(
            "wait for detached Windows process: {}",
            std::io::Error::last_os_error()
        )),
        status => Err(format!(
            "wait for detached Windows process returned unexpected status {status:?}"
        )),
    }
}

#[cfg(unix)]
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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use crate::process::{ProcessObservation, observe_process};
    use std::ffi::OsStr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn quoted(argument: &OsStr) -> String {
        let mut output = Vec::new();
        push_windows_quoted_argument(&mut output, argument).expect("quote Windows argument");
        String::from_utf16(&output).expect("quoted argument is valid UTF-16")
    }

    #[test]
    fn windows_argument_quoting_escapes_quotes_and_trailing_backslashes() {
        assert_eq!(quoted(OsStr::new("plain")), "\"plain\"");
        assert_eq!(quoted(OsStr::new("two words")), "\"two words\"");
        assert_eq!(quoted(OsStr::new("say\"hi")), "\"say\\\"hi\"");
        assert_eq!(quoted(OsStr::new("ends\\")), "\"ends\\\\\"");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn windows_identity_capture_failure_terminates_detached_process() {
        let observed = Arc::new(Mutex::new(None));
        let recorder_observation = Arc::clone(&observed);
        let powershell = std::path::PathBuf::from(
            std::env::var_os("SystemRoot").expect("Windows SystemRoot is available"),
        )
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let command = Command::new(powershell).args([
            "-NoLogo",
            "-NoProfile",
            "-Command",
            "Start-Sleep -Seconds 30",
        ]);

        let error = spawn_verified_detached_with(command, move |pid| {
            *recorder_observation
                .lock()
                .expect("lock detached process observation") =
                Some(record_process(pid).expect("record detached Windows process"));
            Err(ProcessLifecycleError::Inspect {
                pid,
                source: std::io::Error::other("injected identity failure"),
            })
        })
        .await
        .expect_err("injected identity capture should fail");

        assert!(error.contains("injected identity failure"));
        let recorded = observed
            .lock()
            .expect("lock detached process result")
            .expect("detached Windows process was recorded");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while observe_process(recorded).expect("observe detached Windows process")
            != ProcessObservation::Missing
        {
            assert!(
                std::time::Instant::now() < deadline,
                "detached Windows process survived failed identity capture"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(all(test, unix))]
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
        process.detach().expect("commit verified detached fixture");
        crate::process::terminate_recorded_process(recorded, Duration::from_millis(100))
            .await
            .expect("terminate verified detached fixture");
    }
}
