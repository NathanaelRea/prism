use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU8, Ordering},
    time::{Duration, Instant},
};

use process_wrap::tokio::{CommandWrap, JobObject, KillOnDrop};
use tokio::{process::Command, time};
use windows::{
    Win32::{
        Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::{
            Console::{
                AttachConsole, CTRL_BREAK_EVENT, FreeConsole, GenerateConsoleCtrlEvent,
                SetConsoleCtrlHandler,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectExtendedLimitInformation, OpenJobObjectW, SetInformationJobObject,
                TerminateJobObject,
            },
            SystemServices::{JOB_OBJECT_ASSIGN_PROCESS, JOB_OBJECT_QUERY},
            Threading::{
                CREATE_NEW_CONSOLE, CREATE_NEW_PROCESS_GROUP, GetCurrentProcess, GetProcessTimes,
                OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION,
                WaitForSingleObject,
            },
        },
    },
    core::{BOOL, PCWSTR},
};

use crate::support::{SpikeResult, TempDir, fail, require};

const SYNCHRONIZE_PROCESS: u32 = 0x0010_0000;
const HANDLER_ROOT: u8 = 1;
const HANDLER_IGNORE: u8 = 2;
static HANDLER_MODE: AtomicU8 = AtomicU8::new(0);

struct ContainmentJob(HANDLE);

impl ContainmentJob {
    fn create(name: &str) -> SpikeResult<Self> {
        let name = widestring::U16CString::from_str(name)?;
        let handle = unsafe { CreateJobObjectW(None, PCWSTR(name.as_ptr()))? };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                size_of_val(&limits) as u32,
            )?;
        }
        Ok(Self(handle))
    }

    fn terminate(&self) -> SpikeResult {
        unsafe {
            TerminateJobObject(self.0, 1)?;
        }
        Ok(())
    }
}

impl Drop for ContainmentJob {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct ManagedTree {
    child: tokio::process::Child,
    job: ContainmentJob,
}

impl ManagedTree {
    fn id(&self) -> Option<u32> {
        self.child.id()
    }

