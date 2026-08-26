use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use sqlx::{Connection as _, SqliteConnection};

use crate::persistence::{database, pools};
use crate::repo::prism_repo_dir;
use crate::workspace;
use crate::{DurableWorkflowRunStore, async_runtime};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DevStateSummary {
    pub(crate) repository_databases: usize,
    pub(crate) copied_repository_databases: usize,
    pub(crate) copied_workflow_database: bool,
    pub(crate) removed_nonterminal_workflows: u64,
}

pub(crate) struct DevInstallReaderLease {
    path: Option<PathBuf>,
}

impl Drop for DevInstallReaderLease {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_dir(path);
        }
    }
}

pub(crate) fn reader_lease_from_environment() -> Result<DevInstallReaderLease, String> {
    let Some(path) = std::env::var_os("PRISM_DEV_READER_PATH")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
    else {
        return Ok(DevInstallReaderLease { path: None });
    };
    let process_id = std::process::id().to_string();
    if path.file_name() != Some(OsStr::new(&process_id)) {
        return Ok(DevInstallReaderLease { path: None });
    }
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("inspect prism-dev reader lease {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "prism-dev reader lease is not a directory: {}",
            path.display()
        ));
    }
    Ok(DevInstallReaderLease { path: Some(path) })
}

#[derive(Clone, Copy)]
enum CopyScope {
    Global,
    Repository,
}

pub(crate) fn prepare(source: &Path, destination: &Path) -> Result<DevStateSummary, String> {
    validate_roots(source, destination)?;
    let destination_created = prepare_empty_destination(destination)?;
    if let Err(error) = validate_roots(source, destination) {
        if destination_created {
            fs::remove_dir(destination).map_err(|cleanup_error| {
                format!(
                    "{error}; remove rejected development state destination {}: {cleanup_error}",
                    destination.display()
                )
            })?;
        }
        return Err(error);
    }

    if source.exists() {
        copy_tree(source, destination, CopyScope::Global, Path::new(""))?;
    }

    let entries = workspace::load_entries_from_path(&source.join("repos.toml"))?;
    let mut copied_repository_databases = 0;
    for entry in &entries {
        // Repository discovery resolves path aliases (for example, macOS /var to
        // /private/var), so use the same physical root when locating its state.
        let storage_root = fs::canonicalize(&entry.root).unwrap_or_else(|_| entry.root.clone());
        let source_repo_dir = prism_repo_dir(&storage_root, source);
        let destination_repo_dir = prism_repo_dir(&storage_root, destination);
        if source_repo_dir.exists() {
            copy_tree(
                &source_repo_dir,
                &destination_repo_dir,
                CopyScope::Repository,
                Path::new(""),
            )?;
        } else {
            fs::create_dir_all(&destination_repo_dir)
                .map_err(|error| format!("create {}: {error}", destination_repo_dir.display()))?;
        }

        let source_database = source_repo_dir.join("prism.db");
        let destination_database = destination_repo_dir.join("prism.db");
        if snapshot_database(&source_database, &destination_database)? {
            copied_repository_databases += 1;
        }
        database::initialize(&destination_database).map_err(|error| {
            format!(
                "migrate development repository database {}: {error}",
                destination_database.display()
            )
        })?;
    }

    let source_workflow_database = source.join("workflow.db");
    let destination_workflow_database = destination.join("workflow.db");
    let copied_workflow_database =
        snapshot_database(&source_workflow_database, &destination_workflow_database)?;
    let removed_nonterminal_workflows = prepare_workflow_database(&destination_workflow_database)?;
    set_owner_only(&destination_workflow_database)?;

    set_owner_only(destination)?;
    Ok(DevStateSummary {
        repository_databases: entries.len(),
        copied_repository_databases,
        copied_workflow_database,
        removed_nonterminal_workflows,
    })
}

fn validate_roots(source: &Path, destination: &Path) -> Result<(), String> {
    if source == destination {
        return Err("development state source and destination must differ".to_string());
    }
    if source.exists() && destination.exists() {
        let source = fs::canonicalize(source)
            .map_err(|error| format!("resolve {}: {error}", source.display()))?;
        let destination = fs::canonicalize(destination)
            .map_err(|error| format!("resolve {}: {error}", destination.display()))?;
        if destination == source || destination.starts_with(&source) {
            return Err(format!(
                "development state destination {} must not be inside source {}",
                destination.display(),
                source.display()
            ));
        }
    }
    Ok(())
}

fn prepare_empty_destination(destination: &Path) -> Result<bool, String> {
    if let Some(parent) = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", destination.display()))?;
    }
    let created = match fs::create_dir(destination) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            return Err(format!("create {}: {error}", destination.display()));
        }
    };
    let mut entries = fs::read_dir(destination)
        .map_err(|error| format!("read {}: {error}", destination.display()))?;
    if entries
        .next()
        .transpose()
        .map_err(|error| {
            format!(
                "read development state destination {}: {error}",
                destination.display()
            )
        })?
        .is_some()
    {
        return Err(format!(
            "development state destination is not empty: {}",
            destination.display()
        ));
    }
    set_owner_only(destination)?;
    Ok(created)
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    scope: CopyScope,
    relative: &Path,
) -> Result<(), String> {
    fs::create_dir_all(destination)
        .map_err(|error| format!("create {}: {error}", destination.display()))?;
    let entries =
        fs::read_dir(source).map_err(|error| format!("read {}: {error}", source.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read {}: {error}", source.display()))?;
        let name = entry.file_name();
        let child_relative = relative.join(&name);
        if should_skip(scope, &child_relative) {
            continue;
        }
        let source_path = entry.path();
        let destination_path = destination.join(&name);
        let metadata = fs::symlink_metadata(&source_path)
            .map_err(|error| format!("inspect {}: {error}", source_path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "development state snapshot does not follow symlink {}",
                source_path.display()
            ));
        } else if metadata.is_dir() {
            copy_tree(&source_path, &destination_path, scope, &child_relative)?;
        } else if metadata.is_file() {
            copy_file(&source_path, &destination_path)?;
        }
    }
    Ok(())
}

