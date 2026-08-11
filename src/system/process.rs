#[cfg(windows)]
use process_wrap::std::{ChildWrapper, CommandWrap, CreationFlags, JobObject};
use std::cell::RefCell;
use std::env;
use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::ops::{Deref, DerefMut};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
#[cfg(unix)]
use std::process::Child;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::flight_recorder::{self, ExternalCallCategory, ExternalCallOutcome, ExternalCallTrace};
use crate::observability::{self, LogLevel};

#[cfg(unix)]
type ManagedChild = Child;
#[cfg(windows)]
type ManagedChild = Box<dyn ChildWrapper>;

thread_local! {
    static CURRENT_CANCELLATION: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

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
pub fn terminate_recorded_process(
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
        std::thread::sleep(Duration::from_millis(10));
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
        std::thread::sleep(Duration::from_millis(10));
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
pub fn terminate_recorded_process(
    process: RecordedProcess,
    grace: Duration,
) -> Result<TerminationOutcome, ProcessLifecycleError> {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_TERMINATE, TerminateProcess,
        WaitForSingleObject,
    };
    const SYNCHRONIZE_PROCESS: PROCESS_ACCESS_RIGHTS = PROCESS_ACCESS_RIGHTS(0x0010_0000);

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
        std::thread::sleep(Duration::from_millis(10));
    }

    let handle =
        unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE_PROCESS, false, process.pid) }
            .map_err(|source| ProcessLifecycleError::Signal {
                pid: process.pid,
                source: io::Error::other(source),
            })?;
    let result = (|| {
        unsafe { TerminateProcess(handle, 1) }.map_err(|source| ProcessLifecycleError::Signal {
            pid: process.pid,
            source: io::Error::other(source),
        })?;
        let wait_ms = grace.as_millis().min(u128::from(u32::MAX)) as u32;
        match unsafe { WaitForSingleObject(handle, wait_ms) } {
            WAIT_OBJECT_0 => Ok(TerminationOutcome::Terminated),
            WAIT_TIMEOUT => Err(ProcessLifecycleError::TerminationTimedOut { pid: process.pid }),
            _ => Err(ProcessLifecycleError::Inspect {
                pid: process.pid,
                source: io::Error::last_os_error(),
            }),
        }
    })();
    let _ = unsafe { CloseHandle(handle) };
    result
}

pub fn process_arguments(pid: u32) -> Result<Option<Vec<String>>, ProcessLifecycleError> {
    native_process_arguments(pid).map_err(|source| ProcessLifecycleError::Inspect { pid, source })
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
fn send_process_group_signal(pid: u32, signal: libc::c_int) -> io::Result<bool> {
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
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
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

pub(crate) fn with_cancellation<T>(canceled: Arc<AtomicBool>, operation: impl FnOnce() -> T) -> T {
    struct ResetCancellation(Option<Arc<AtomicBool>>);

    impl Drop for ResetCancellation {
        fn drop(&mut self) {
            CURRENT_CANCELLATION.with(|current| {
                current.replace(self.0.take());
            });
        }
    }

    let previous = CURRENT_CANCELLATION.with(|current| current.replace(Some(canceled)));
    let _reset = ResetCancellation(previous);
    operation()
}

pub(crate) fn current_cancellation() -> Option<Arc<AtomicBool>> {
    CURRENT_CANCELLATION.with(|current| current.borrow().clone())
}

pub struct ProcessOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub stdout_total_bytes: u64,
    pub stdout_truncated: bool,
    pub stderr_total_bytes: u64,
    pub stderr_truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessDescriptor {
    name: &'static str,
}

impl ProcessDescriptor {
    pub const fn new(name: &'static str) -> Self {
        Self { name }
    }

    pub fn for_tmux(command: &Command) -> Self {
        let args = command
            .get_args()
            .filter_map(|argument| argument.to_str())
            .collect::<Vec<_>>();
        Self::new(infer_tmux_name(&args))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessPolicy {
    Metadata,
    LocalMutation,
    NetworkQuery,
    WorkflowStep,
    TmuxPoll,
    TmuxCapture,
    #[cfg(test)]
    Test,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetachedProcessPolicy {
    WorkerDaemon,
}

impl ProcessPolicy {
    fn settings(self) -> PolicySettings {
        match self {
            Self::Metadata => PolicySettings::new(Duration::from_secs(30), 1024 * 1024),
            Self::LocalMutation => {
                PolicySettings::new(Duration::from_secs(10 * 60), 4 * 1024 * 1024)
            }
            Self::NetworkQuery => PolicySettings::new(Duration::from_secs(5 * 60), 4 * 1024 * 1024),
            Self::WorkflowStep => {
                PolicySettings::new(Duration::from_secs(6 * 60 * 60), 4 * 1024 * 1024)
            }
            Self::TmuxPoll => PolicySettings::new(Duration::from_secs(15), 1024 * 1024),
            Self::TmuxCapture => PolicySettings::new(Duration::from_secs(4), 4 * 1024 * 1024),
            #[cfg(test)]
            Self::Test => PolicySettings {
                deadline: Duration::from_millis(250),
                termination_grace: Duration::from_millis(100),
                capture_bytes: 1024,
            },
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::LocalMutation => "local_mutation",
            Self::NetworkQuery => "network_query",
            Self::WorkflowStep => "workflow_step",
            Self::TmuxPoll => "tmux_poll",
            Self::TmuxCapture => "tmux_capture",
            #[cfg(test)]
            Self::Test => "test",
        }
    }

    pub fn deadline(self) -> Duration {
        self.settings().deadline
    }
}

fn process_trace(
    descriptor: ProcessDescriptor,
    policy: Option<ProcessPolicy>,
) -> ExternalCallTrace {
    let mut fields = Vec::new();
    if let Some(policy) = policy {
        fields.push(flight_recorder::text("policy", policy.label()));
        fields.push(flight_recorder::unsigned(
            "deadline_ms",
            policy.deadline().as_millis(),
        ));
    }
    ExternalCallTrace::begin(ExternalCallCategory::Process, descriptor.name, fields)
}

fn process_outcome(
    status: Option<ExitStatus>,
    completion: ProcessCompletion,
) -> ExternalCallOutcome {
    match completion {
        ProcessCompletion::DeadlineExceeded => ExternalCallOutcome::TimedOut,
        ProcessCompletion::Canceled => ExternalCallOutcome::Canceled,
        ProcessCompletion::Exited | ProcessCompletion::Signaled => {
            if status.is_some_and(|status| status.success()) {
                ExternalCallOutcome::Success
            } else {
                ExternalCallOutcome::Failed
            }
        }
    }
}

fn process_error_fields(kind: ProcessErrorKind) -> Vec<flight_recorder::Field> {
    vec![
        flight_recorder::text("completion", "supervision_error"),
        flight_recorder::text("error_kind", kind.label()),
        flight_recorder::text("termination_stage", "none"),
    ]
}

fn append_status_fields(fields: &mut Vec<flight_recorder::Field>, status: ExitStatus) {
    if let Some(code) = status.code() {
        fields.push(flight_recorder::unsigned("exit_code", code));
    }
    #[cfg(unix)]
    if let Some(signal) = status.signal() {
        fields.push(flight_recorder::unsigned("signal", signal));
    }
}

fn infer_descriptor(command: &Command) -> ProcessDescriptor {
    let program = Path::new(command.get_program())
        .file_name()
        .and_then(|program| program.to_str())
        .unwrap_or_default();
    let args = command
        .get_args()
        .filter_map(|argument| argument.to_str())
        .collect::<Vec<_>>();
    let name = match program {
        "gh" => match args.as_slice() {
            ["pr", "create", ..] => "gh.pr.create",
            ["pr", "merge", ..] => "gh.pr.merge",
            ["pr", "view", ..] => "gh.pr.view",
            ["pr", "list", ..] => "gh.pr.list",
            ["api", "graphql", ..] => "gh.api.graphql",
            ["run", "list", ..] => "gh.run.list",
            ["run", "view", ..] => "gh.run.view",
            ["auth", "status", ..] => "gh.auth.status",
            _ => "process.other",
        },
        "git" => infer_git_name(&args),
        "tmux" => infer_tmux_name(&args),
        "fzf" => "fzf.select",
        "lazygit" => "lazygit.open",
        "sqlite3" => "sqlite.shell",
        "date" => "system.time.format",
        "open" | "xdg-open" => "browser.open",
        _ => "process.other",
    };
    ProcessDescriptor::new(name)
}

fn infer_git_name(args: &[&str]) -> &'static str {
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "-C" | "--git-dir" | "--work-tree" | "-c" => index += 2,
            argument if argument.starts_with('-') => index += 1,
            operation => {
                return match (operation, args.get(index + 1).copied()) {
                    ("fetch", _) => "git.fetch",
                    ("push", _) => "git.push",
                    ("pull", _) => "git.pull",
                    ("ls-remote", _) => "git.ls_remote",
                    ("remote", Some("update")) => "git.remote.update",
                    ("remote", Some("get-url")) => "git.remote.get_url",
                    ("status", _) => "git.status",
                    ("show-ref", _) => "git.show_ref",
                    ("worktree", Some("list")) => "git.worktree.list",
                    ("worktree", Some("add")) => "git.worktree.add",
                    ("worktree", Some("remove")) => "git.worktree.remove",
                    ("worktree", Some("prune")) => "git.worktree.prune",
                    ("switch", _) => "git.switch",
                    ("rev-list", _) => "git.rev_list",
                    ("rev-parse", _) => "git.rev_parse",
                    ("add", _) => "git.add",
                    ("commit", _) => "git.commit",
                    ("branch", _) => "git.branch",
                    ("merge-tree", _) => "git.merge_tree",
                    ("merge", _) => "git.merge",
                    _ => "process.other",
                };
            }
        }
    }
    "process.other"
}

fn infer_tmux_name(args: &[&str]) -> &'static str {
    args.iter()
        .find_map(|argument| match *argument {
            "load-buffer" => Some("tmux.buffer.load"),
            "paste-buffer" => Some("tmux.buffer.paste"),
            "list-sessions" => Some("tmux.session.list"),
            "has-session" => Some("tmux.session.exists"),
            "new-session" => Some("tmux.session.create"),
            "attach-session" => Some("tmux.session.attach"),
            "kill-session" => Some("tmux.session.kill"),
            "set-option" => Some("tmux.option.set"),
            "list-windows" => Some("tmux.window.list"),
            "new-window" => Some("tmux.window.create"),
            "move-window" => Some("tmux.window.move"),
            "rename-window" => Some("tmux.window.rename"),
            "resize-window" => Some("tmux.window.resize"),
            "capture-pane" => Some("tmux.pane.capture"),
            "display-message" if args.contains(&"#{pane_start_command}") => {
                Some("tmux.pane.start_command")
            }
            "display-message" => Some("tmux.pane.current_command"),
            "send-keys" => Some("tmux.pane.start_command"),
            _ => None,
        })
        .unwrap_or("process.other")
}

#[derive(Clone, Copy)]
struct PolicySettings {
    deadline: Duration,
    termination_grace: Duration,
    capture_bytes: usize,
}

fn child_stdin(child: &mut ManagedChild) -> &mut Option<std::process::ChildStdin> {
    #[cfg(unix)]
    return &mut child.stdin;
    #[cfg(windows)]
    return child.stdin();
}

fn child_stdout(child: &mut ManagedChild) -> &mut Option<std::process::ChildStdout> {
    #[cfg(unix)]
    return &mut child.stdout;
    #[cfg(windows)]
    return child.stdout();
}

fn child_stderr(child: &mut ManagedChild) -> &mut Option<std::process::ChildStderr> {
    #[cfg(unix)]
    return &mut child.stderr;
    #[cfg(windows)]
    return child.stderr();
}

fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    let _ = command;
}

