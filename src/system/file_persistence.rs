//! Durable replacement for Prism-managed TOML files.
//!
//! On Unix, workspace, UI, user, and repository config files follow a final symlink so
//! user-managed config links retain their identity. Windows rejects final reparse points because
//! a pathname replacement cannot safely preserve that contract across junction races. Writers
//! serialize on a permanent adjacent `.lock` file.

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub type BoxError = Box<dyn Error + Send + Sync + 'static>;
pub use crate::durability::DurabilityIntent;

#[derive(Clone, Copy, Debug)]
pub struct UpdateOptions {
    pub durability: DurabilityIntent,
    pub lock_timeout: Duration,
}

impl UpdateOptions {
    pub const fn important_toml() -> Self {
        Self {
            durability: DurabilityIntent::Maximum,
            lock_timeout: Duration::from_millis(250),
        }
    }

    pub const fn ui_state() -> Self {
        Self {
            durability: DurabilityIntent::Standard,
            lock_timeout: Duration::from_millis(100),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    CreateParent,
    ResolveFinalSymlink,
    OpenLock,
    AcquireLock,
    Read,
    Transform,
    InspectPermissions,
    CreateStaging,
    Write,
    ApplyPermissions,
    SyncFile,
    FullSyncFile,
    Rename,
    SyncCommittedFile,
    SyncParent,
}

impl fmt::Display for Stage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.description())
    }
}

impl Stage {
    pub const fn label(self) -> &'static str {
        match self {
            Self::CreateParent => "create_parent",
            Self::ResolveFinalSymlink => "resolve_final_symlink",
            Self::OpenLock => "open_lock",
            Self::AcquireLock => "acquire_lock",
            Self::Read => "read",
            Self::Transform => "transform",
            Self::InspectPermissions => "inspect_permissions",
            Self::CreateStaging => "create_staging",
            Self::Write => "write",
            Self::ApplyPermissions => "apply_permissions",
            Self::SyncFile => "sync_file",
            Self::FullSyncFile => "full_sync_file",
            Self::Rename => "rename",
            Self::SyncCommittedFile => "sync_committed_file",
            Self::SyncParent => "sync_parent",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::CreateParent => "create parent directory",
            Self::ResolveFinalSymlink => "resolve final symlink",
            Self::OpenLock => "open lock file",
            Self::AcquireLock => "acquire lock",
            Self::Read => "read",
            Self::Transform => "transform",
            Self::InspectPermissions => "inspect permissions",
            Self::CreateStaging => "create staging file",
            Self::Write => "write staging file",
            Self::ApplyPermissions => "apply final permissions",
            Self::SyncFile => "sync staging file",
            Self::FullSyncFile => "fully sync staging file",
            Self::Rename => "rename staging file",
            Self::SyncCommittedFile => "sync committed file",
            Self::SyncParent => "sync parent directory",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistenceErrorKind {
    Contention,
    InvalidData,
    Io,
    Unsupported,
}

impl PersistenceErrorKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Contention => "contention",
            Self::InvalidData => "invalid_data",
            Self::Io => "io",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug)]
pub struct PersistenceError {
    kind: PersistenceErrorKind,
    stage: Stage,
    path: PathBuf,
    committed: bool,
    durability: Option<DurabilityIntent>,
    source: BoxError,
}

impl PersistenceError {
    pub fn kind(&self) -> PersistenceErrorKind {
        self.kind
    }

    pub fn stage(&self) -> Stage {
        self.stage
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn committed(&self) -> bool {
        self.committed
    }

    pub fn durability(&self) -> Option<DurabilityIntent> {
        self.durability
    }

    fn new(
        stage: Stage,
        path: impl Into<PathBuf>,
        committed: bool,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self::new_with_kind(
            persistence_error_kind(stage),
            stage,
            path,
            committed,
            source,
        )
    }

    fn new_with_kind(
        kind: PersistenceErrorKind,
        stage: Stage,
        path: impl Into<PathBuf>,
        committed: bool,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            stage,
            path: path.into(),
            committed,
            durability: None,
            source: Box::new(source),
        }
    }