fn copy_file(source: &Path, destination: &Path) -> Result<(), String> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    fs::copy(source, destination).map_err(|error| {
        format!(
            "copy development state {} to {}: {error}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn should_skip(scope: CopyScope, relative: &Path) -> bool {
    let name = relative
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let depth = relative.components().count();

    if name != "package.lock"
        && (name.ends_with(".lock")
            || name.ends_with(".pid")
            || name.ends_with(".sock")
            || name.ends_with("-wal")
            || name.ends_with("-shm")
            || name.ends_with("-journal"))
    {
        return true;
    }

    match scope {
        CopyScope::Global => {
            depth == 1 && (matches!(name, "repos" | "server") || name.starts_with("workflow.db"))
        }
        CopyScope::Repository => {
            (depth == 1 && matches!(name, "logs" | "recordings" | "run-markers"))
                || name.starts_with("prism.db")
                || name.starts_with("runtime.log")
        }
    }
}

fn snapshot_database(source: &Path, destination: &Path) -> Result<bool, String> {
    if !source.exists() {
        return Ok(false);
    }
    let source_metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("inspect {}: {error}", source.display()))?;
    if source_metadata.file_type().is_symlink() {
        return Err(format!(
            "development database snapshot does not follow symlink {}",
            source.display()
        ));
    }
    if !source_metadata.is_file() {
        return Err(format!(
            "development database source is not a regular file: {}",
            source.display()
        ));
    }
    if destination.exists() {
        return Err(format!(
            "development database destination already exists: {}",
            destination.display()
        ));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }

    let source = source.to_path_buf();
    let destination = destination.to_path_buf();
    let snapshot_destination = destination.clone();
    async_runtime::block_on(async move {
        let options = pools::options(&source, false, true).map_err(|error| error.to_string())?;
        let mut connection = SqliteConnection::connect_with(&options)
            .await
            .map_err(|error| {
                format!(
                    "open development snapshot source {}: {error}",
                    source.display()
                )
            })?;
        let statement = format!("vacuum into {}", sqlite_string(&snapshot_destination)?);
        let result = sqlx::query(&statement)
            .execute(&mut connection)
            .await
            .map_err(|error| {
                format!(
                    "snapshot database {} to {}: {error}",
                    source.display(),
                    snapshot_destination.display()
                )
            });
        let close = connection.close().await.map_err(|error| {
            format!(
                "close development snapshot source {}: {error}",
                source.display()
            )
        });
        result?;
        close?;
        Ok::<(), String>(())
    })
    .map_err(|error| format!("start development database snapshot runtime: {error}"))??;
    set_owner_only(&destination)?;
    Ok(true)
}

fn prepare_workflow_database(path: &Path) -> Result<u64, String> {
    let path = path.to_path_buf();
    async_runtime::block_on(async move {
        let store = DurableWorkflowRunStore::open(&path)
            .await
            .map_err(|error| format!("migrate development Workflow database: {error}"))?;
        store.close().await;

        let options = pools::options(&path, false, false).map_err(|error| error.to_string())?;
        let mut connection = SqliteConnection::connect_with(&options)
            .await
            .map_err(|error| {
                format!(
                    "open development Workflow database {}: {error}",
                    path.display()
                )
            })?;
        let removed = sqlx::query(
            "delete from workflow_run where status not in ('succeeded', 'failed', 'cancelled')",
        )
        .execute(&mut connection)
        .await
        .map_err(|error| format!("remove active work from development snapshot: {error}"))?
        .rows_affected();
        connection.close().await.map_err(|error| {
            format!(
                "close development Workflow database {}: {error}",
                path.display()
            )
        })?;
        Ok::<u64, String>(removed)
    })
    .map_err(|error| format!("start development Workflow database runtime: {error}"))?
}

fn sqlite_string(path: &Path) -> Result<String, String> {
    let text = path.to_str().ok_or_else(|| {
        format!(
            "SQLite development snapshot path is not valid UTF-8: {}",
            path.display()
        )
    })?;
    Ok(format!("'{}'", text.replace('\'', "''")))
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    let mut permissions = fs::metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?
        .permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)
        .map_err(|error| format!("set owner-only permissions on {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::prepare;
    use crate::compact_runtime::CompactTempDir;
    use std::fs;

    #[test]
    fn missing_destination_inside_source_is_rejected_and_removed() {
        let temporary = CompactTempDir::new("nested-dev-state");
        let source = temporary.path().join("source");
        fs::create_dir(&source).expect("create source");
        let destination = source.join(format!("nested-{}", "d".repeat(180)));

        let error = prepare(&source, &destination).expect_err("reject nested destination");

        assert!(
            error.contains("must not be inside source"),
            "unexpected error: {error}"
        );
        assert!(!destination.exists(), "rejected destination was retained");
        assert_eq!(
            fs::read_dir(&source).expect("read source").count(),
            0,
            "rejected copy changed the source"
        );
    }
}
