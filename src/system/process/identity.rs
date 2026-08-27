//! Reuse-safe identity and recovery for persisted external processes.

use std::error::Error;
use std::fmt;
use std::io;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessIdentity(u64);

impl ProcessIdentity {
    pub const fn from_stored_value(value: u64) -> Self {
        Self(value)
    }

    pub const fn stored_value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordedProcess {
    pub pid: u32,
    pub identity: Option<ProcessIdentity>,
}

impl RecordedProcess {
    pub fn from_stored(pid: u32, identity: Option<u64>) -> Self {
        Self {
            pid,
            identity: identity.map(ProcessIdentity::from_stored_value),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessObservation {
    RunningSameProcess,
    Missing,
    IdentityReused,
    RunningUnverifiable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminationOutcome {
    Terminated,
    AlreadyExited,
    IdentityReused,
    Unverifiable,
}

#[derive(Debug)]
pub enum ProcessLifecycleError {
    Inspect { pid: u32, source: io::Error },
    Signal { pid: u32, source: io::Error },
    TerminationTimedOut { pid: u32 },
}

impl fmt::Display for ProcessLifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inspect { pid, source } => write!(formatter, "inspect process {pid}: {source}"),
            Self::Signal { pid, source } => {
                write!(formatter, "signal process group {pid}: {source}")
            }
            Self::TerminationTimedOut { pid } => {
                write!(
                    formatter,
                    "process group {pid} survived bounded termination"
                )
            }
        }
    }
}

impl Error for ProcessLifecycleError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Inspect { source, .. } | Self::Signal { source, .. } => Some(source),
            Self::TerminationTimedOut { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeProcessObservation {
    Missing,
    Running(Option<ProcessIdentity>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessRequest {
    Observe,
    Terminate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessDecision {
    Observation(ProcessObservation),
    Terminate,
    TerminationOutcome(TerminationOutcome),
}

fn decide_process_request(
    recorded_identity: Option<ProcessIdentity>,
    native: NativeProcessObservation,
    request: ProcessRequest,
) -> ProcessDecision {
    let observation = match native {
        NativeProcessObservation::Missing => ProcessObservation::Missing,
        NativeProcessObservation::Running(None) => ProcessObservation::RunningUnverifiable,
        NativeProcessObservation::Running(Some(observed)) => match recorded_identity {
            Some(recorded) if recorded == observed => ProcessObservation::RunningSameProcess,
            Some(_) => ProcessObservation::IdentityReused,
            None => ProcessObservation::RunningUnverifiable,
        },
    };
    match (request, observation) {
        (ProcessRequest::Observe, observation) => ProcessDecision::Observation(observation),
        (ProcessRequest::Terminate, ProcessObservation::RunningSameProcess) => {
            ProcessDecision::Terminate
        }
        (ProcessRequest::Terminate, ProcessObservation::Missing) => {
            ProcessDecision::TerminationOutcome(TerminationOutcome::AlreadyExited)
        }
        (ProcessRequest::Terminate, ProcessObservation::IdentityReused) => {
            ProcessDecision::TerminationOutcome(TerminationOutcome::IdentityReused)
        }
        (ProcessRequest::Terminate, ProcessObservation::RunningUnverifiable) => {
            ProcessDecision::TerminationOutcome(TerminationOutcome::Unverifiable)
        }
    }
}

pub fn record_process(pid: u32) -> Result<RecordedProcess, ProcessLifecycleError> {
    let identity = match native_process_observation(pid)? {
        NativeProcessObservation::Missing => None,
        NativeProcessObservation::Running(identity) => identity,
    };
    Ok(RecordedProcess { pid, identity })
}

pub fn observe_process(
    process: RecordedProcess,
) -> Result<ProcessObservation, ProcessLifecycleError> {
    let native = native_process_observation(process.pid)?;
    match decide_process_request(process.identity, native, ProcessRequest::Observe) {
        ProcessDecision::Observation(observation) => Ok(observation),
        _ => unreachable!("observation request always produces an observation"),
    }
}

#[cfg(unix)]
pub async fn terminate_recorded_process(
    process: RecordedProcess,
    grace: Duration,
) -> Result<TerminationOutcome, ProcessLifecycleError> {
    let native = native_process_observation(process.pid)?;
    match decide_process_request(process.identity, native, ProcessRequest::Terminate) {
        ProcessDecision::TerminationOutcome(outcome) => return Ok(outcome),
        ProcessDecision::Terminate => {}
        ProcessDecision::Observation(_) => unreachable!("termination request cannot observe"),
    }

    if !send_process_group_signal(process.pid, libc::SIGTERM).map_err(|source| {
        ProcessLifecycleError::Signal {
            pid: process.pid,
            source,
        }
    })? {
        return Ok(TerminationOutcome::AlreadyExited);
    }
    let term_deadline = Instant::now() + grace;
    while Instant::now() < term_deadline {
        if !probe_process_group(process.pid).map_err(|source| ProcessLifecycleError::Inspect {
            pid: process.pid,
            source,
        })? {
            return Ok(TerminationOutcome::Terminated);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if !send_process_group_signal(process.pid, libc::SIGKILL).map_err(|source| {
        ProcessLifecycleError::Signal {
            pid: process.pid,
            source,
        }
    })? {
        return Ok(TerminationOutcome::Terminated);
    }
    let kill_deadline = Instant::now() + grace;
    while Instant::now() < kill_deadline {
        if !probe_process_group(process.pid).map_err(|source| ProcessLifecycleError::Inspect {
            pid: process.pid,
            source,
        })? {
            return Ok(TerminationOutcome::Terminated);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if probe_process_group(process.pid).map_err(|source| ProcessLifecycleError::Inspect {
        pid: process.pid,
        source,
    })? {
        Err(ProcessLifecycleError::TerminationTimedOut { pid: process.pid })
    } else {
        Ok(TerminationOutcome::Terminated)
    }
}

#[cfg(windows)]
pub async fn terminate_recorded_process(
    process: RecordedProcess,
    grace: Duration,
) -> Result<TerminationOutcome, ProcessLifecycleError> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    let native = native_process_observation(process.pid)?;
    match decide_process_request(process.identity, native, ProcessRequest::Terminate) {
        ProcessDecision::TerminationOutcome(outcome) => return Ok(outcome),
        ProcessDecision::Terminate => {}
        ProcessDecision::Observation(_) => unreachable!("termination request cannot observe"),
    }

    // CTRL+BREAK is valid only for compatible console process groups. Failure is expected for
    // detached and GUI processes; deterministic termination below does not depend on it.
    let _ = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, process.pid) };
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if matches!(observe_process(process)?, ProcessObservation::Missing) {
            return Ok(TerminationOutcome::Terminated);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Do not retain a raw HANDLE across an await: windows::HANDLE is not Send,
    // while recovery futures run inside Send TUI jobs. Terminate synchronously,
    // close the capability immediately, then poll the exact recorded identity.
    let handle =
        unsafe { OpenProcess(PROCESS_TERMINATE, false, process.pid) }.map_err(|source| {
            ProcessLifecycleError::Signal {
                pid: process.pid,
                source: io::Error::other(source),
            }
        })?;
    let terminated =
        unsafe { TerminateProcess(handle, 1) }.map_err(|source| ProcessLifecycleError::Signal {
            pid: process.pid,
            source: io::Error::other(source),
        });
    let _ = unsafe { CloseHandle(handle) };
    terminated?;

    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        match observe_process(process)? {
            ProcessObservation::Missing | ProcessObservation::IdentityReused => {
                return Ok(TerminationOutcome::Terminated);
            }
            ProcessObservation::RunningSameProcess | ProcessObservation::RunningUnverifiable => {}
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(ProcessLifecycleError::TerminationTimedOut { pid: process.pid })
}

pub fn process_arguments(pid: u32) -> Result<Option<Vec<String>>, ProcessLifecycleError> {
    native_process_arguments(pid).map_err(|source| ProcessLifecycleError::Inspect { pid, source })
}

#[cfg(windows)]
pub fn process_executable(pid: u32) -> Result<Option<std::path::PathBuf>, ProcessLifecycleError> {
    process_arguments(pid).map(|arguments| {
        arguments.and_then(|arguments| arguments.into_iter().next().map(std::path::PathBuf::from))
    })
}

fn native_process_observation(pid: u32) -> Result<NativeProcessObservation, ProcessLifecycleError> {
    if !probe_process(pid).map_err(|source| ProcessLifecycleError::Inspect { pid, source })? {
        return Ok(NativeProcessObservation::Missing);
    }
    let identity = native_process_identity(pid)
        .map_err(|source| ProcessLifecycleError::Inspect { pid, source })?;
    if identity.is_none()
        && !probe_process(pid).map_err(|source| ProcessLifecycleError::Inspect { pid, source })?
    {
        return Ok(NativeProcessObservation::Missing);
    }
    Ok(NativeProcessObservation::Running(identity))
}

#[cfg(unix)]
fn probe_result(result: libc::c_int, error: Option<i32>) -> io::Result<bool> {
    if result == 0 {
        return Ok(true);
    }
    match error {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        Some(code) => Err(io::Error::from_raw_os_error(code)),
        None => Err(io::Error::other("process probe failed without an OS error")),
    }
}

#[cfg(unix)]
fn native_pid(pid: u32) -> io::Result<libc::pid_t> {
    let pid = libc::pid_t::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "process id is out of range"))?;
    if pid == 0 {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process id must not be zero",
        ))
    } else {
        Ok(pid)
    }
}

#[cfg(unix)]
fn probe_process(pid: u32) -> io::Result<bool> {
    let result = unsafe { libc::kill(native_pid(pid)?, 0) };
    probe_result(
        result,
        (result != 0)
            .then(|| io::Error::last_os_error().raw_os_error())
            .flatten(),
    )
}

#[cfg(unix)]
fn probe_process_group(pid: u32) -> io::Result<bool> {
    let result = unsafe { libc::kill(-native_pid(pid)?, 0) };
    probe_result(
        result,
        (result != 0)
            .then(|| io::Error::last_os_error().raw_os_error())
            .flatten(),
    )
}

#[cfg(unix)]
pub(crate) fn send_process_group_signal(pid: u32, signal: libc::c_int) -> io::Result<bool> {
    let result = unsafe { libc::kill(-native_pid(pid)?, signal) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(windows)]
fn probe_process(pid: u32) -> io::Result<bool> {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };
    const SYNCHRONIZE_PROCESS: PROCESS_ACCESS_RIGHTS = PROCESS_ACCESS_RIGHTS(0x0010_0000);

    let handle = match unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE_PROCESS,
            false,
            pid,
        )
    } {
        Ok(handle) => handle,
        Err(error) if error.code().0 as u32 == 0x8007_0057 => return Ok(false),
        // Access denied proves that the PID is allocated, but not its identity.
        Err(error) if error.code().0 as u32 == 0x8007_0005 => return Ok(true),
        Err(error) => return Err(io::Error::other(error)),
    };
    let wait = unsafe { WaitForSingleObject(handle, 0) };
    let _ = unsafe { CloseHandle(handle) };
    match wait {
        WAIT_TIMEOUT => Ok(true),
        WAIT_OBJECT_0 => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(target_os = "linux")]
fn native_process_identity(pid: u32) -> io::Result<Option<ProcessIdentity>> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            return Ok(None);
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(parse_linux_process_start_time(&stat).map(ProcessIdentity))
}

#[cfg(target_os = "linux")]
fn parse_linux_process_start_time(stat: &str) -> Option<u64> {
    let fields_after_comm = stat.rsplit_once(") ")?.1;
    fields_after_comm.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(target_os = "macos")]
fn native_process_identity(pid: u32) -> io::Result<Option<ProcessIdentity>> {
    let pid = native_pid(pid)?;
    let mut info = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let result = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size as libc::c_int,
        )
    };
    if result != size as libc::c_int {
        return Ok(None);
    }
    let Some(seconds) = info.pbi_start_tvsec.checked_shl(20) else {
        return Ok(None);
    };
    if info.pbi_start_tvusec >= (1 << 20) {
        return Ok(None);
    }
    Ok(Some(ProcessIdentity(seconds | info.pbi_start_tvusec)))
}

#[cfg(windows)]
fn native_process_identity(pid: u32) -> io::Result<Option<ProcessIdentity>> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => handle,
        Err(error) if error.code().0 as u32 == 0x8007_0057 => return Ok(None),
        Err(error) if error.code().0 as u32 == 0x8007_0005 => return Ok(None),
        Err(error) => return Err(io::Error::other(error)),
    };
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let result =
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }
            .map(|()| {
                ProcessIdentity(
                    (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime),
                )
            })
            .map_err(io::Error::other);
    let _ = unsafe { CloseHandle(handle) };
    result.map(Some)
}