    fn terminate(&self) -> SpikeResult {
        self.job.terminate()
    }
}

fn join_containment_job(name: &str) -> SpikeResult {
    let name = widestring::U16CString::from_str(name)?;
    let job = unsafe {
        OpenJobObjectW(
            JOB_OBJECT_ASSIGN_PROCESS | JOB_OBJECT_QUERY,
            false,
            PCWSTR(name.as_ptr()),
        )?
    };
    let result = (|| {
        let mut already_joined = false.into();
        unsafe {
            IsProcessInJob(GetCurrentProcess(), Some(job), &mut already_joined)?;
            if !already_joined.as_bool() {
                AssignProcessToJobObject(job, GetCurrentProcess())?;
            }
        }
        Ok(())
    })();
    unsafe {
        CloseHandle(job)?;
    }
    result
}

extern "system" fn console_handler(control: u32) -> BOOL {
    if control != CTRL_BREAK_EVENT {
        return false.into();
    }
    match HANDLER_MODE.load(Ordering::Relaxed) {
        HANDLER_ROOT => {
            HANDLER_MODE.store(0, Ordering::Release);
            true.into()
        }
        HANDLER_IGNORE => true.into(),
        _ => false.into(),
    }
}

fn install_console_handler(mode: u8) -> SpikeResult {
    HANDLER_MODE.store(mode, Ordering::Release);
    unsafe {
        SetConsoleCtrlHandler(Some(console_handler), true)?;
    }
    Ok(())
}

fn append_pid(path: &Path, role: &str) -> SpikeResult {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{role}={}", std::process::id())?;
    file.flush()?;
    Ok(())
}

pub async fn run_root(
    job_name: String,
    pid_file: PathBuf,
    graceful_marker: PathBuf,
) -> SpikeResult {
    join_containment_job(&job_name)?;
    install_console_handler(HANDLER_ROOT)?;
    append_pid(&pid_file, "root")?;

    let executable = std::env::current_exe()?;
    let mut nested = Command::new(executable);
    nested
        .arg("--process-nested-child")
        .arg(&job_name)
        .arg(&pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut nested = nested.spawn()?;

    loop {
        if HANDLER_MODE.load(Ordering::Acquire) == 0 {
            fs::write(&graceful_marker, b"CTRL_BREAK_EVENT handled\n")?;
            return Ok(());
        }
        if let Some(status) = nested.try_wait()? {
            return fail(format!(
                "nested process exited before cancellation: {status}"
            ));
        }
        time::sleep(Duration::from_millis(10)).await;
    }
}

pub async fn run_nested_child(job_name: String, pid_file: PathBuf) -> SpikeResult {
    join_containment_job(&job_name)?;
    install_console_handler(HANDLER_IGNORE)?;
    append_pid(&pid_file, "nested")?;

    let executable = std::env::current_exe()?;
    let mut command = CommandWrap::with_new(executable, |command| {
        command
            .arg("--process-leaf")
            .arg(&pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    });
    command.wrap(JobObject).wrap(KillOnDrop);
    let mut leaf = command.spawn()?;
    leaf.wait().await?;
    Ok(())
}

pub async fn run_leaf(pid_file: PathBuf) -> SpikeResult {
    install_console_handler(HANDLER_IGNORE)?;
    append_pid(&pid_file, "leaf")?;
    loop {
        time::sleep(Duration::from_secs(60)).await;
    }
}

pub fn run_signal_sender(process_group_id: u32) -> SpikeResult {
    unsafe {
        let _ = FreeConsole();
        AttachConsole(process_group_id)?;
    }
    install_console_handler(HANDLER_IGNORE)?;
    unsafe {
        GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, process_group_id)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessIdentity(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IdentityObservation {
    Gone,
    Match,
    Reused,
}

fn observe_identity(pid: u32, expected: ProcessIdentity) -> SpikeResult<IdentityObservation> {
    match process_creation_time(pid)? {
        None => Ok(IdentityObservation::Gone),
        Some(actual) if actual == expected => Ok(IdentityObservation::Match),
        Some(_) => Ok(IdentityObservation::Reused),
    }
}

fn process_creation_time(pid: u32) -> SpikeResult<Option<ProcessIdentity>> {
    let rights = PROCESS_ACCESS_RIGHTS(PROCESS_QUERY_LIMITED_INFORMATION.0 | SYNCHRONIZE_PROCESS);
    let handle = match unsafe { OpenProcess(rights, false, pid) } {
        Ok(handle) => handle,
        Err(error) if error.code().0 as u32 == 0x8007_0057 => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let result = process_creation_time_from_handle(handle);
    unsafe {
        CloseHandle(handle)?;
    }
    result.map(Some)
}

fn process_creation_time_from_handle(handle: HANDLE) -> SpikeResult<ProcessIdentity> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user)?;
    }
    Ok(ProcessIdentity(
        (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime),
    ))
}

fn process_running(pid: u32) -> SpikeResult<bool> {
    let rights = PROCESS_ACCESS_RIGHTS(PROCESS_QUERY_LIMITED_INFORMATION.0 | SYNCHRONIZE_PROCESS);
    let handle = match unsafe { OpenProcess(rights, false, pid) } {
        Ok(handle) => handle,
        Err(error) if error.code().0 as u32 == 0x8007_0057 => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let wait = unsafe { WaitForSingleObject(handle, 0) };
    unsafe {
        CloseHandle(handle)?;
    }
    match wait {
        WAIT_TIMEOUT => Ok(true),
        WAIT_OBJECT_0 => Ok(false),
        WAIT_FAILED => Err(std::io::Error::last_os_error().into()),
        other => fail(format!("unexpected process wait result {other:?}")),
    }
}

fn managed_tree(pid_file: &Path, marker: &Path) -> SpikeResult<ManagedTree> {
    let job_name = crate::support::unique_name("prism-windows-job");
    let job = ContainmentJob::create(&job_name)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--process-root")
        .arg(&job_name)
        .arg(pid_file)
        .arg(marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags((CREATE_NEW_PROCESS_GROUP | CREATE_NEW_CONSOLE).0);
    let child = command.spawn()?;
    Ok(ManagedTree { child, job })
}

async fn signal_process_group(process_group_id: u32) -> SpikeResult {
    let output = Command::new(std::env::current_exe()?)
        .arg("--process-signal")
        .arg(process_group_id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await?;
    require(
        output.status.success(),
        format!(
            "CTRL+BREAK sender failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    )
}

async fn wait_for_pids(path: &Path) -> SpikeResult<Vec<u32>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let contents = fs::read_to_string(path).unwrap_or_default();
        let pids: Result<Vec<_>, _> = contents
            .lines()
            .map(|line| {
                line.split_once('=')
                    .map(|(_, pid)| pid)
                    .unwrap_or("")
                    .parse::<u32>()
            })
            .collect();
        if let Ok(pids) = pids
            && pids.len() == 3
        {
            return Ok(pids);
        }
        if Instant::now() >= deadline {
            return fail(format!(
                "process tree did not report three PIDs; saw {contents:?}"
            ));
        }
        time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_file(path: &Path) -> SpikeResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        if Instant::now() >= deadline {
            return fail(format!("timed out waiting for {}", path.display()));
        }
        time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

async fn require_all_gone(processes: &[(u32, ProcessIdentity)]) -> SpikeResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut running = Vec::new();
        for (pid, identity) in processes {
            if observe_identity(*pid, *identity)? == IdentityObservation::Match
                && process_running(*pid)?
            {
                running.push(*pid);
            }
        }
        if running.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return fail(format!(
                "managed descendants survived cancellation: {running:?}"
            ));
        }
        time::sleep(Duration::from_millis(20)).await;
    }
}

pub async fn run_spike() -> SpikeResult {
    println!("[process] Job Object cancellation, identity, nesting, and kill-on-drop");
    let temp = TempDir::new("prism-windows-process")?;
    let pid_file = temp.path().join("tree.pids");
    let marker = temp.path().join("graceful.marker");
    let tree = managed_tree(&pid_file, &marker)?;
    let root_pid = tree.id().ok_or("managed root did not expose a PID")?;
    let pids = wait_for_pids(&pid_file).await?;
    require(
        pids.contains(&root_pid),
        "reported PIDs omit the managed root",
    )?;
    let processes = pids
        .iter()
        .map(|pid| {
            process_creation_time(*pid)?
                .map(|identity| (*pid, identity))
                .ok_or_else(|| {
                    format!("managed process {pid} vanished before identity query").into()
                })
        })
        .collect::<SpikeResult<Vec<_>>>()?;

    let identity = processes
        .iter()
        .find_map(|(pid, identity)| (*pid == root_pid).then_some(*identity))
        .ok_or("managed root identity was not recorded")?;
    require(
        observe_identity(root_pid, identity)? == IdentityObservation::Match,
        "recorded process identity did not match",
    )?;
    require(
        observe_identity(root_pid, ProcessIdentity(identity.0.wrapping_add(1)))?
            == IdentityObservation::Reused,
        "simulated stale process identity was not rejected",
    )?;

    signal_process_group(root_pid).await?;
    wait_for_file(&marker).await?;
    time::sleep(Duration::from_millis(250)).await;
    require(
        !process_running(root_pid)?,
        "gracefully cancelled root remained alive",
    )?;
    let mut stubborn_descendants = Vec::new();
    for (pid, identity) in processes
        .iter()
        .copied()
        .filter(|(pid, _)| *pid != root_pid)
    {
        if observe_identity(pid, identity)? == IdentityObservation::Match && process_running(pid)? {
            stubborn_descendants.push(pid);
        }
    }
    require(
        stubborn_descendants.len() == 2,
        format!(
            "expected two stubborn descendants before escalation, saw {stubborn_descendants:?}"
        ),
    )?;

    tree.terminate()?;
    require_all_gone(&processes).await?;
    drop(tree);

    let drop_pid_file = temp.path().join("drop-tree.pids");
    let drop_marker = temp.path().join("drop.marker");
    let drop_tree = managed_tree(&drop_pid_file, &drop_marker)?;
    let drop_processes = wait_for_pids(&drop_pid_file)
        .await?
        .into_iter()
        .map(|pid| {
            process_creation_time(pid)?
                .map(|identity| (pid, identity))
                .ok_or_else(|| {
                    format!("drop-test process {pid} vanished before identity query").into()
                })
        })
        .collect::<SpikeResult<Vec<_>>>()?;
    drop(drop_tree);
    require_all_gone(&drop_processes).await?;

    println!("[process] PASS");
    Ok(())
}