    fn boxed(stage: Stage, path: impl Into<PathBuf>, committed: bool, source: BoxError) -> Self {
        Self {
            kind: persistence_error_kind(stage),
            stage,
            path: path.into(),
            committed,
            durability: None,
            source,
        }
    }
}

const fn persistence_error_kind(stage: Stage) -> PersistenceErrorKind {
    match stage {
        Stage::Transform => PersistenceErrorKind::InvalidData,
        Stage::CreateParent
        | Stage::ResolveFinalSymlink
        | Stage::OpenLock
        | Stage::AcquireLock
        | Stage::Read
        | Stage::InspectPermissions
        | Stage::CreateStaging
        | Stage::Write
        | Stage::ApplyPermissions
        | Stage::SyncFile
        | Stage::FullSyncFile
        | Stage::Rename
        | Stage::SyncCommittedFile
        | Stage::SyncParent => PersistenceErrorKind::Io,
    }
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {}: {}",
            self.stage,
            self.path.display(),
            self.source
        )?;
        if self.committed {
            formatter.write_str(" (replacement was already committed)")?;
        }
        Ok(())
    }
}

impl Error for PersistenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Debug)]
pub enum FileContents {
    Missing,
    Present(Vec<u8>),
}

impl FileContents {
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Missing => None,
            Self::Present(bytes) => Some(bytes),
        }
    }
}

#[derive(Debug)]
struct LockTimeout {
    timeout: Duration,
}

impl fmt::Display for LockTimeout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "file remained locked for {:?}", self.timeout)
    }
}

impl Error for LockTimeout {}

struct StagingGuard {
    path: PathBuf,
    renamed: bool,
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if !self.renamed {
            remove_staging_best_effort(&self.path);
        }
    }
}

#[cfg(unix)]
fn remove_staging_best_effort(path: &Path) {
    let _ = fs::remove_file(path);
}

#[cfg(windows)]
fn remove_staging_best_effort(path: &Path) {
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        match fs::remove_file(path) {
            Ok(()) => return,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error)
                if matches!(error.raw_os_error(), Some(5 | 32 | 33))
                    && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return,
        }
    }
}

pub fn update<T>(
    path: &Path,
    options: UpdateOptions,
    transform: impl FnOnce(FileContents) -> Result<(T, Option<Vec<u8>>), BoxError>,
) -> Result<T, PersistenceError> {
    update_with_fault(path, options, transform, |_| Ok(()))
}

fn update_with_fault<T>(
    path: &Path,
    options: UpdateOptions,
    transform: impl FnOnce(FileContents) -> Result<(T, Option<Vec<u8>>), BoxError>,
    fault: impl FnMut(Stage) -> io::Result<()>,
) -> Result<T, PersistenceError> {
    let result = update_inner_with_fault(path, options, transform, fault);
    match result {
        Ok((value, committed)) => {
            crate::observability::emit_deferred(crate::observability::EventInput {
                level: crate::observability::LogLevel::Debug,
                target: "atomic_write",
                action: "terminal",
                operation_id: None,
                parent_operation_id: None,
                branch: None,
                session: None,
                message: if committed {
                    "atomic replacement committed".to_string()
                } else {
                    "atomic replacement not needed".to_string()
                },
                data_json: Some(crate::observability::persistence_data_json(
                    if committed { "success" } else { "no_change" },
                    if committed {
                        "sync_parent"
                    } else {
                        "transform"
                    },
                    committed,
                    options.durability.label(),
                    None,
                )),
            });
            Ok(value)
        }
        Err(mut error) => {
            error.durability = Some(options.durability);
            crate::observability::emit_deferred(crate::observability::EventInput {
                level: crate::observability::LogLevel::Error,
                target: "atomic_write",
                action: "terminal",
                operation_id: None,
                parent_operation_id: None,
                branch: None,
                session: None,
                message: format!(
                    "atomic replacement failed: category={}, stage={}",
                    error.kind.label(),
                    error.stage.label()
                ),
                data_json: Some(crate::observability::persistence_data_json(
                    error.kind.label(),
                    error.stage.label(),
                    error.committed,
                    options.durability.label(),
                    Some(&error.to_string()),
                )),
            });
            Err(error)
        }
    }
}

