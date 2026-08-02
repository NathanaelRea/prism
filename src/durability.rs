use std::fs::File;
use std::io;
use std::path::Path;

use crate::platform::SupportedOs;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurabilityIntent {
    Standard,
    Maximum,
}

impl DurabilityIntent {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Maximum => "maximum",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DurabilityPolicy {
    FileAndDirectory,
    MacOsFullSyncAndDirectory,
}

pub(crate) const fn policy_for(os: SupportedOs, intent: DurabilityIntent) -> DurabilityPolicy {
    match (os, intent) {
        (SupportedOs::MacOs, DurabilityIntent::Maximum) => {
            DurabilityPolicy::MacOsFullSyncAndDirectory
        }
        (SupportedOs::Linux | SupportedOs::MacOs, _) => DurabilityPolicy::FileAndDirectory,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileSyncStage {
    File,
    FullFile,
}

#[derive(Debug)]
pub(crate) struct FileSyncError {
    stage: FileSyncStage,
    source: io::Error,
}

impl FileSyncError {
    pub(crate) fn stage(&self) -> FileSyncStage {
        self.stage
    }

    pub(crate) fn into_source(self) -> io::Error {
        self.source
    }
}

pub(crate) fn sync_file(file: &File, intent: DurabilityIntent) -> Result<(), FileSyncError> {
    sync_file_for(file, intent, crate::platform::current_os())
}

fn sync_file_for(
    file: &File,
    intent: DurabilityIntent,
    os: SupportedOs,
) -> Result<(), FileSyncError> {
    file.sync_all().map_err(|source| FileSyncError {
        stage: FileSyncStage::File,
        source,
    })?;
    if policy_for(os, intent) == DurabilityPolicy::MacOsFullSyncAndDirectory {
        full_sync(file).map_err(|source| FileSyncError {
            stage: FileSyncStage::FullFile,
            source,
        })?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn full_sync(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
fn full_sync(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "F_FULLFSYNC is only available on macOS",
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sync_directory_native(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sync_directory_native(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "directory sync is not supported on this platform",
    ))
}

pub(crate) fn sync_directory(path: &Path, intent: DurabilityIntent) -> io::Result<()> {
    match policy_for(crate::platform::current_os(), intent) {
        DurabilityPolicy::FileAndDirectory | DurabilityPolicy::MacOsFullSyncAndDirectory => {
            sync_directory_native(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maximum_durability_selects_the_strongest_supported_policy() {
        assert_eq!(
            policy_for(SupportedOs::Linux, DurabilityIntent::Maximum),
            DurabilityPolicy::FileAndDirectory,
        );
        assert_eq!(
            policy_for(SupportedOs::MacOs, DurabilityIntent::Maximum),
            DurabilityPolicy::MacOsFullSyncAndDirectory,
        );
    }

    #[test]
    fn standard_durability_has_the_same_policy_on_supported_platforms() {
        for os in [SupportedOs::Linux, SupportedOs::MacOs] {
            assert_eq!(
                policy_for(os, DurabilityIntent::Standard),
                DurabilityPolicy::FileAndDirectory,
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unsupported_native_strong_sync_is_an_explicit_error() {
        let path = std::env::temp_dir().join(format!(
            "prism-durability-unsupported-{}",
            std::process::id()
        ));
        let file = File::create(&path).unwrap();

        let error = sync_file_for(&file, DurabilityIntent::Maximum, SupportedOs::MacOs)
            .unwrap_err()
            .into_source();

        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn platform_smoke_native_durability_syncs_file_and_directory() {
        let path =
            std::env::temp_dir().join(format!("prism-durability-smoke-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        let file_path = path.join("state");
        let file = File::create(&file_path).unwrap();

        sync_file(&file, DurabilityIntent::Maximum).unwrap();
        sync_directory(&path, DurabilityIntent::Maximum).unwrap();

        drop(file);
        std::fs::remove_dir_all(path).unwrap();
    }
}