pub struct SupervisedChild {
    child: ManagedChild,
    stdin_writer: Option<JoinHandle<io::Result<()>>>,
    started: Instant,
    deadline: Option<Duration>,
    termination_grace: Duration,
    terminate_on_drop: bool,
    trace: Option<ExternalCallTrace>,
    observed_status: Option<ExitStatus>,
    pending_completion: Option<(ProcessCompletion, TerminationStage)>,
    stdout_bytes: Arc<AtomicU64>,
    stderr_bytes: Arc<AtomicU64>,
    stdout_counted: bool,
    stderr_counted: bool,
}

pub struct CountingReader<R> {
    inner: R,
    bytes: Arc<AtomicU64>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.bytes.fetch_add(count as u64, Ordering::Relaxed);
        Ok(count)
    }
}

impl SupervisedChild {
    pub fn spawn(
        command: &mut Command,
        policy: Option<ProcessPolicy>,
        input: Option<Vec<u8>>,
    ) -> Result<Self, ProcessError> {
        let descriptor = infer_descriptor(command);
        Self::spawn_named(command, policy, input, descriptor)
    }

    pub fn spawn_named(
        command: &mut Command,
        policy: Option<ProcessPolicy>,
        input: Option<Vec<u8>>,
        descriptor: ProcessDescriptor,
    ) -> Result<Self, ProcessError> {
        let mut trace = process_trace(descriptor, policy);
        command.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        configure_process_group(command);

        let started = Instant::now();
        let mut child = match spawn_managed(command) {
            Ok(child) => child,
            Err(error) => {
                trace.finish(
                    ExternalCallOutcome::SpawnFailed,
                    process_error_fields(ProcessErrorKind::Spawn),
                );
                return Err(ProcessError::Spawn(error));
            }
        };
        let stdout_counted = child_stdout(&mut child).is_some();
        let stderr_counted = child_stderr(&mut child).is_some();
        let stdin_writer = if let Some(bytes) = input {
            let Some(mut stdin) = child_stdin(&mut child).take() else {
                let _ = terminate_active_child(&mut child, Duration::from_secs(1));
                trace.finish(
                    ExternalCallOutcome::Failed,
                    process_error_fields(ProcessErrorKind::MissingPipe),
                );
                return Err(ProcessError::MissingPipe("stdin"));
            };
            Some(std::thread::spawn(move || stdin.write_all(&bytes)))
        } else {
            None
        };
        let settings = policy.map(ProcessPolicy::settings);
        Ok(Self {
            child,
            stdin_writer,
            started,
            deadline: settings.map(|settings| settings.deadline),
            termination_grace: settings
                .map(|settings| settings.termination_grace)
                .unwrap_or(Duration::from_secs(1)),
            terminate_on_drop: policy.is_some(),
            trace: Some(trace),
            observed_status: None,
            pending_completion: None,
            stdout_bytes: Arc::new(AtomicU64::new(0)),
            stderr_bytes: Arc::new(AtomicU64::new(0)),
            stdout_counted,
            stderr_counted,
        })
    }

    pub fn deadline(&self) -> Option<Duration> {
        self.deadline
    }

    pub fn deadline_exceeded(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| self.started.elapsed() >= deadline)
    }

    pub fn finish_stdin(&mut self) -> Result<(), ProcessError> {
        join_stdin(self.stdin_writer.take())
    }

    pub fn take_stdout(&mut self) -> Option<CountingReader<std::process::ChildStdout>> {
        child_stdout(&mut self.child)
            .take()
            .map(|inner| CountingReader {
                inner,
                bytes: Arc::clone(&self.stdout_bytes),
            })
    }

    pub fn take_stderr(&mut self) -> Option<CountingReader<std::process::ChildStderr>> {
        child_stderr(&mut self.child)
            .take()
            .map(|inner| CountingReader {
                inner,
                bytes: Arc::clone(&self.stderr_bytes),
            })
    }

    pub fn terminate(&mut self) -> Result<TerminationStage, ProcessError> {
        let termination = terminate_active_child(&mut self.child, self.termination_grace);
        // Closing the process group releases a writer blocked on a full stdin pipe.
        let _ = self.finish_stdin();
        match termination {
            Ok(stage) => {
                let completion = if self.deadline_exceeded() {
                    ProcessCompletion::DeadlineExceeded
                } else {
                    ProcessCompletion::Canceled
                };
                let status = self.child.try_wait().ok().flatten();
                self.observed_status = status;
                self.pending_completion = Some((completion, stage));
                Ok(stage)
            }
            Err(error) => {
                self.finish_trace_error(error.kind());
                Err(error)
            }
        }
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait();
        match status {
            Ok(Some(status)) => {
                self.observed_status = Some(status);
                self.pending_completion =
                    Some((completion_from_status(status), TerminationStage::None));
                Ok(Some(status))
            }
            Ok(None) => Ok(None),
            Err(error) => {
                self.finish_trace_error(ProcessErrorKind::Wait);
                Err(error)
            }
        }
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait();
        match status {
            Ok(status) => {
                self.observed_status = Some(status);
                self.pending_completion =
                    Some((completion_from_status(status), TerminationStage::None));
                Ok(status)
            }
            Err(error) => {
                self.finish_trace_error(ProcessErrorKind::Wait);
                Err(error)
            }
        }
    }

    fn finish_trace(
        &mut self,
        status: Option<ExitStatus>,
        completion: ProcessCompletion,
        termination_stage: TerminationStage,
    ) {
        let Some(trace) = self.trace.as_mut() else {
            return;
        };
        let outcome = process_outcome(status, completion);
        let mut fields = vec![
            flight_recorder::text("completion", completion.label()),
            flight_recorder::text("termination_stage", termination_stage.label()),
        ];
        if let Some(status) = status {
            append_status_fields(&mut fields, status);
        }
        if self.stdout_counted {
            fields.push(flight_recorder::unsigned(
                "stdout_bytes",
                self.stdout_bytes.load(Ordering::Relaxed),
            ));
            fields.push(flight_recorder::boolean("stdout_truncated", false));
        }
        if self.stderr_counted {
            fields.push(flight_recorder::unsigned(
                "stderr_bytes",
                self.stderr_bytes.load(Ordering::Relaxed),
            ));
            fields.push(flight_recorder::boolean("stderr_truncated", false));
        }
        trace.finish(outcome, fields);
    }

    fn finish_trace_error(&mut self, kind: ProcessErrorKind) {
        if let Some(trace) = self.trace.as_mut() {
            trace.finish(ExternalCallOutcome::Failed, process_error_fields(kind));
        }
    }
}

#[cfg(unix)]
impl Deref for SupervisedChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

#[cfg(unix)]
impl DerefMut for SupervisedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

#[cfg(windows)]
impl SupervisedChild {
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        let status = if let Some(status) = self.observed_status {
            Ok(Some(status))
        } else {
            self.child.try_wait()
        };
        let child_running = !matches!(status, Ok(Some(_)));
        if child_running
            && (self.terminate_on_drop
                || self
                    .stdin_writer
                    .as_ref()
                    .is_some_and(|writer| !writer.is_finished()))
        {
            match terminate_active_child(&mut self.child, self.termination_grace) {
                Ok(stage) => self.finish_trace(None, ProcessCompletion::Canceled, stage),
                Err(error) => self.finish_trace_error(error.kind()),
            }
        } else if let Ok(Some(status)) = status {
            let (completion, stage) = self
                .pending_completion
                .unwrap_or((completion_from_status(status), TerminationStage::None));
            self.finish_trace(Some(status), completion, stage);
        } else if status.is_err() {
            self.finish_trace_error(ProcessErrorKind::Wait);
        }
        let _ = self.finish_stdin();
    }
}