fn update_inner_with_fault<T>(
    path: &Path,
    options: UpdateOptions,
    transform: impl FnOnce(FileContents) -> Result<(T, Option<Vec<u8>>), BoxError>,
    mut fault: impl FnMut(Stage) -> io::Result<()>,
) -> Result<(T, bool), PersistenceError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| PersistenceError::new(Stage::CreateParent, parent, false, error))?;
    #[cfg(windows)]
    if parent != Path::new(".") {
        crate::system::windows_security::secure_path(parent, true)
            .map_err(|error| PersistenceError::new(Stage::CreateParent, parent, false, error))?;
    }

    let target = resolve_final_symlink(path)?;
    let target_parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    #[cfg(windows)]
    if target.exists() {
        crate::system::windows_security::secure_path(&target, false).map_err(|error| {
            PersistenceError::new(Stage::InspectPermissions, &target, false, error)
        })?;
    }
    let lock_path = adjacent_lock_path(&target);
    let lock = open_lock(&lock_path)?;
    acquire_lock(&lock, &lock_path, options.lock_timeout)?;

    let contents = match fs::read(&target) {
        Ok(bytes) => FileContents::Present(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => FileContents::Missing,
        Err(error) => return Err(PersistenceError::new(Stage::Read, &target, false, error)),
    };
    let permissions = match fs::metadata(&target) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(PersistenceError::new(
                Stage::InspectPermissions,
                &target,
                false,
                error,
            ));
        }
    };
    let (value, replacement) = transform(contents)
        .map_err(|error| PersistenceError::boxed(Stage::Transform, &target, false, error))?;
    let Some(replacement) = replacement else {
        return Ok((value, false));
    };

    let (mut staging_file, staging_path) = create_staging(&target, target_parent)?;
    let mut staging = StagingGuard {
        path: staging_path.clone(),
        renamed: false,
    };
    staging_file
        .write_all(&replacement)
        .map_err(|error| PersistenceError::new(Stage::Write, &staging_path, false, error))?;
    fault(Stage::Write)
        .map_err(|error| PersistenceError::new(Stage::Write, &staging_path, false, error))?;
    if let Some(permissions) = permissions {
        staging_file.set_permissions(permissions).map_err(|error| {
            PersistenceError::new(Stage::ApplyPermissions, &staging_path, false, error)
        })?;
    }
    crate::durability::sync_file(&staging_file, options.durability).map_err(|error| {
        let stage = match error.stage() {
            crate::durability::FileSyncStage::File => Stage::SyncFile,
            crate::durability::FileSyncStage::FullFile => Stage::FullSyncFile,
        };
        let source = error.into_source();
        let kind = if source.kind() == io::ErrorKind::Unsupported {
            PersistenceErrorKind::Unsupported
        } else {
            PersistenceErrorKind::Io
        };
        PersistenceError::new_with_kind(kind, stage, &staging_path, false, source)
    })?;
    fault(Stage::SyncFile)
        .map_err(|error| PersistenceError::new(Stage::SyncFile, &staging_path, false, error))?;
    commit_staging(&staging_path, &target)
        .map_err(|error| PersistenceError::new(Stage::Rename, &target, false, error))?;
    staging.renamed = true;
    fault(Stage::Rename)
        .map_err(|error| PersistenceError::new(Stage::Rename, &target, true, error))?;
    // On Windows this is the explicit FlushFileBuffers after ReplaceFileW/MoveFileExW.
    // Keeping the staging handle open also works when preserved attributes make the new path
    // read-only immediately after replacement.
    staging_file
        .sync_all()
        .map_err(|error| PersistenceError::new(Stage::SyncCommittedFile, &target, true, error))?;
    fault(Stage::SyncCommittedFile)
        .map_err(|error| PersistenceError::new(Stage::SyncCommittedFile, &target, true, error))?;
    drop(staging_file);
    crate::durability::sync_directory(target_parent, options.durability).map_err(|error| {
        let kind = if error.kind() == io::ErrorKind::Unsupported {
            PersistenceErrorKind::Unsupported
        } else {
            PersistenceErrorKind::Io
        };
        PersistenceError::new_with_kind(kind, Stage::SyncParent, target_parent, true, error)
    })?;
    fault(Stage::SyncParent)
        .map_err(|error| PersistenceError::new(Stage::SyncParent, target_parent, true, error))?;
    Ok((value, true))
}