#[cfg(target_os = "linux")]
fn native_process_arguments(pid: u32) -> io::Result<Option<Vec<String>>> {
    let bytes = match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(Some(
        bytes
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .map(|argument| String::from_utf8_lossy(argument).into_owned())
            .collect(),
    ))
}

#[cfg(target_os = "macos")]
fn native_process_arguments(pid: u32) -> io::Result<Option<Vec<String>>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, native_pid(pid)?];
    let mut size = 0;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    let mut bytes = vec![0_u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    bytes.truncate(size);
    parse_macos_process_arguments(&bytes).map(Some)
}

#[cfg(windows)]
fn native_process_arguments(pid: u32) -> io::Result<Option<Vec<String>>> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;

    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => handle,
        Err(error) if error.code().0 as u32 == 0x8007_0057 => return Ok(None),
        // Command-line access is deliberately classified as unavailable when denied.
        Err(error) if error.code().0 as u32 == 0x8007_0005 => return Ok(None),
        Err(error) => return Err(io::Error::other(error)),
    };
    let mut path = vec![0_u16; 32_768];
    let mut len = path.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut len,
        )
    }
    .map(|()| {
        path.truncate(len as usize);
        vec![
            std::ffi::OsString::from_wide(&path)
                .to_string_lossy()
                .into_owned(),
        ]
    })
    .map_err(io::Error::other);
    let _ = unsafe { CloseHandle(handle) };
    result.map(Some)
}