impl PolicySettings {
    const fn new(deadline: Duration, capture_bytes: usize) -> Self {
        Self {
            deadline,
            termination_grace: Duration::from_secs(1),
            capture_bytes,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ProcessInput<'a> {
    Null,
    Bytes(&'a [u8]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(windows, allow(dead_code))]
pub enum ProcessCompletion {
    Exited,
    Signaled,
    DeadlineExceeded,
    Canceled,
}

impl ProcessCompletion {
    fn label(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::Signaled => "signaled",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::Canceled => "canceled",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminationStage {
    #[default]
    None,
    Term,
    Kill,
}

impl TerminationStage {
    fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Term => "term",
            Self::Kill => "kill",
        }
    }
}

#[derive(Debug)]
pub struct CapturedTail {
    pub bytes: Vec<u8>,
    pub total_bytes: u64,
    pub truncated: bool,
}

#[derive(Debug)]
pub struct ProcessOutcome {
    pub status: ExitStatus,
    pub completion: ProcessCompletion,
    pub termination_stage: TerminationStage,
    pub stdout: CapturedTail,
    pub stderr: CapturedTail,
    pub elapsed: Duration,
    pub deadline: Duration,
    pub child_pid: u32,
    pub process_group: Option<u32>,
}

#[derive(Debug)]
pub enum ProcessError {
    Spawn(io::Error),
    Signal {
        signal: &'static str,
        source: io::Error,
    },
    Wait(io::Error),
    Reap(io::Error),
    Stdin(io::Error),
    Read {
        stream: &'static str,
        source: io::Error,
    },
    MissingPipe(&'static str),
    ThreadSpawn {
        thread: &'static str,
        source: io::Error,
    },
    ThreadPanicked(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessErrorKind {
    Spawn,
    Signal,
    Wait,
    Reap,
    Stdin,
    Read,
    MissingPipe,
    ThreadSpawn,
    ThreadPanicked,
}

impl ProcessErrorKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Signal => "signal",
            Self::Wait => "wait",
            Self::Reap => "reap",
            Self::Stdin => "stdin",
            Self::Read => "read",
            Self::MissingPipe => "missing_pipe",
            Self::ThreadSpawn => "thread_spawn",
            Self::ThreadPanicked => "thread_panicked",
        }
    }
}

impl ProcessError {
    pub const fn kind(&self) -> ProcessErrorKind {
        match self {
            Self::Spawn(_) => ProcessErrorKind::Spawn,
            Self::Signal { .. } => ProcessErrorKind::Signal,
            Self::Wait(_) => ProcessErrorKind::Wait,
            Self::Reap(_) => ProcessErrorKind::Reap,
            Self::Stdin(_) => ProcessErrorKind::Stdin,
            Self::Read { .. } => ProcessErrorKind::Read,
            Self::MissingPipe(_) => ProcessErrorKind::MissingPipe,
            Self::ThreadSpawn { .. } => ProcessErrorKind::ThreadSpawn,
            Self::ThreadPanicked(_) => ProcessErrorKind::ThreadPanicked,
        }
    }
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(formatter, "{error}"),
            Self::Signal { signal, source } => {
                write!(formatter, "send {signal} to subprocess group: {source}")
            }
            Self::Wait(error) => write!(formatter, "wait for subprocess: {error}"),
            Self::Reap(error) => write!(formatter, "reap subprocess: {error}"),
            Self::Stdin(error) => write!(formatter, "write subprocess stdin: {error}"),
            Self::Read { stream, source } => {
                write!(formatter, "read subprocess {stream}: {source}")
            }
            Self::MissingPipe(stream) => write!(formatter, "subprocess {stream} unavailable"),
            Self::ThreadSpawn { thread, source } => {
                write!(formatter, "start subprocess {thread} thread: {source}")
            }
            Self::ThreadPanicked(thread) => {
                write!(formatter, "subprocess {thread} thread panicked")
            }
        }
    }
}

impl Error for ProcessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(error) | Self::Wait(error) | Self::Reap(error) | Self::Stdin(error) => {
                Some(error)
            }
            Self::Signal { source, .. } => Some(source),
            Self::Read { source, .. } => Some(source),
            Self::ThreadSpawn { source, .. } => Some(source),
            Self::MissingPipe(_) | Self::ThreadPanicked(_) => None,
        }
    }
}

#[allow(dead_code)]
pub fn spawn_detached(
    command: &mut Command,
    policy: DetachedProcessPolicy,
) -> Result<u32, ProcessError> {
    let descriptor = infer_descriptor(command);
    spawn_detached_named(command, policy, descriptor)
}

pub fn spawn_detached_named(
    command: &mut Command,
    policy: DetachedProcessPolicy,
    descriptor: ProcessDescriptor,
) -> Result<u32, ProcessError> {
    let mut trace = ExternalCallTrace::begin(
        ExternalCallCategory::Process,
        descriptor.name,
        vec![flight_recorder::text("policy", "detached")],
    );
    match policy {
        DetachedProcessPolicy::WorkerDaemon => {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(unix)]
            unsafe {
                // Daemons deliberately escape the normal supervised process group.
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            #[cfg(windows)]
            {
                use windows::Win32::System::Threading::{
                    CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
                };
                command.creation_flags((CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS).0);
            }
        }
    }

    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            trace.finish(
                ExternalCallOutcome::SpawnFailed,
                process_error_fields(ProcessErrorKind::Spawn),
            );
            return Err(ProcessError::Spawn(error));
        }
    };
    let child_pid = child.id();
    let child = Arc::new(Mutex::new(Some(child)));
    let reaper_child = Arc::clone(&child);
    if let Err(source) = std::thread::Builder::new()
        .name("prism-detached-reaper".to_string())
        .spawn(move || {
            let Some(mut child) = reaper_child.lock().ok().and_then(|mut child| child.take())
            else {
                return;
            };
            let _ = child.wait();
        })
    {
        if let Some(mut child) = child.lock().ok().and_then(|mut child| child.take()) {
            let _ = child.kill();
            let _ = child.wait();
        }
        trace.finish(
            ExternalCallOutcome::Failed,
            process_error_fields(ProcessErrorKind::ThreadSpawn),
        );
        return Err(ProcessError::ThreadSpawn {
            thread: "detached reaper",
            source,
        });
    }
    trace.finish(
        ExternalCallOutcome::Success,
        vec![flight_recorder::text("completion", "spawned")],
    );
    Ok(child_pid)
}

pub fn run_capture(command: &mut Command, policy: ProcessPolicy) -> Result<String, String> {
    let descriptor = infer_descriptor(command);
    run_capture_named(command, policy, descriptor)
}

pub fn run_capture_named(
    command: &mut Command,
    policy: ProcessPolicy,
    descriptor: ProcessDescriptor,
) -> Result<String, String> {
    let command_display = observability::command_display(command);
    let output = run_output_named(command, policy, descriptor)?;
    if output.status.success() && !output.stdout_truncated {
        Ok(output.stdout)
    } else if output.status.success() {
        Err(format!(
            "{command_display}: stdout was truncated after capturing a bounded tail of {} total bytes",
            output.stdout_total_bytes
        ))
    } else {
        Err(format!(
            "{command_display}: {}",
            process_failure_message(&output)
        ))
    }
}

pub fn run_status(command: &mut Command, policy: ProcessPolicy) -> Result<(), String> {
    let descriptor = infer_descriptor(command);
    run_status_named(command, policy, descriptor)
}

pub fn run_status_named(
    command: &mut Command,
    policy: ProcessPolicy,
    descriptor: ProcessDescriptor,
) -> Result<(), String> {
    let command_display = observability::command_display(command);
    let output = run_output_named(command, policy, descriptor)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{command_display}: {}",
            process_failure_message(&output)
        ))
    }
}

pub fn run_output_named(
    command: &mut Command,
    policy: ProcessPolicy,
    descriptor: ProcessDescriptor,
) -> Result<ProcessOutput, String> {
    run_output_with_failure_level(command, policy, LogLevel::Error, descriptor)
}

pub fn run_output_allow_failure(
    command: &mut Command,
    policy: ProcessPolicy,
) -> Result<ProcessOutput, String> {
    let descriptor = infer_descriptor(command);
    run_output_allow_failure_named(command, policy, descriptor)
}

pub fn run_output_allow_failure_named(
    command: &mut Command,
    policy: ProcessPolicy,
    descriptor: ProcessDescriptor,
) -> Result<ProcessOutput, String> {
    run_output_with_failure_level(command, policy, LogLevel::Debug, descriptor)
}

fn run_output_with_failure_level(
    command: &mut Command,
    policy: ProcessPolicy,
    failure_level: LogLevel,
    descriptor: ProcessDescriptor,
) -> Result<ProcessOutput, String> {
    run_output_with_settings(
        command,
        policy,
        failure_level,
        ProcessInput::Null,
        descriptor,
    )
}

