#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum SupportedOs {
    Linux,
    MacOs,
    Windows,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum DesktopNotificationPolicy {
    NativeWorker,
    TerminalSubscriber,
    Unavailable,
}

#[cfg(any(unix, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CommandCandidate<'a> {
    pub program: &'a str,
    pub args: &'a [&'a str],
}

#[cfg(any(unix, test))]
const LINUX_BROWSER_CANDIDATES: &[CommandCandidate<'static>] = &[
    CommandCandidate {
        program: "xdg-open",
        args: &[],
    },
    CommandCandidate {
        program: "gio",
        args: &["open"],
    },
    CommandCandidate {
        program: "wslview",
        args: &[],
    },
];

#[cfg(any(unix, test))]
const MACOS_BROWSER_CANDIDATES: &[CommandCandidate<'static>] = &[CommandCandidate {
    program: "open",
    args: &[],
}];

// Retained as a tested fallback policy. Production Windows opening uses ShellExecuteW directly.
#[cfg(any(unix, test))]
const WINDOWS_BROWSER_CANDIDATES: &[CommandCandidate<'static>] = &[CommandCandidate {
    program: "explorer.exe",
    args: &[],
}];

#[cfg(target_os = "linux")]
pub(crate) const fn current_os() -> SupportedOs {
    SupportedOs::Linux
}

#[cfg(target_os = "macos")]
pub(crate) const fn current_os() -> SupportedOs {
    SupportedOs::MacOs
}

#[cfg(target_os = "windows")]
pub(crate) const fn current_os() -> SupportedOs {
    SupportedOs::Windows
}

pub(crate) const fn default_session_runtime(os: SupportedOs) -> &'static str {
    match os {
        SupportedOs::Linux | SupportedOs::MacOs => "tmux",
        SupportedOs::Windows => "psmux.exe",
    }
}

pub(crate) const fn default_worktrunk_command(os: SupportedOs) -> &'static str {
    match os {
        SupportedOs::Linux | SupportedOs::MacOs => "wt",
        // Worktrunk publishes this alias specifically to avoid Windows Terminal's wt.exe.
        SupportedOs::Windows => "git-wt.exe",
    }
}

#[allow(dead_code)]
pub(crate) const fn desktop_notification_policy(os: SupportedOs) -> DesktopNotificationPolicy {
    match os {
        SupportedOs::Linux => DesktopNotificationPolicy::NativeWorker,
        SupportedOs::MacOs => DesktopNotificationPolicy::TerminalSubscriber,
        SupportedOs::Windows => DesktopNotificationPolicy::Unavailable,
    }
}

#[cfg(any(unix, test))]
pub(crate) const fn browser_candidates(os: SupportedOs) -> &'static [CommandCandidate<'static>] {
    match os {
        SupportedOs::Linux => LINUX_BROWSER_CANDIDATES,
        SupportedOs::MacOs => MACOS_BROWSER_CANDIDATES,
        SupportedOs::Windows => WINDOWS_BROWSER_CANDIDATES,
    }
}

#[cfg(windows)]
pub(crate) fn open_url_with_shell_execute(url: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;

    if url.contains('\0') {
        return Err("browser URL contains a NUL byte".to_string());
    }
    let verb = "open"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let url = std::ffi::OsStr::new(url)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: verb and URL are terminated immutable strings. No command shell is involved.
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(url.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = result.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(format!(
            "Windows browser open failed with ShellExecuteW code {code}"
        ))
    }
}