#[cfg(target_os = "macos")]
fn parse_macos_process_arguments(bytes: &[u8]) -> io::Result<Vec<String>> {
    let argc_bytes: [u8; 4] = bytes
        .get(..4)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing process argc"))?;
    let argc = i32::from_ne_bytes(argc_bytes);
    if argc < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "negative process argc",
        ));
    }
    let mut cursor = 4;
    while cursor < bytes.len() && bytes[cursor] != 0 {
        cursor += 1;
    }
    while cursor < bytes.len() && bytes[cursor] == 0 {
        cursor += 1;
    }
    let mut arguments = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        let start = cursor;
        while cursor < bytes.len() && bytes[cursor] != 0 {
            cursor += 1;
        }
        if cursor == bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unterminated process argument",
            ));
        }
        arguments.push(String::from_utf8_lossy(&bytes[start..cursor]).into_owned());
        cursor += 1;
    }
    Ok(arguments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_decision_is_fail_closed_for_reuse_and_unverifiable_pids() {
        let identity = ProcessIdentity(10);
        assert_eq!(
            decide_process_request(
                Some(identity),
                NativeProcessObservation::Running(Some(ProcessIdentity(11))),
                ProcessRequest::Terminate,
            ),
            ProcessDecision::TerminationOutcome(TerminationOutcome::IdentityReused)
        );
        assert_eq!(
            decide_process_request(
                None,
                NativeProcessObservation::Running(Some(identity)),
                ProcessRequest::Terminate,
            ),
            ProcessDecision::TerminationOutcome(TerminationOutcome::Unverifiable)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_stat_parser_handles_spaces_and_parentheses_in_name() {
        let mut fields = vec!["S"; 19];
        fields.push("4242");
        let stat = format!("12 (a process) name) {}", fields.join(" "));
        assert_eq!(parse_linux_process_start_time(&stat), Some(4242));
    }

    #[tokio::test]
    async fn current_process_identity_is_observable_and_reuse_safe() {
        let recorded = record_process(std::process::id()).unwrap();
        assert!(recorded.identity.is_some());
        assert_eq!(
            observe_process(recorded).unwrap(),
            ProcessObservation::RunningSameProcess
        );
        let reused = RecordedProcess {
            pid: recorded.pid,
            identity: recorded
                .identity
                .map(|identity| ProcessIdentity(identity.0.wrapping_add(1))),
        };
        assert_eq!(
            terminate_recorded_process(reused, Duration::from_millis(10))
                .await
                .unwrap(),
            TerminationOutcome::IdentityReused
        );
    }
}