fn run_output_with_settings(
    command: &mut Command,
    policy: ProcessPolicy,
    failure_level: LogLevel,
    input: ProcessInput<'_>,
    descriptor: ProcessDescriptor,
) -> Result<ProcessOutput, String> {
    let settings = policy.settings();
    let mut trace = process_trace(descriptor, Some(policy));
    let include_argv = observability::enabled(LogLevel::Trace);
    let command_display = observability::command_display(command);
    let operation = observability::begin_operation(
        LogLevel::Debug,
        "process",
        "start",
        "starting subprocess",
        Some(observability::process_start_data_json(
            command,
            include_argv,
            policy.label(),
            settings.deadline.as_millis() as i64,
        )),
    );
    let started = Instant::now();
    let canceled = current_cancellation();
    let outcome = supervise(command, policy, input, canceled.as_deref()).map_err(|error| {
        let trace_outcome = if error.kind() == ProcessErrorKind::Spawn {
            ExternalCallOutcome::SpawnFailed
        } else {
            ExternalCallOutcome::Failed
        };
        trace.finish(trace_outcome, process_error_fields(error.kind()));
        let elapsed_ms = started.elapsed().as_millis() as i64;
        operation.finish(
            LogLevel::Error,
            "process",
            "error",
            match error {
                ProcessError::Spawn(_) => format!("subprocess failed to start: {error}"),
                _ => format!("subprocess supervision failed: {error}"),
            },
            Some(observability::process_error_data_json(
                command,
                include_argv,
                policy.label(),
                elapsed_ms,
                settings.deadline.as_millis() as i64,
                error.kind().label(),
                &error.to_string(),
            )),
        );
        format!("{command_display}: {error}")
    })?;
    let elapsed_ms = outcome.elapsed.as_millis() as i64;
    let status = outcome.status;
    let stdout = String::from_utf8_lossy(&outcome.stdout.bytes).to_string();
    let stderr = String::from_utf8_lossy(&outcome.stderr.bytes).to_string();
    let process_output = ProcessOutput {
        status,
        stdout,
        stderr,
        stdout_total_bytes: outcome.stdout.total_bytes,
        stdout_truncated: outcome.stdout.truncated,
        stderr_total_bytes: outcome.stderr.total_bytes,
        stderr_truncated: outcome.stderr.truncated,
    };
    let trace_outcome = process_outcome(Some(status), outcome.completion);
    let mut trace_fields = vec![
        flight_recorder::text("completion", outcome.completion.label()),
        flight_recorder::text("termination_stage", outcome.termination_stage.label()),
        flight_recorder::unsigned("stdout_bytes", process_output.stdout_total_bytes),
        flight_recorder::unsigned("stderr_bytes", process_output.stderr_total_bytes),
        flight_recorder::boolean("stdout_truncated", process_output.stdout_truncated),
        flight_recorder::boolean("stderr_truncated", process_output.stderr_truncated),
    ];
    append_status_fields(&mut trace_fields, status);
    trace.finish(trace_outcome, trace_fields);
    let deadline_error = (outcome.completion == ProcessCompletion::DeadlineExceeded).then(|| {
        format!(
            "subprocess timed out after {} ms",
            outcome.deadline.as_millis()
        )
    });
    let canceled_error = (outcome.completion == ProcessCompletion::Canceled)
        .then(|| "subprocess canceled".to_string());
    let completion_error = deadline_error.or(canceled_error);
    let (level, error) = if completion_error.is_none() && process_output.status.success() {
        (LogLevel::Debug, None)
    } else {
        (
            failure_level,
            completion_error.or_else(|| Some(process_failure_message(&process_output))),
        )
    };
    operation.finish(
        level,
        "process",
        "exit",
        if error.is_none() {
            "subprocess exited successfully".to_string()
        } else {
            format!("subprocess failed: {}", outcome.completion.label())
        },
        Some(observability::process_data_json(
            command,
            include_argv,
            observability::ProcessExecutionObservation {
                policy: policy.label(),
                elapsed_ms,
                deadline_ms: outcome.deadline.as_millis() as i64,
                child_pid: outcome.child_pid,
                process_group: outcome.process_group,
                status: &process_output.status.to_string(),
                completion: outcome.completion.label(),
                termination_stage: outcome.termination_stage.label(),
                stdout_bytes: process_output.stdout_total_bytes,
                stdout_truncated: process_output.stdout_truncated,
                stderr_bytes: process_output.stderr_total_bytes,
                stderr_truncated: process_output.stderr_truncated,
                error: error.as_deref(),
            },
        )),
    );
    match error {
        Some(error) if outcome.completion == ProcessCompletion::DeadlineExceeded => {
            Err(format!("{command_display}: {error}"))
        }
        Some(error) if outcome.completion == ProcessCompletion::Canceled => {
            Err(format!("{command_display}: {error}"))
        }
        _ => Ok(process_output),
    }
}

#[allow(dead_code)]
pub fn run_status_with_stdin(
    command: &mut Command,
    stdin: &str,
    policy: ProcessPolicy,
) -> Result<(), String> {
    let descriptor = infer_descriptor(command);
    run_status_with_stdin_named(command, stdin, policy, descriptor)
}

pub fn run_status_with_stdin_named(
    command: &mut Command,
    stdin: &str,
    policy: ProcessPolicy,
    descriptor: ProcessDescriptor,
) -> Result<(), String> {
    let command_display = observability::command_display(command);
    let output = run_output_with_settings(
        command,
        policy,
        LogLevel::Error,
        ProcessInput::Bytes(stdin.as_bytes()),
        descriptor,
    )?;
    output
        .status
        .success()
        .then_some(())
        .ok_or_else(|| format!("{command_display}: {}", process_failure_message(&output)))
}

pub fn supervise(
    command: &mut Command,
    policy: ProcessPolicy,
    input: ProcessInput<'_>,
    canceled: Option<&AtomicBool>,
) -> Result<ProcessOutcome, ProcessError> {
    let current_cancellation = canceled.is_none().then(current_cancellation).flatten();
    let canceled = canceled.or(current_cancellation.as_deref());
    supervise_with_settings(command, policy, policy.settings(), input, canceled)
}

