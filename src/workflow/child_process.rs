use std::io;
use std::process::ExitStatus;

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};

#[cfg(windows)]
use process_wrap::tokio::{ChildWrapper, CommandWrap, CreationFlags, JobObject, KillOnDrop};

#[cfg(unix)]
pub(super) type Child = tokio::process::Child;
#[cfg(windows)]
pub(super) type Child = Box<dyn ChildWrapper>;

pub(super) fn configure(command: &mut Command) {
    #[cfg(unix)]
    command.as_std_mut().process_group(0);
    #[cfg(windows)]
    let _ = command;
}

#[cfg(unix)]
pub(super) fn spawn(command: &mut Command) -> io::Result<Child> {
    command.spawn()
}

#[cfg(windows)]
pub(super) fn spawn(command: &mut Command) -> io::Result<Child> {
    use windows::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

    let owned = std::mem::replace(command, Command::new(""));
    let mut wrapped = CommandWrap::from(owned);
    wrapped.wrap(CreationFlags(CREATE_NEW_PROCESS_GROUP));
    wrapped.wrap(JobObject);
    wrapped.wrap(KillOnDrop);
    let result = wrapped.spawn();
    *command = wrapped.into_command();
    result
}

pub(super) fn id(child: &Child) -> Option<u32> {
    child.id()
}

pub(super) fn stdin(child: &mut Child) -> &mut Option<ChildStdin> {
    #[cfg(unix)]
    return &mut child.stdin;
    #[cfg(windows)]
    return child.stdin();
}

pub(super) fn stdout(child: &mut Child) -> &mut Option<ChildStdout> {
    #[cfg(unix)]
    return &mut child.stdout;
    #[cfg(windows)]
    return child.stdout();
}

pub(super) fn stderr(child: &mut Child) -> &mut Option<ChildStderr> {
    #[cfg(unix)]
    return &mut child.stderr;
    #[cfg(windows)]
    return child.stderr();
}

pub(super) async fn wait(child: &mut Child) -> io::Result<ExitStatus> {
    child.wait().await
}

#[cfg(unix)]
pub(super) async fn kill(child: &mut Child) -> io::Result<()> {
    child.kill().await
}

#[cfg(windows)]
pub(super) async fn kill(child: &mut Child) -> io::Result<()> {
    Box::into_pin(child.kill()).await
}

pub(super) async fn terminate(child: &mut Child) {
    #[cfg(unix)]
    if let Some(process_id) = id(child) {
        let _ = crate::system::process::send_process_group_signal(process_id, libc::SIGKILL);
    }
    let _ = kill(child).await;
}

pub(super) struct DropGuard {
    #[cfg(unix)]
    process_id: Option<u32>,
    armed: bool,
}

impl DropGuard {
    pub(super) fn new(_child: &Child) -> Self {
        Self {
            #[cfg(unix)]
            process_id: id(_child),
            armed: true,
        }
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.armed
            && let Some(process_id) = self.process_id
        {
            let _ = crate::system::process::send_process_group_signal(process_id, libc::SIGKILL);
        }
        #[cfg(windows)]
        let _ = self.armed;
        // On Windows, KillOnDrop owns the Job Object and terminates its process tree when Child
        // is dropped. The guard only tracks whether normal completion was observed.
    }
}