fn resolve_final_symlink(path: &Path) -> Result<PathBuf, PersistenceError> {
    match fs::symlink_metadata(path) {
        #[cfg(unix)]
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(path)
            .map_err(|error| PersistenceError::new(Stage::ResolveFinalSymlink, path, false, error)),
        #[cfg(windows)]
        Ok(_) => {
            crate::system::windows_security::reject_reparse_point(path).map_err(|error| {
                PersistenceError::new_with_kind(
                    PersistenceErrorKind::Unsupported,
                    Stage::ResolveFinalSymlink,
                    path,
                    false,
                    error,
                )
            })?;
            Ok(path.to_path_buf())
        }
        #[cfg(unix)]
        Ok(_) => Ok(path.to_path_buf()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => Err(PersistenceError::new(
            Stage::ResolveFinalSymlink,
            path,
            false,
            error,
        )),
    }
}

pub fn adjacent_lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

fn open_lock(path: &Path) -> Result<File, PersistenceError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options.mode(0o600);
    #[cfg(windows)]
    options
        .access_mode(
            windows::Win32::Foundation::GENERIC_READ.0
                | windows::Win32::Foundation::GENERIC_WRITE.0
                | windows::Win32::Storage::FileSystem::READ_CONTROL.0
                | windows::Win32::Storage::FileSystem::WRITE_DAC.0,
        )
        .custom_flags(windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT.0);
    let file = options
        .open(path)
        .map_err(|error| PersistenceError::new(Stage::OpenLock, path, false, error))?;
    #[cfg(windows)]
    crate::system::windows_security::secure_file(&file, false)
        .map_err(|error| PersistenceError::new(Stage::OpenLock, path, false, error))?;
    Ok(file)
}

fn acquire_lock(file: &File, path: &Path, timeout: Duration) -> Result<(), PersistenceError> {
    let started = Instant::now();
    loop {
        #[cfg(unix)]
        let result = file.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => None,
            fs::TryLockError::Error(error) => Some(error),
        });
        #[cfg(windows)]
        let result = fs4::FileExt::try_lock(file).map_err(|error| match error {
            fs4::TryLockError::WouldBlock => None,
            fs4::TryLockError::Error(error) => Some(error),
        });
        match result {
            Ok(()) => return Ok(()),
            Err(None) => {
                if started.elapsed() >= timeout {
                    return Err(PersistenceError::new_with_kind(
                        PersistenceErrorKind::Contention,
                        Stage::AcquireLock,
                        path,
                        false,
                        LockTimeout { timeout },
                    ));
                }
                thread::sleep(Duration::from_millis(5).min(timeout));
            }
            Err(Some(error)) => {
                return Err(PersistenceError::new(
                    Stage::AcquireLock,
                    path,
                    false,
                    error,
                ));
            }
        }
    }
}