fn supervise_with_settings(
    command: &mut Command,
    _policy: ProcessPolicy,
    settings: PolicySettings,
    input: ProcessInput<'_>,
    canceled: Option<&AtomicBool>,
) -> Result<ProcessOutcome, ProcessError> {
    command
        .stdin(match input {
            ProcessInput::Null => Stdio::null(),
            ProcessInput::Bytes(_) => Stdio::piped(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_group(command);

    let started = Instant::now();
    let mut child = spawn_managed(command).map_err(ProcessError::Spawn)?;
    let child_pid = child.id();
    let stop_readers = Arc::new(AtomicBool::new(false));
    let stdout = child_stdout(&mut child).take();
    let stderr = child_stderr(&mut child).take();
    let stdin = match input {
        ProcessInput::Null => None,
        ProcessInput::Bytes(_) => child_stdin(&mut child).take(),
    };
    let missing_pipe = if stdout.is_none() {
        Some("stdout")
    } else if stderr.is_none() {
        Some("stderr")
    } else if matches!(input, ProcessInput::Bytes(_)) && stdin.is_none() {
        Some("stdin")
    } else {
        None
    };
    if let Some(stream) = missing_pipe {
        let _ = terminate_active_child(&mut child, settings.termination_grace);
        return Err(ProcessError::MissingPipe(stream));
    }
    let stdout = stdout.expect("checked stdout pipe");
    let stderr = stderr.expect("checked stderr pipe");
    let stdout_reader =
        spawn_capture_reader(stdout, settings.capture_bytes, Arc::clone(&stop_readers));
    let stderr_reader =
        spawn_capture_reader(stderr, settings.capture_bytes, Arc::clone(&stop_readers));
    let stdin_writer = match input {
        ProcessInput::Null => None,
        ProcessInput::Bytes(bytes) => {
            let mut stdin = stdin.expect("checked stdin pipe");
            let bytes = bytes.to_vec();
            Some(std::thread::spawn(move || stdin.write_all(&bytes)))
        }
    };

    let mut status = None;
    let mut wait_error = None;
    let mut termination_error = None;
    let completion = loop {
        match child.try_wait() {
            Ok(Some(exit_status)) => {
                status = Some(exit_status);
                break completion_from_status(exit_status);
            }
            Ok(None) => {}
            Err(error) => {
                wait_error = Some(error);
                break ProcessCompletion::Canceled;
            }
        }
        if canceled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            break ProcessCompletion::Canceled;
        }
        if started.elapsed() >= settings.deadline {
            break ProcessCompletion::DeadlineExceeded;
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let mut termination_stage = TerminationStage::None;
    if matches!(
        completion,
        ProcessCompletion::DeadlineExceeded | ProcessCompletion::Canceled
    ) {
        match signal_term(child_pid) {
            Ok(stage) => termination_stage = stage,
            Err(error) => termination_error = Some(error),
        }
        let grace_deadline = Instant::now() + settings.termination_grace;
        while Instant::now() < grace_deadline {
            if status.is_none() {
                match child.try_wait() {
                    Ok(child_status) => status = child_status,
                    Err(error) => {
                        wait_error.get_or_insert(error);
                        break;
                    }
                }
            }
            if status.is_some()
                && stdout_reader.is_finished()
                && stderr_reader.is_finished()
                && !process_group_exists(child_pid)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        match force_kill(&mut child, child_pid) {
            Ok(true) => termination_stage = TerminationStage::Kill,
            Ok(false) => {}
            Err(error) => {
                termination_error.get_or_insert(error);
                let _ = child.kill();
            }
        }
    } else {
        let drain_deadline = Instant::now() + settings.termination_grace;
        while (!stdout_reader.is_finished() || !stderr_reader.is_finished())
            && Instant::now() < drain_deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !stdout_reader.is_finished() || !stderr_reader.is_finished() {
            match signal_term(child_pid) {
                Ok(stage) => termination_stage = stage,
                Err(error) => termination_error = Some(error),
            }
            let term_deadline = Instant::now() + settings.termination_grace;
            while (process_group_exists(child_pid)
                || !stdout_reader.is_finished()
                || !stderr_reader.is_finished())
                && Instant::now() < term_deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            if process_group_exists(child_pid)
                || !stdout_reader.is_finished()
                || !stderr_reader.is_finished()
            {
                match force_kill(&mut child, child_pid) {
                    Ok(true) => termination_stage = TerminationStage::Kill,
                    Ok(false) => {}
                    Err(error) => {
                        termination_error.get_or_insert(error);
                        let _ = child.kill();
                    }
                }
            }
        }
    }

    let reap_error = if status.is_none() {
        match child.wait() {
            Ok(child_status) => {
                status = Some(child_status);
                None
            }
            Err(error) => Some(ProcessError::Reap(error)),
        }
    } else {
        None
    };
    stop_readers.store(true, Ordering::Release);
    let stdin_result = join_stdin(stdin_writer);
    let stdout = join_capture_reader(stdout_reader, "stdout");
    let stderr = join_capture_reader(stderr_reader, "stderr");

    if let Some(error) = wait_error {
        return Err(ProcessError::Wait(error));
    }
    if let Some(error) = reap_error {
        return Err(error);
    }
    if let Some(error) = termination_error {
        return Err(error);
    }
    if !matches!(
        completion,
        ProcessCompletion::DeadlineExceeded | ProcessCompletion::Canceled
    ) {
        stdin_result?;
    }
    let stdout = stdout?;
    let stderr = stderr?;
    Ok(ProcessOutcome {
        status: status.expect("subprocess was reaped"),
        completion,
        termination_stage,
        stdout,
        stderr,
        elapsed: started.elapsed(),
        deadline: settings.deadline,
        child_pid,
        process_group: unix_process_group(child_pid),
    })
}

fn completion_from_status(status: ExitStatus) -> ProcessCompletion {
    #[cfg(unix)]
    if status.signal().is_some() {
        return ProcessCompletion::Signaled;
    }
    let _ = status;
    ProcessCompletion::Exited
}

const fn unix_process_group(child_pid: u32) -> Option<u32> {
    #[cfg(unix)]
    return Some(child_pid);
    #[cfg(windows)]
    {
        let _ = child_pid;
        None
    }
}

#[cfg(unix)]
fn spawn_managed(command: &mut Command) -> io::Result<ManagedChild> {
    const BUSY_RETRIES: usize = 4;

    for retry in 0..=BUSY_RETRIES {
        match command.spawn() {
            Err(error)
                if error.kind() == io::ErrorKind::ExecutableFileBusy && retry < BUSY_RETRIES =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result,
        }
    }
    unreachable!("bounded spawn loop always returns")
}

#[cfg(windows)]
fn spawn_managed(command: &mut Command) -> io::Result<ManagedChild> {
    use windows::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

    let owned = std::mem::replace(command, Command::new(""));
    let mut wrapped = CommandWrap::from(owned);
    // CreationFlags must precede JobObject so the latter can add CREATE_SUSPENDED and assign the
    // process before its first instruction executes.
    wrapped.wrap(CreationFlags(CREATE_NEW_PROCESS_GROUP));
    wrapped.wrap(JobObject);
    let result = wrapped.spawn();
    *command = wrapped.into_command();
    result
}

#[cfg(unix)]
fn spawn_capture_reader<R>(
    reader: R,
    max_bytes: usize,
    stop: Arc<AtomicBool>,
) -> JoinHandle<io::Result<CapturedTail>>
where
    R: Read + AsRawFd + Send + 'static,
{
    std::thread::spawn(move || read_captured_tail(reader, max_bytes, &stop))
}

#[cfg(unix)]
fn read_captured_tail(
    mut reader: impl Read + AsRawFd,
    max_bytes: usize,
    stop: &AtomicBool,
) -> io::Result<CapturedTail> {
    let fd = reader.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let mut tail = TailBuffer::new(max_bytes);
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(tail.finish()),
            Ok(read) => tail.push(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Acquire) {
                    return Ok(tail.finish());
                }
                let mut descriptor = libc::pollfd {
                    fd,
                    events: libc::POLLIN | libc::POLLHUP,
                    revents: 0,
                };
                let result = unsafe { libc::poll(&mut descriptor, 1, 25) };
                if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    return Err(io::Error::last_os_error());
                }
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(windows)]
fn spawn_capture_reader<R>(
    reader: R,
    max_bytes: usize,
    _stop: Arc<AtomicBool>,
) -> JoinHandle<io::Result<CapturedTail>>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || read_captured_tail(reader, max_bytes))
}

#[cfg(windows)]
fn read_captured_tail(mut reader: impl Read, max_bytes: usize) -> io::Result<CapturedTail> {
    let mut tail = TailBuffer::new(max_bytes);
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(tail.finish()),
            Ok(read) => tail.push(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

struct TailBuffer {
    bytes: Vec<u8>,
    max_bytes: usize,
    total_bytes: u64,
}

impl TailBuffer {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(max_bytes),
            max_bytes,
            total_bytes: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(bytes.len() as u64);
        if bytes.len() >= self.max_bytes {
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&bytes[bytes.len() - self.max_bytes..]);
            return;
        }
        let overflow = self
            .bytes
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(self.max_bytes);
        if overflow > 0 {
            self.bytes.drain(..overflow);
        }
        self.bytes.extend_from_slice(bytes);
    }

    fn finish(self) -> CapturedTail {
        CapturedTail {
            truncated: self.total_bytes > self.bytes.len() as u64,
            total_bytes: self.total_bytes,
            bytes: self.bytes,
        }
    }
}

fn join_capture_reader(
    reader: JoinHandle<io::Result<CapturedTail>>,
    stream: &'static str,
) -> Result<CapturedTail, ProcessError> {
    reader
        .join()
        .map_err(|_| ProcessError::ThreadPanicked(stream))?
        .map_err(|source| ProcessError::Read { stream, source })
}

fn join_stdin(writer: Option<JoinHandle<io::Result<()>>>) -> Result<(), ProcessError> {
    let Some(writer) = writer else {
        return Ok(());
    };
    writer
        .join()
        .map_err(|_| ProcessError::ThreadPanicked("stdin writer"))?
        .map_err(ProcessError::Stdin)
}

#[cfg(unix)]
fn signal_term(process_id: u32) -> Result<TerminationStage, ProcessError> {
    signal_process_group(process_id, libc::SIGTERM).map(|_| TerminationStage::Term)
}

#[cfg(windows)]
fn signal_term(_process_id: u32) -> Result<TerminationStage, ProcessError> {
    // GenerateConsoleCtrlEvent must run from a helper attached only to the child's console.
    // Calling it from Prism's console can interrupt the parent shell on headless Windows hosts.
    // Graceful delivery is conditional on Windows, so defer to bounded Job Object termination.
    Ok(TerminationStage::None)
}

#[cfg(unix)]
fn force_kill(_child: &mut ManagedChild, process_id: u32) -> Result<bool, ProcessError> {
    signal_process_group(process_id, libc::SIGKILL)
}

#[cfg(windows)]
fn force_kill(child: &mut ManagedChild, _process_id: u32) -> Result<bool, ProcessError> {
    child
        .start_kill()
        .map(|()| true)
        .map_err(|source| ProcessError::Signal {
            signal: "TerminateJobObject",
            source,
        })
}

#[cfg(unix)]
fn signal_process_group(process_id: u32, signal: libc::c_int) -> Result<bool, ProcessError> {
    send_process_group_signal(process_id, signal).map_err(|error| ProcessError::Signal {
        signal: match signal {
            libc::SIGTERM => "SIGTERM",
            libc::SIGKILL => "SIGKILL",
            _ => "signal",
        },
        source: error,
    })
}

fn process_group_exists(process_id: u32) -> bool {
    #[cfg(unix)]
    return probe_process_group(process_id).unwrap_or(true);
    #[cfg(windows)]
    return probe_process(process_id).unwrap_or(true);
}

pub fn terminate_active_child(
    child: &mut ManagedChild,
    grace: Duration,
) -> Result<TerminationStage, ProcessError> {
    let process_id = child.id();
    let mut first_error = None;
    let mut status = match child.try_wait() {
        Ok(status) => status,
        Err(error) => {
            first_error = Some(ProcessError::Wait(error));
            None
        }
    };
    let mut stage = TerminationStage::None;
    match signal_term(process_id) {
        Ok(term_stage) => stage = term_stage,
        Err(error) => {
            first_error.get_or_insert(error);
        }
    };
    let deadline = Instant::now() + grace;
    while process_group_exists(process_id) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        if status.is_none() {
            match child.try_wait() {
                Ok(child_status) => status = child_status,
                Err(error) => {
                    first_error.get_or_insert(ProcessError::Wait(error));
                    break;
                }
            }
        }
    }

    // The leader may have honored TERM while a descendant retained the group.
    match force_kill(child, process_id) {
        Ok(true) => stage = TerminationStage::Kill,
        Ok(false) => {}
        Err(error) => {
            first_error.get_or_insert(error);
            let _ = child.kill();
        }
    }
    if status.is_none()
        && let Err(error) = child.wait()
    {
        first_error.get_or_insert(ProcessError::Reap(error));
    }
    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(stage)
    }
}

struct InteractiveProcessOutput {
    status: ExitStatus,
    stdout: Option<CapturedTail>,
    canceled: bool,
    termination_stage: TerminationStage,
}

enum InteractiveIo<'a> {
    Inherited,
    CaptureStdout { input: &'a [u8], max_bytes: usize },
}

// This is the explicit interactive exception: normal execution is unbounded, while
// signal cancellation still terminates the child group and reaps its leader.
pub fn run_status_inherited(command: &mut Command) -> Result<(), String> {
    let descriptor = infer_descriptor(command);
    run_status_inherited_named(command, descriptor)
}

pub fn run_status_inherited_named(
    command: &mut Command,
    descriptor: ProcessDescriptor,
) -> Result<(), String> {
    let command_display = observability::command_display(command);
    let output = run_interactive(command, InteractiveIo::Inherited, descriptor)?;
    if output.canceled {
        return Err(format!(
            "{command_display}: interactive subprocess canceled"
        ));
    }
    output
        .status
        .success()
        .then_some(())
        .ok_or_else(|| format!("{command_display}: exited with {}", output.status))
}

