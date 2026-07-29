//! Durable replacement for Prism-managed TOML files.
//!
//! Workspace, UI, user, and repository config files all follow a final symlink.
//! This preserves user-managed config symlinks while replacing the resolved target
//! atomically. Writers serialize on a permanent adjacent `.lock` file.

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

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub type BoxError = Box<dyn Error + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    FileAndDirectory,
    MacOsFullSync,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinalSymlink {
    Follow,
}

#[derive(Clone, Copy, Debug)]
pub struct UpdateOptions {
    pub durability: Durability,
    pub final_symlink: FinalSymlink,
    pub lock_timeout: Duration,
}

impl UpdateOptions {
    pub const fn important_toml() -> Self {
        Self {
            durability: Durability::MacOsFullSync,
            final_symlink: FinalSymlink::Follow,
            lock_timeout: Duration::from_millis(250),
        }
    }

    pub const fn ui_state() -> Self {
        Self {
            durability: Durability::FileAndDirectory,
            final_symlink: FinalSymlink::Follow,
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
    SyncParent,
}

impl fmt::Display for Stage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
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
            Self::SyncParent => "sync parent directory",
        };
        formatter.write_str(label)
    }
}

#[derive(Debug)]
pub struct PersistenceError {
    stage: Stage,
    path: PathBuf,
    committed: bool,
    source: BoxError,
}

impl PersistenceError {
    pub fn stage(&self) -> Stage {
        self.stage
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn committed(&self) -> bool {
        self.committed
    }

    fn new(
        stage: Stage,
        path: impl Into<PathBuf>,
        committed: bool,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            stage,
            path: path.into(),
            committed,
            source: Box::new(source),
        }
    }

    fn boxed(stage: Stage, path: impl Into<PathBuf>, committed: bool, source: BoxError) -> Self {
        Self {
            stage,
            path: path.into(),
            committed,
            source,
        }
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
            let _ = fs::remove_file(&self.path);
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
    mut fault: impl FnMut(Stage) -> io::Result<()>,
) -> Result<T, PersistenceError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| PersistenceError::new(Stage::CreateParent, parent, false, error))?;

    let target = resolve_final_symlink(path, options.final_symlink)?;
    let target_parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
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
        return Ok(value);
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
    staging_file
        .sync_all()
        .map_err(|error| PersistenceError::new(Stage::SyncFile, &staging_path, false, error))?;
    fault(Stage::SyncFile)
        .map_err(|error| PersistenceError::new(Stage::SyncFile, &staging_path, false, error))?;
    if options.durability == Durability::MacOsFullSync {
        full_sync(&staging_file).map_err(|error| {
            PersistenceError::new(Stage::FullSyncFile, &staging_path, false, error)
        })?;
    }
    drop(staging_file);
    fs::rename(&staging_path, &target)
        .map_err(|error| PersistenceError::new(Stage::Rename, &target, false, error))?;
    staging.renamed = true;
    fault(Stage::Rename)
        .map_err(|error| PersistenceError::new(Stage::Rename, &target, true, error))?;
    sync_parent(target_parent)
        .map_err(|error| PersistenceError::new(Stage::SyncParent, target_parent, true, error))?;
    fault(Stage::SyncParent)
        .map_err(|error| PersistenceError::new(Stage::SyncParent, target_parent, true, error))?;
    Ok(value)
}

fn resolve_final_symlink(path: &Path, behavior: FinalSymlink) -> Result<PathBuf, PersistenceError> {
    match behavior {
        FinalSymlink::Follow => match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                fs::canonicalize(path).map_err(|error| {
                    PersistenceError::new(Stage::ResolveFinalSymlink, path, false, error)
                })
            }
            Ok(_) => Ok(path.to_path_buf()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(path.to_path_buf()),
            Err(error) => Err(PersistenceError::new(
                Stage::ResolveFinalSymlink,
                path,
                false,
                error,
            )),
        },
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
    options
        .open(path)
        .map_err(|error| PersistenceError::new(Stage::OpenLock, path, false, error))
}

fn acquire_lock(file: &File, path: &Path, timeout: Duration) -> Result<(), PersistenceError> {
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(fs::TryLockError::WouldBlock) => {
                if started.elapsed() >= timeout {
                    return Err(PersistenceError::new(
                        Stage::AcquireLock,
                        path,
                        false,
                        LockTimeout { timeout },
                    ));
                }
                thread::sleep(Duration::from_millis(5).min(timeout));
            }
            Err(fs::TryLockError::Error(error)) => {
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
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
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
    Ok(())
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::Permissions;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn pre_rename_failure_leaves_old_file_and_removes_staging() {
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
        assert!(!error.committed());
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
        assert!(error.committed());
        assert_eq!(error.path(), dir.as_path());
        assert_eq!(fs::read_to_string(&path).unwrap(), "value = 'new'\n");
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
        assert!(!error.committed());
        drop(lock);
        assert!(lock_path.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn replacement_preserves_mode_and_final_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};

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

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "value = 'new'\n");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
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
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "file_persistence::tests::atomic_crash_helper",
                ])
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
                    unsafe { libc::_exit(75) };
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