fn create_staging(target: &Path, parent: &Path) -> Result<(File, PathBuf), PersistenceError> {
    let target_name = target.file_name().unwrap_or_default();
    for _ in 0..100 {
        let mut name = OsString::from(".");
        name.push(target_name);
        name.push(format!(
            ".tmp-{}-{}",
            std::process::id(),
            STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let path = parent.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        #[cfg(windows)]
        options
            .access_mode(
                windows::Win32::Foundation::GENERIC_WRITE.0
                    | windows::Win32::Storage::FileSystem::READ_CONTROL.0
                    | windows::Win32::Storage::FileSystem::WRITE_DAC.0,
            )
            .custom_flags(windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT.0);
        match options.open(&path) {
            Ok(file) => {
                #[cfg(windows)]
                crate::system::windows_security::secure_file(&file, false).map_err(|error| {
                    PersistenceError::new(Stage::CreateStaging, &path, false, error)
                })?;
                return Ok((file, path));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(PersistenceError::new(
                    Stage::CreateStaging,
                    path,
                    false,
                    error,
                ));
            }
        }
    }
    Err(PersistenceError::new(
        Stage::CreateStaging,
        target,
        false,
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique staging file",
        ),
    ))
}

#[cfg(unix)]
fn commit_staging(staging: &Path, target: &Path) -> io::Result<()> {
    fs::rename(staging, target)
}

#[cfg(windows)]
fn commit_staging(staging: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, ReplaceFileW, MOVEFILE_WRITE_THROUGH, REPLACE_FILE_FLAGS,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let staging = wide(staging);
    let target = wide(target);
    if unsafe {
        ReplaceFileW(
            PCWSTR(target.as_ptr()),
            PCWSTR(staging.as_ptr()),
            PCWSTR::null(),
            REPLACE_FILE_FLAGS(0),
            None,
            None,
        )
    }
    .is_ok()
    {
        return Ok(());
    }
    let replace_error = io::Error::last_os_error();
    if !matches!(replace_error.raw_os_error(), Some(2 | 3)) {
        return Err(replace_error);
    }
    // ReplaceFileW requires an existing destination. MoveFileExW commits the first generation
    // without creating a missing-destination interval.
    unsafe {
        MoveFileExW(
            PCWSTR(staging.as_ptr()),
            PCWSTR(target.as_ptr()),
            MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs::Permissions;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn platform_smoke_native_persistence_pre_rename_failure_preserves_old_file() {
        let _ = crate::observability::take_captured_events();
        let dir = temp_dir("failure");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, "value = 'old'\n").unwrap();

        let error = update_with_fault(
            &path,
            UpdateOptions::important_toml(),
            |_| Ok(((), Some(b"value = 'new'\n".to_vec()))),
            |stage| {
                if stage == Stage::SyncFile {
                    Err(io::Error::other("injected sync failure"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();

        assert_eq!(error.stage(), Stage::SyncFile);
        assert_eq!(error.kind(), PersistenceErrorKind::Io);
        assert!(!error.committed());
        assert_eq!(
            error.durability(),
            Some(UpdateOptions::important_toml().durability)
        );
        assert!(error.source().is_some());
        let event = crate::observability::take_captured_events()
            .into_iter()
            .filter(|event| event.target == "atomic_write" && event.action == "terminal")
            .filter_map(|event| event.data_json)
            .map(|data| serde_json::from_str::<serde_json::Value>(&data).unwrap())
            .find(|data| {
                data["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("injected sync failure"))
            })
            .unwrap();
        assert_eq!(event["category"], "io");
        assert_eq!(event["stage"], "sync_file");
        assert_eq!(event["committed"], false);
        assert_eq!(
            event["durability"],
            UpdateOptions::important_toml().durability.label()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "value = 'old'\n");
        let staging_count = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(staging_count, 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parent_sync_failure_reports_committed_replacement() {
        let dir = temp_dir("parent-sync");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, "value = 'old'\n").unwrap();

        let error = update_with_fault(
            &path,
            UpdateOptions::important_toml(),
            |_| Ok(((), Some(b"value = 'new'\n".to_vec()))),
            |stage| {
                if stage == Stage::SyncParent {
                    Err(io::Error::other("injected parent sync failure"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();

        assert_eq!(error.stage(), Stage::SyncParent);
        assert_eq!(error.kind(), PersistenceErrorKind::Io);
        assert!(error.committed());
        assert_eq!(
            error.durability(),
            Some(UpdateOptions::important_toml().durability)
        );
        assert_eq!(error.path(), dir.as_path());
        assert_eq!(fs::read_to_string(&path).unwrap(), "value = 'new'\n");
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_atomic_replace_changes_identity_and_keeps_old_open_handle() {
        use std::io::{Read, Seek};

        let dir = temp_dir("windows-identity");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.toml");
        fs::write(&path, "value = 'old'\n").unwrap();
        let old_identity = file_id::get_file_id(&path).unwrap();
        let mut old_handle = File::open(&path).unwrap();

        update(&path, UpdateOptions::important_toml(), |_| {
            Ok(((), Some(b"value = 'new'\n".to_vec())))
        })
        .unwrap();

        assert_ne!(file_id::get_file_id(&path).unwrap(), old_identity);
        assert_eq!(fs::read_to_string(&path).unwrap(), "value = 'new'\n");
        old_handle.rewind().unwrap();
        let mut old = String::new();
        old_handle.read_to_string(&mut old).unwrap();
        assert_eq!(old, "value = 'old'\n");
        drop(old_handle);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bounded_lock_contention_returns_at_acquire_stage_and_keeps_lock_file() {
        let dir = temp_dir("lock");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let lock_path = adjacent_lock_path(&path);
        let lock = open_lock(&lock_path).unwrap();
        lock.try_lock().unwrap();
        let mut options = UpdateOptions::important_toml();
        options.lock_timeout = Duration::from_millis(20);

        let error = update(&path, options, |_| Ok(((), Some(Vec::new())))).unwrap_err();

        assert_eq!(error.stage(), Stage::AcquireLock);
        assert_eq!(error.kind(), PersistenceErrorKind::Contention);
        assert!(!error.committed());
        assert_eq!(error.durability(), Some(options.durability));
        drop(lock);
        assert!(lock_path.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn replacement_preserves_mode_and_final_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = temp_dir("symlink");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("managed.toml");
        let link = dir.join("config.toml");
        fs::write(&target, "value = 'old'\n").unwrap();
        fs::set_permissions(&target, Permissions::from_mode(0o640)).unwrap();
        symlink(&target, &link).unwrap();

        update(&link, UpdateOptions::important_toml(), |_| {
            Ok(((), Some(b"value = 'new'\n".to_vec())))
        })
        .unwrap();

        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "value = 'new'\n");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn dangling_final_symlink_reports_resolve_stage_and_preserves_link() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("dangling-symlink");
        fs::create_dir_all(&dir).unwrap();
        let link = dir.join("config.toml");
        symlink(dir.join("missing.toml"), &link).unwrap();

        let error = update(&link, UpdateOptions::important_toml(), |_| {
            Ok(((), Some(b"value = 'new'\n".to_vec())))
        })
        .unwrap_err();

        assert_eq!(error.stage(), Stage::ResolveFinalSymlink);
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn child_hard_exits_leave_complete_old_or_new_files() {
        for (stage, expected) in [
            ("write", "value = 'old'\n"),
            ("sync", "value = 'old'\n"),
            ("rename", "value = 'new'\n"),
        ] {
            let dir = temp_dir(stage);
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("config.toml");
            fs::write(&path, "value = 'old'\n").unwrap();
            let module = module_path!()
                .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
                .unwrap_or(module_path!());
            let helper = format!("{module}::atomic_crash_helper");
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", &helper])
                .env("PRISM_ATOMIC_CRASH_PATH", &path)
                .env("PRISM_ATOMIC_CRASH_STAGE", stage)
                .status()
                .unwrap();

            assert_eq!(status.code(), Some(75));
            let text = fs::read_to_string(&path).unwrap();
            assert_eq!(text, expected);
            toml::from_str::<toml::Value>(&text).unwrap();
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    #[ignore]
    fn atomic_crash_helper() {
        let Ok(path) = std::env::var("PRISM_ATOMIC_CRASH_PATH") else {
            return;
        };
        let stage = std::env::var("PRISM_ATOMIC_CRASH_STAGE").unwrap();
        let crash_stage = match stage.as_str() {
            "write" => Stage::Write,
            "sync" => Stage::SyncFile,
            "rename" => Stage::Rename,
            _ => panic!("unknown crash stage {stage}"),
        };
        let _ = update_with_fault(
            Path::new(&path),
            UpdateOptions::important_toml(),
            |_| Ok(((), Some(b"value = 'new'\n".to_vec()))),
            |stage| {
                if stage == crash_stage {
                    std::process::exit(75);
                }
                Ok(())
            },
        );
        panic!("fault was not reached");
    }

    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "prism-file-persistence-{label}-{}-{unique}",
            std::process::id()
        ))
    }
}