#[cfg(unix)]
pub fn run_status_attached_named(
    command: &mut Command,
    descriptor: ProcessDescriptor,
) -> Result<(), String> {
    run_status_inherited_named(command, descriptor)
}

#[cfg(windows)]
pub fn run_status_attached_named(
    command: &mut Command,
    descriptor: ProcessDescriptor,
) -> Result<(), String> {
    let command_display = observability::command_display(command);
    let mut trace = ExternalCallTrace::begin(
        ExternalCallCategory::Process,
        descriptor.name,
        vec![flight_recorder::text("policy", "attached_console")],
    );
    let _interrupt_owner = crate::system::windows_console::attached_child_owns_interrupt()
        .map_err(|error| format!("{command_display}: prepare attached console: {error}"))?;
    let status = command.status().map_err(|error| {
        trace.finish(
            ExternalCallOutcome::SpawnFailed,
            process_error_fields(ProcessErrorKind::Spawn),
        );
        format!("{command_display}: {error}")
    })?;
    let completion = completion_from_status(status);
    trace.finish(
        process_outcome(Some(status), completion),
        vec![flight_recorder::text("completion", completion.label())],
    );
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("{command_display}: exited with {status}"))
}

pub fn run_capture_interactive(
    command: &mut Command,
    input: &[u8],
    max_bytes: usize,
) -> Result<String, String> {
    let descriptor = infer_descriptor(command);
    run_capture_interactive_named(command, input, max_bytes, descriptor)
}

pub fn run_capture_interactive_named(
    command: &mut Command,
    input: &[u8],
    max_bytes: usize,
    descriptor: ProcessDescriptor,
) -> Result<String, String> {
    let command_display = observability::command_display(command);
    let output = run_interactive(
        command,
        InteractiveIo::CaptureStdout { input, max_bytes },
        descriptor,
    )?;
    if output.canceled {
        return Err(format!(
            "{command_display}: interactive subprocess canceled"
        ));
    }
    if !output.status.success() {
        return Err(format!("{command_display}: exited with {}", output.status));
    }
    let stdout = output.stdout.expect("interactive stdout capture requested");
    if stdout.truncated {
        return Err(format!(
            "{command_display}: stdout was truncated from {} bytes",
            stdout.total_bytes
        ));
    }
    Ok(String::from_utf8_lossy(&stdout.bytes).into_owned())
}

fn run_interactive(
    command: &mut Command,
    io_mode: InteractiveIo<'_>,
    descriptor: ProcessDescriptor,
) -> Result<InteractiveProcessOutput, String> {
    let mut trace = ExternalCallTrace::begin(
        ExternalCallCategory::Process,
        descriptor.name,
        vec![flight_recorder::text("policy", "interactive")],
    );
    let result = run_interactive_inner(command, io_mode, &mut trace);
    match &result {
        Ok(output) => {
            let completion = if output.canceled {
                ProcessCompletion::Canceled
            } else {
                completion_from_status(output.status)
            };
            let mut fields = vec![
                flight_recorder::text("completion", completion.label()),
                flight_recorder::text("termination_stage", output.termination_stage.label()),
            ];
            append_status_fields(&mut fields, output.status);
            if let Some(stdout) = output.stdout.as_ref() {
                fields.push(flight_recorder::unsigned(
                    "stdout_bytes",
                    stdout.total_bytes,
                ));
                fields.push(flight_recorder::boolean(
                    "stdout_truncated",
                    stdout.truncated,
                ));
            }
            trace.finish(process_outcome(Some(output.status), completion), fields);
        }
        Err(_) => trace.finish(
            ExternalCallOutcome::Failed,
            vec![
                flight_recorder::text("completion", "supervision_error"),
                flight_recorder::text("error_kind", "interactive"),
                flight_recorder::text("termination_stage", "none"),
            ],
        ),
    }
    result
}

fn run_interactive_inner(
    command: &mut Command,
    io_mode: InteractiveIo<'_>,
    trace: &mut ExternalCallTrace,
) -> Result<InteractiveProcessOutput, String> {
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
    let current = current_cancellation();
    let local_cancellation = if current.is_none() {
        Some(InteractiveSignalCancellation::install().map_err(|error| {
            format!("{command_display}: install interactive cancellation: {error}")
        })?)
    } else {
        None
    };
    let canceled = current
        .as_deref()
        .or_else(|| local_cancellation.as_ref().map(|guard| guard.flag.as_ref()))
        .expect("interactive cancellation is always installed");
    match io_mode {
        InteractiveIo::Inherited => {
            command
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        }
        InteractiveIo::CaptureStdout { .. } => {
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
        }
    }
    configure_process_group(command);
    let mut child = spawn_managed(command).map_err(|error| {
        trace.finish(
            ExternalCallOutcome::SpawnFailed,
            process_error_fields(ProcessErrorKind::Spawn),
        );
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
    let foreground = ForegroundProcessGroup::give_to(child.id()).map_err(|error| {
        let _ = terminate_active_child(&mut child, Duration::from_secs(1));
        format!("{command_display}: give subprocess the terminal: {error}")
    })?;
    let stop_reader = Arc::new(AtomicBool::new(false));
    let (stdin_writer, stdout_reader) = match io_mode {
        InteractiveIo::Inherited => (None, None),
        InteractiveIo::CaptureStdout { input, max_bytes } => {
            let mut stdin = child_stdin(&mut child)
                .take()
                .expect("configured interactive stdin pipe");
            let stdout = child_stdout(&mut child)
                .take()
                .expect("configured interactive stdout pipe");
            let input = input.to_vec();
            (
                Some(std::thread::spawn(move || stdin.write_all(&input))),
                Some(spawn_capture_reader(
                    stdout,
                    max_bytes,
                    Arc::clone(&stop_reader),
                )),
            )
        }
    };
    let mut was_canceled = false;
    let mut termination_stage = TerminationStage::None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(child_status)) => {
                break child_status;
            }
            Ok(None) => {}
            Err(error) => {
                let _ = terminate_active_child(&mut child, Duration::from_secs(1));
                return Err(format!(
                    "{command_display}: wait for interactive subprocess: {error}"
                ));
            }
        }
        if canceled.load(Ordering::Acquire) {
            was_canceled = true;
            termination_stage = terminate_active_child(&mut child, Duration::from_secs(1))
                .map_err(|error| {
                    format!("{command_display}: cancel interactive subprocess: {error}")
                })?;
            let status = child
                .try_wait()
                .map_err(|error| {
                    format!("{command_display}: reap interactive subprocess: {error}")
                })?
                .ok_or_else(|| format!("{command_display}: subprocess was not reaped"))?;
            break status;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(foreground);

    if let Some(reader) = stdout_reader.as_ref() {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !reader.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !reader.is_finished() {
            termination_stage = terminate_active_child(&mut child, Duration::from_secs(1))
                .map_err(|error| format!("{command_display}: drain interactive stdout: {error}"))?;
        }
    }
    stop_reader.store(true, Ordering::Release);
    let stdin_result = join_stdin(stdin_writer);
    if !was_canceled {
        stdin_result.map_err(|error| format!("{command_display}: {error}"))?;
    }
    let stdout = stdout_reader
        .map(|reader| join_capture_reader(reader, "stdout"))
        .transpose()
        .map_err(|error| format!("{command_display}: {error}"))?;
    let elapsed_ms = started.elapsed().as_millis() as i64;
    if status.success() && !was_canceled {
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
        Ok(InteractiveProcessOutput {
            status,
            stdout,
            canceled: false,
            termination_stage,
        })
    } else {
        let message = if was_canceled {
            format!("canceled after {termination_stage:?}")
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
        Ok(InteractiveProcessOutput {
            status,
            stdout,
            canceled: was_canceled,
            termination_stage,
        })
    }
}

struct InteractiveSignalCancellation {
    flag: Arc<AtomicBool>,
    #[cfg(unix)]
    registrations: Vec<signal_hook::SigId>,
}

impl InteractiveSignalCancellation {
    fn install() -> io::Result<Self> {
        #[cfg(unix)]
        let flag = Arc::new(AtomicBool::new(false));
        #[cfg(windows)]
        let flag = crate::system::windows_console::cancellation()?;
        #[cfg(unix)]
        let registrations = {
            let mut registrations = Vec::new();
            for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
                match signal_hook::flag::register(signal, Arc::clone(&flag)) {
                    Ok(registration) => registrations.push(registration),
                    Err(error) => {
                        for registration in registrations {
                            signal_hook::low_level::unregister(registration);
                        }
                        return Err(error);
                    }
                }
            }
            registrations
        };
        Ok(Self {
            flag,
            #[cfg(unix)]
            registrations,
        })
    }
}

#[cfg(unix)]
impl Drop for InteractiveSignalCancellation {
    fn drop(&mut self) {
        for registration in self.registrations.drain(..) {
            signal_hook::low_level::unregister(registration);
        }
    }
}

#[cfg(unix)]
struct ForegroundProcessGroup {
    original: Option<libc::pid_t>,
}

#[cfg(unix)]
impl ForegroundProcessGroup {
    fn give_to(process_id: u32) -> io::Result<Self> {
        if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
            return Ok(Self { original: None });
        }
        let original = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
        if original == -1 {
            return Err(io::Error::last_os_error());
        }
        set_foreground_process_group(process_id as libc::pid_t)?;
        Ok(Self {
            original: Some(original),
        })
    }
}

#[cfg(unix)]
impl Drop for ForegroundProcessGroup {
    fn drop(&mut self) {
        if let Some(original) = self.original {
            let _ = set_foreground_process_group(original);
        }
    }
}

