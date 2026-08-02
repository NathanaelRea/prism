#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum SupportedOs {
    Linux,
    MacOs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CommandCandidate<'a> {
    pub program: &'a str,
    pub args: &'a [&'a str],
}

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

const MACOS_BROWSER_CANDIDATES: &[CommandCandidate<'static>] = &[CommandCandidate {
    program: "open",
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

pub(crate) const fn browser_candidates(os: SupportedOs) -> &'static [CommandCandidate<'static>] {
    match os {
        SupportedOs::Linux => LINUX_BROWSER_CANDIDATES,
        SupportedOs::MacOs => MACOS_BROWSER_CANDIDATES,
    }
}