#[cfg(windows)]
struct ForegroundProcessGroup;

#[cfg(windows)]
impl ForegroundProcessGroup {
    fn give_to(_process_id: u32) -> io::Result<Self> {
        // Windows console and psmux/ConPTY keep ownership of the inherited console. Prism does
        // not emulate Unix tcsetpgrp handoff.
        Ok(Self)
    }
}

#[cfg(windows)]
impl Drop for ForegroundProcessGroup {
    fn drop(&mut self) {}
}

#[cfg(unix)]
fn set_foreground_process_group(process_group: libc::pid_t) -> io::Result<()> {
    unsafe {
        let mut blocked = std::mem::zeroed::<libc::sigset_t>();
        let mut previous = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGTTOU);
        let block_result = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous);
        if block_result != 0 {
            return Err(io::Error::from_raw_os_error(block_result));
        }
        let result = libc::tcsetpgrp(libc::STDIN_FILENO, process_group);
        let error = (result == -1).then(io::Error::last_os_error);
        let restore_result =
            libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        if let Some(error) = error {
            Err(error)
        } else if restore_result != 0 {
            Err(io::Error::from_raw_os_error(restore_result))
        } else {
            Ok(())
        }
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
    if command.contains('/') || command.contains('\\') {
        return executable_path_exists(Path::new(command));
    }
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| executable_path_exists(&dir.join(command)))
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

pub fn command_version(command: &str) -> Option<String> {
    let argv = split_command_words(command);
    let program = argv.first()?;
    if !command_exists(program) {
        return None;
    }
    let output = run_output_allow_failure(
        Command::new(program).arg("--version"),
        ProcessPolicy::Metadata,
    )
    .ok()?;
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
        run_status(
            Command::new(program).args(&argv[1..]).current_dir(cwd),
            ProcessPolicy::WorkflowStep,
        )
        .map_err(|error| format!("{label} check `{command}` failed: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_observation_and_termination_decision_table_is_complete() {
        let first = ProcessIdentity(10);
        let second = ProcessIdentity(20);
        let cases = [
            (
                None,
                NativeProcessObservation::Missing,
                ProcessObservation::Missing,
                ProcessDecision::TerminationOutcome(TerminationOutcome::AlreadyExited),
            ),
            (
                None,
                NativeProcessObservation::Running(None),
                ProcessObservation::RunningUnverifiable,
                ProcessDecision::TerminationOutcome(TerminationOutcome::Unverifiable),
            ),
            (
                None,
                NativeProcessObservation::Running(Some(first)),
                ProcessObservation::RunningUnverifiable,
                ProcessDecision::TerminationOutcome(TerminationOutcome::Unverifiable),
            ),
            (
                None,
                NativeProcessObservation::Running(Some(second)),
                ProcessObservation::RunningUnverifiable,
                ProcessDecision::TerminationOutcome(TerminationOutcome::Unverifiable),
            ),
            (
                Some(first),
                NativeProcessObservation::Missing,
                ProcessObservation::Missing,
                ProcessDecision::TerminationOutcome(TerminationOutcome::AlreadyExited),
            ),
            (
                Some(first),
                NativeProcessObservation::Running(None),
                ProcessObservation::RunningUnverifiable,
                ProcessDecision::TerminationOutcome(TerminationOutcome::Unverifiable),
            ),
            (
                Some(first),
                NativeProcessObservation::Running(Some(first)),
                ProcessObservation::RunningSameProcess,
                ProcessDecision::Terminate,
            ),
            (
                Some(first),
                NativeProcessObservation::Running(Some(second)),
                ProcessObservation::IdentityReused,
                ProcessDecision::TerminationOutcome(TerminationOutcome::IdentityReused),
            ),
        ];

        for (recorded, native, observation, termination) in cases {
            assert_eq!(
                decide_process_request(recorded, native, ProcessRequest::Observe),
                ProcessDecision::Observation(observation)
            );
            assert_eq!(
                decide_process_request(recorded, native, ProcessRequest::Terminate),
                termination
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn platform_contract_process_probe_treats_permission_denied_as_existing() {
        assert!(probe_result(-1, Some(libc::EPERM)).unwrap());
        assert!(!probe_result(-1, Some(libc::ESRCH)).unwrap());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_process_stat_parser_handles_spaces_and_parentheses_in_name() {
        let mut fields = vec!["S"; 19];
        fields.push("4242");
        let stat = format!("12 (a process) name) {}", fields.join(" "));

        assert_eq!(parse_linux_process_start_time(&stat), Some(4242));
    }

    #[test]
    fn platform_smoke_native_process_identity_observes_current_process() {
        let recorded = record_process(std::process::id()).unwrap();

        assert!(recorded.identity.is_some());
        assert_eq!(
            observe_process(recorded).unwrap(),
            ProcessObservation::RunningSameProcess
        );
    }

    #[test]
    fn platform_smoke_native_absent_process_and_group_are_missing() {
        let process = RecordedProcess::from_stored(i32::MAX as u32, Some(1));

        assert_eq!(
            observe_process(process).unwrap(),
            ProcessObservation::Missing
        );
        assert_eq!(
            terminate_recorded_process(process, Duration::from_millis(10)).unwrap(),
            TerminationOutcome::AlreadyExited
        );
        #[cfg(unix)]
        assert!(!probe_process_group(process.pid).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn platform_smoke_native_identity_checked_termination_rejects_reuse() {
        use std::os::unix::process::CommandExt;

        let mut child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let recorded = record_process(child.id()).unwrap();
        let reused = RecordedProcess {
            pid: recorded.pid,
            identity: recorded
                .identity
                .map(|identity| ProcessIdentity(identity.0.wrapping_add(1))),
        };

        assert_eq!(
            terminate_recorded_process(reused, Duration::from_millis(50)).unwrap(),
            TerminationOutcome::IdentityReused
        );
        assert!(child.try_wait().unwrap().is_none());

        let reaper = std::thread::spawn(move || child.wait().unwrap());
        assert_eq!(
            terminate_recorded_process(recorded, Duration::from_secs(1)).unwrap(),
            TerminationOutcome::Terminated
        );
        reaper.join().unwrap();
    }

    #[test]
    fn process_descriptors_use_only_finite_known_command_labels() {
        assert_eq!(
            infer_descriptor(
                Command::new("git")
                    .arg("-C")
                    .arg("/secret/repo")
                    .args(["fetch", "origin"])
            ),
            ProcessDescriptor::new("git.fetch")
        );
        assert_eq!(
            infer_descriptor(Command::new("gh").args(["api", "graphql", "-f", "query=secret"])),
            ProcessDescriptor::new("gh.api.graphql")
        );
        assert_eq!(
            infer_descriptor(Command::new("/tmp/custom-tool").args(["fetch", "secret"])),
            ProcessDescriptor::new("process.other")
        );
        assert_eq!(
            ProcessDescriptor::for_tmux(Command::new("/tmp/custom-tmux").args([
                "display-message",
                "-p",
                "#{pane_start_command}"
            ])),
            ProcessDescriptor::new("tmux.pane.start_command")
        );
    }

    #[test]
    fn explicit_descriptor_attributes_a_configured_executable_logically() {
        let mut command = Command::new("/tmp/company-github-wrapper");
        command.args(["pr", "view", "42"]);
        let inferred = infer_descriptor(&command);
        let configured = ProcessDescriptor::new("gh.pr.view");

        assert_eq!(inferred, ProcessDescriptor::new("process.other"));
        assert_eq!(configured.name, "gh.pr.view");
    }

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

    #[cfg(unix)]
    #[test]
    fn output_timeout_terminates_long_running_process() {
        let error = run_output_allow_failure(
            Command::new("sh").args(["-c", "exec sleep 1"]),
            ProcessPolicy::Test,
        )
        .err()
        .expect("long-running process should time out");

        assert!(error.contains("subprocess timed out"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn run_capture_rejects_truncated_success_output() {
        let error = run_capture(
            Command::new("sh").args(["-c", "dd if=/dev/zero bs=2048 count=1 2>/dev/null"]),
            ProcessPolicy::Test,
        )
        .unwrap_err();

        assert!(error.contains("stdout was truncated"), "{error}");
        assert!(error.contains("2048 total bytes"), "{error}");
    }

    #[test]
    #[cfg(unix)]
    fn detached_process_is_reaped_after_exit() {
        let pid = spawn_detached(
            Command::new("sh").args(["-c", "exit 0"]),
            DetachedProcessPolicy::WorkerDaemon,
        )
        .unwrap() as libc::pid_t;
        let deadline = Instant::now() + Duration::from_secs(2);

        loop {
            let result = unsafe { libc::kill(pid, 0) };
            if result != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(Instant::now() < deadline, "detached process was not reaped");
            std::thread::yield_now();
        }
    }

    #[cfg(unix)]
    #[test]
    fn interactive_capture_preserves_input_without_a_deadline() {
        let output = run_capture_interactive(
            Command::new("sh").args(["-c", "cat"]),
            b"selected-plan.md\n",
            1024,
        )
        .unwrap();

        assert_eq!(output, "selected-plan.md\n");
    }

    #[test]
    #[cfg(unix)]
    fn platform_smoke_native_interactive_cancellation_kills_process_group_and_reaps_leader() {
        let temp = std::env::temp_dir().join(format!(
            "prism-interactive-process-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&temp).unwrap();
        let leader_path = temp.join("leader.pid");
        let descendant_path = temp.join("descendant.pid");
        let canceled = Arc::new(AtomicBool::new(false));
        let trigger = Arc::clone(&canceled);
        let ready_path = descendant_path.clone();
        let trigger_thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !ready_path.exists() {
                assert!(
                    Instant::now() < deadline,
                    "interactive child did not become ready"
                );
                std::thread::yield_now();
            }
            trigger.store(true, Ordering::Release);
        });
        let script = r#"
            trap '' TERM
            printf '%s\n' "$$" > "$1"
            (
                trap '' TERM
                while :; do :; done
            ) &
            descendant=$!
            printf '%s\n' "$descendant" > "$2"
            wait "$descendant"
        "#;
        let started = Instant::now();
        let error = with_cancellation(canceled, || {
            run_status_inherited(
                Command::new("sh")
                    .arg("-c")
                    .arg(script)
                    .arg("interactive-fixture")
                    .arg(&leader_path)
                    .arg(&descendant_path),
            )
        })
        .unwrap_err();
        trigger_thread.join().unwrap();

        assert!(error.contains("interactive subprocess canceled"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(4));
        for path in [&leader_path, &descendant_path] {
            let pid = std::fs::read_to_string(path)
                .unwrap()
                .trim()
                .parse::<libc::pid_t>()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let result = unsafe { libc::kill(pid, 0) };
                if result != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "interactive process survived cancellation"
                );
                std::thread::yield_now();
            }
        }
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn supervisor_uses_null_stdin_unless_input_is_supplied() {
        let outcome = supervise(
            Command::new("sh").args([
                "-c",
                "if read value; then exit 9; else printf 'stdin-eof'; fi",
            ]),
            ProcessPolicy::Metadata,
            ProcessInput::Null,
            None,
        )
        .unwrap();

        assert!(outcome.status.success());
        assert_eq!(outcome.stdout.bytes, b"stdin-eof");
    }

    #[test]
    fn spawn_error_retains_its_io_source() {
        let error = supervise(
            &mut Command::new("/prism-test/command-that-does-not-exist"),
            ProcessPolicy::Metadata,
            ProcessInput::Null,
            None,
        )
        .unwrap_err();

        assert!(matches!(error, ProcessError::Spawn(_)));
        assert_eq!(error.kind(), ProcessErrorKind::Spawn);
        assert_eq!(error.kind().label(), "spawn");
        assert!(error.source().is_some());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn supervisor_retries_an_executable_that_is_temporarily_busy() {
        use std::os::unix::fs::PermissionsExt;

        let temp = std::env::temp_dir().join(format!(
            "prism-busy-executable-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&temp).unwrap();
        let executable = temp.join("command");
        let mut writer = std::fs::File::create(&executable).unwrap();
        writer.write_all(b"#!/bin/sh\nprintf ready\n").unwrap();
        writer.flush().unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let release_writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            drop(writer);
        });

        let outcome = supervise(
            &mut Command::new(&executable),
            ProcessPolicy::Metadata,
            ProcessInput::Null,
            None,
        )
        .unwrap();

        release_writer.join().unwrap();
        assert!(outcome.status.success());
        assert_eq!(outcome.stdout.bytes, b"ready");
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn supervisor_reports_requested_cancellation() {
        let canceled = AtomicBool::new(true);
        let outcome = supervise(
            Command::new("sh").args(["-c", "exec sleep 30"]),
            ProcessPolicy::Test,
            ProcessInput::Null,
            Some(&canceled),
        )
        .unwrap();

        assert_eq!(outcome.completion, ProcessCompletion::Canceled);
        assert!(matches!(
            outcome.termination_stage,
            TerminationStage::Term | TerminationStage::Kill
        ));
    }

    #[test]
    #[cfg(unix)]
    fn platform_smoke_native_supervisor_kills_term_ignoring_descendant_and_bounds_capture() {
        let temp = std::env::temp_dir().join(format!(
            "prism-process-tree-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&temp).unwrap();
        let descendant_path = temp.join("descendant.pid");
        let script = r#"
            trap '' TERM
            (
                trap '' TERM
                i=0
                while [ "$i" -lt 400 ]; do
                    printf 'stdout-%04d-xxxxxxxx\n' "$i"
                    printf 'stderr-%04d-yyyyyyyy\n' "$i" >&2
                    i=$((i + 1))
                done
                while :; do :; done
            ) &
            descendant=$!
            printf '%s\n' "$descendant" > "$1"
            wait "$descendant"
        "#;
        let started = Instant::now();
        let outcome = supervise(
            Command::new("sh")
                .arg("-c")
                .arg(script)
                .arg("process-fixture")
                .arg(&descendant_path),
            ProcessPolicy::Test,
            ProcessInput::Null,
            None,
        )
        .unwrap();

        assert_eq!(outcome.completion, ProcessCompletion::DeadlineExceeded);
        assert_eq!(outcome.termination_stage, TerminationStage::Kill);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(outcome.stdout.bytes.len(), 1024);
        assert_eq!(outcome.stderr.bytes.len(), 1024);
        assert!(outcome.stdout.total_bytes > outcome.stdout.bytes.len() as u64);
        assert!(outcome.stderr.total_bytes > outcome.stderr.bytes.len() as u64);
        assert!(outcome.stdout.truncated);
        assert!(outcome.stderr.truncated);

        let descendant = std::fs::read(&descendant_path).unwrap();
        let descendant = std::str::from_utf8(&descendant)
            .unwrap()
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();
        let gone_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let result = unsafe { libc::kill(descendant, 0) };
            if result != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                Instant::now() < gone_deadline,
                "descendant survived group kill"
            );
            std::thread::yield_now();
        }

        assert!(!outcome.stdout.bytes.is_empty());
        assert!(!outcome.stderr.bytes.is_empty());
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_job_cancellation_bounds_output_and_removes_stubborn_descendants() {
        let temp = std::env::temp_dir().join(format!(
            "prism-windows-process-{}-{}",
            std::process::id(),
            crate::util::timestamp_nanos()
        ));
        std::fs::create_dir_all(&temp).unwrap();
        let descendant_path = temp.join("descendant.pid");
        let canceled = Arc::new(AtomicBool::new(false));
        let cancel_when_ready = Arc::clone(&canceled);
        let marker = descendant_path.clone();
        let canceler = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !marker.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                marker.exists(),
                "PowerShell fixture did not report its descendant"
            );
            cancel_when_ready.store(true, Ordering::Release);
        });
        let script = r#"
            $child = Start-Process pwsh.exe -ArgumentList @(
                '-NoProfile', '-Command',
                'while ($true) { Start-Sleep -Seconds 1 }'
            ) -PassThru
            Set-Content -LiteralPath $env:PRISM_DESCENDANT_PID -Value $child.Id
            [Console]::Out.Write(('o' * 8192))
            [Console]::Error.Write(('e' * 8192))
            while ($true) { Start-Sleep -Seconds 1 }
        "#;
        let settings = PolicySettings {
            deadline: Duration::from_secs(15),
            termination_grace: Duration::from_millis(100),
            capture_bytes: 1024,
        };
        let mut command = Command::new("pwsh.exe");
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ])
            .env("PRISM_DESCENDANT_PID", &descendant_path);
        let outcome = supervise_with_settings(
            &mut command,
            ProcessPolicy::Test,
            settings,
            ProcessInput::Null,
            Some(&canceled),
        )
        .unwrap();
        canceler.join().unwrap();

        assert_eq!(outcome.completion, ProcessCompletion::Canceled);
        assert_eq!(outcome.termination_stage, TerminationStage::Kill);
        assert_eq!(outcome.process_group, None);
        assert_eq!(outcome.stdout.bytes.len(), 1024);
        assert_eq!(outcome.stderr.bytes.len(), 1024);
        assert!(outcome.stdout.truncated);
        assert!(outcome.stderr.truncated);

        let descendant = std::fs::read_to_string(&descendant_path)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let recorded = record_process(descendant).unwrap();
        assert_eq!(
            recorded.identity, None,
            "managed descendant survived Job termination"
        );
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_managed_child_drop_removes_descendants_after_parent_failure() {
        let marker = std::env::temp_dir().join(format!(
            "prism-windows-drop-{}-{}.pid",
            std::process::id(),
            crate::util::timestamp_nanos()
        ));
        let script = r#"
            $child = Start-Process pwsh.exe -ArgumentList @(
                '-NoProfile', '-Command',
                'while ($true) { Start-Sleep -Seconds 1 }'
            ) -PassThru
            Set-Content -LiteralPath $env:PRISM_DESCENDANT_PID -Value $child.Id
            while ($true) { Start-Sleep -Seconds 1 }
        "#;
        let mut command = Command::new("pwsh.exe");
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ])
            .env("PRISM_DESCENDANT_PID", &marker);
        let child = SupervisedChild::spawn(&mut command, Some(ProcessPolicy::Test), None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let descendant = loop {
            if let Ok(contents) = std::fs::read_to_string(&marker)
                && let Ok(descendant) = contents.trim().parse::<u32>()
            {
                break descendant;
            }
            assert!(
                Instant::now() < deadline,
                "PowerShell fixture did not report its descendant"
            );
            std::thread::sleep(Duration::from_millis(10));
        };

        drop(child);

        assert_eq!(
            observe_process(RecordedProcess::from_stored(descendant, Some(u64::MAX))).unwrap(),
            ProcessObservation::Missing
        );
        std::fs::remove_file(marker).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_pre_spawn_cancellation_is_reaped_without_a_race() {
        let canceled = AtomicBool::new(true);
        let mut command = Command::new("pwsh.exe");
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "while ($true) { Start-Sleep -Seconds 1 }",
        ]);

        let outcome = supervise(
            &mut command,
            ProcessPolicy::Test,
            ProcessInput::Null,
            Some(&canceled),
        )
        .unwrap();

        assert_eq!(outcome.completion, ProcessCompletion::Canceled);
        assert_eq!(
            observe_process(RecordedProcess::from_stored(outcome.child_pid, None)).unwrap(),
            ProcessObservation::Missing
        );
    }
}
