use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use super::PackageValidationError;
use super::source::{SourceLimits, canonical_tree};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileUpdate {
    Apply { path: PathBuf, content: Vec<u8> },
    Delete { path: PathBuf },
    PreserveLocal { path: PathBuf },
    Tombstone { path: PathBuf },
    Conflict(MergeConflict),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeConflict {
    pub path: PathBuf,
    pub local: Option<Vec<u8>>,
    pub incoming: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdatePlan {
    pub updates: Vec<FileUpdate>,
    pub dirty: bool,
}

impl UpdatePlan {
    pub fn has_conflicts(&self) -> bool {
        self.updates
            .iter()
            .any(|update| matches!(update, FileUpdate::Conflict(_)))
    }
}

pub struct WorkingCopy {
    pub root: PathBuf,
    pub base: PathBuf,
    pub metadata: PathBuf,
}

impl WorkingCopy {
    pub fn new(
        root: impl Into<PathBuf>,
        base: impl Into<PathBuf>,
        metadata: impl Into<PathBuf>,
    ) -> Self {
        Self {
            root: root.into(),
            base: base.into(),
            metadata: metadata.into(),
        }
    }

    pub fn is_dirty(&self) -> Result<bool, WorkingCopyError> {
        Ok(canonical_tree(&self.root, SourceLimits::default())?
            != canonical_tree(&self.base, SourceLimits::default())?)
    }

    pub fn plan_update(&self, incoming: &Path) -> Result<UpdatePlan, WorkingCopyError> {
        let base = tree_files(&self.base)?;
        let local = tree_files(&self.root)?;
        let incoming = tree_files(incoming)?;
        let tombstones = self.load_tombstones()?;
        let paths: BTreeSet<_> = base
            .keys()
            .chain(local.keys())
            .chain(incoming.keys())
            .cloned()
            .collect();
        let mut updates = Vec::new();
        let mut dirty = false;
        for path in paths {
            let old = base.get(&path);
            let ours = local.get(&path);
            let theirs = incoming.get(&path);
            if tombstones.contains(&path) || (old.is_some() && ours.is_none()) {
                dirty = true;
                updates.push(FileUpdate::Tombstone { path });
                continue;
            }
            if ours == old {
                match theirs {
                    Some(content) if Some(content) != ours => updates.push(FileUpdate::Apply {
                        path,
                        content: content.clone(),
                    }),
                    None if ours.is_some() => updates.push(FileUpdate::Delete { path }),
                    _ => {}
                }
                continue;
            }
            dirty = true;
            if theirs == old {
                updates.push(FileUpdate::PreserveLocal { path });
                continue;
            }
            if ours == theirs {
                updates.push(FileUpdate::PreserveLocal { path });
                continue;
            }
            match (old, ours, theirs) {
                (Some(old), Some(ours), Some(theirs)) => match merge_text(old, ours, theirs) {
                    Some(content) => updates.push(FileUpdate::Apply { path, content }),
                    None => updates.push(FileUpdate::Conflict(MergeConflict {
                        path,
                        local: Some(ours.clone()),
                        incoming: Some(theirs.clone()),
                    })),
                },
                (_, ours, theirs) => updates.push(FileUpdate::Conflict(MergeConflict {
                    path,
                    local: ours.cloned(),
                    incoming: theirs.cloned(),
                })),
            }
        }
        updates.sort_by(|left, right| update_path(left).cmp(update_path(right)));
        Ok(UpdatePlan { updates, dirty })
    }

    pub fn apply_update(
        &self,
        incoming: &Path,
        plan: &UpdatePlan,
        validate: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<(), WorkingCopyError> {
        fs::create_dir_all(&self.metadata)?;
        for update in &plan.updates {
            if let FileUpdate::Conflict(conflict) = update {
                let candidate = self
                    .metadata
                    .join("conflicts")
                    .join(&conflict.path)
                    .with_extension(format!(
                        "{}.incoming",
                        conflict
                            .path
                            .extension()
                            .and_then(|value| value.to_str())
                            .unwrap_or("file")
                    ));
                if let Some(parent) = candidate.parent() {
                    fs::create_dir_all(parent)?;
                }
                match &conflict.incoming {
                    Some(content) => fs::write(candidate, content)?,
                    None => fs::write(candidate, b"# incoming revision deleted this resource\n")?,
                }
            }
        }
        if plan.has_conflicts() {
            return Ok(());
        }
        let parent = self
            .root
            .parent()
            .ok_or_else(|| WorkingCopyError::Invalid("working copy has no parent".into()))?;
        let candidate = parent.join(format!(".update-candidate-{}", std::process::id()));
        let backup = parent.join(format!(".update-backup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&candidate);
        let _ = fs::remove_dir_all(&backup);
        fs::create_dir(&candidate)?;
        copy_directory(&self.root, &candidate)?;
        let mut tombstones = self.load_tombstones()?;
        for update in &plan.updates {
            match update {
                FileUpdate::Apply { path, content } => {
                    let target = candidate.join(path);
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(target, content)?;
                    tombstones.remove(path);
                }
                FileUpdate::Delete { path } => {
                    remove_if_exists(&candidate.join(path))?;
                }
                FileUpdate::Tombstone { path } => {
                    remove_if_exists(&candidate.join(path))?;
                    tombstones.insert(path.clone());
                }
                FileUpdate::PreserveLocal { .. } => {}
                FileUpdate::Conflict(_) => unreachable!(),
            }
        }
        if let Err(message) = validate(&candidate) {
            fs::remove_dir_all(&candidate)?;
            return Err(WorkingCopyError::Validation(message));
        }
        fs::rename(&self.root, &backup)?;
        if let Err(error) = fs::rename(&candidate, &self.root) {
            let _ = fs::rename(&backup, &self.root);
            return Err(error.into());
        }
        if let Err(error) = replace_directory(incoming, &self.base) {
            let _ = fs::remove_dir_all(&self.root);
            let _ = fs::rename(&backup, &self.root);
            return Err(error);
        }
        let _ = fs::remove_dir_all(&backup);
        self.save_tombstones(&tombstones)?;
        Ok(())
    }

    fn load_tombstones(&self) -> Result<BTreeSet<PathBuf>, WorkingCopyError> {
        let path = self.metadata.join("tombstones");
        match fs::read_to_string(path) {
            Ok(source) => Ok(source
                .lines()
                .filter(|line| !line.is_empty())
                .map(PathBuf::from)
                .collect()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
            Err(error) => Err(error.into()),
        }
    }
    fn save_tombstones(&self, tombstones: &BTreeSet<PathBuf>) -> Result<(), WorkingCopyError> {
        let source = tombstones
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(
            self.metadata.join("tombstones"),
            if source.is_empty() {
                source
            } else {
                format!("{source}\n")
            },
        )?;
        Ok(())
    }
}

fn merge_text(base: &[u8], local: &[u8], incoming: &[u8]) -> Option<Vec<u8>> {
    let (base, local, incoming) = (
        std::str::from_utf8(base).ok()?,
        std::str::from_utf8(local).ok()?,
        std::str::from_utf8(incoming).ok()?,
    );
    let base_lines = keyed_lines(base);
    let local_lines = keyed_lines(local);
    let incoming_lines = keyed_lines(incoming);
    let keys: BTreeSet<_> = base_lines
        .keys()
        .chain(local_lines.keys())
        .chain(incoming_lines.keys())
        .cloned()
        .collect();
    let mut incoming_changes = Vec::new();
    for key in keys {
        let old = base_lines.get(&key);
        let ours = local_lines.get(&key);
        let theirs = incoming_lines.get(&key);
        if ours != old && theirs != old && ours != theirs {
            return None;
        }
        if ours == old && theirs != old {
            incoming_changes.push((key, theirs.cloned()));
        }
    }
    let mut result: Vec<String> = local.lines().map(str::to_owned).collect();
    for (key, replacement) in incoming_changes {
        if let Some(index) = indexed_lines(&result.join("\n"))
            .iter()
            .position(|(candidate, _)| candidate == &key)
        {
            match replacement {
                Some(line) => result[index] = line,
                None => {
                    result.remove(index);
                }
            }
        } else if let Some(line) = replacement {
            result.push(line);
        }
    }
    let trailing = local.ends_with('\n') || incoming.ends_with('\n');
    let mut merged = result.join("\n");
    if trailing {
        merged.push('\n');
    }
    Some(merged.into_bytes())
}

fn keyed_lines(source: &str) -> BTreeMap<String, String> {
    indexed_lines(source).into_iter().collect()
}

fn indexed_lines(source: &str) -> Vec<(String, String)> {
    let mut section = String::new();
    let mut section_counts = BTreeMap::<String, usize>::new();
    source
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                let count = section_counts.entry(trimmed.to_owned()).or_default();
                section = format!("{trimmed}#{count}");
                *count += 1;
            }
            let key = if line.trim_start().starts_with('#') || trimmed.is_empty() {
                format!("{section}/@{index}:{trimmed}")
            } else {
                format!(
                    "{section}/{}",
                    line.split_once('=').map_or(trimmed, |(key, _)| key.trim())
                )
            };
            (key, line.to_owned())
        })
        .collect()
}

fn tree_files(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>, WorkingCopyError> {
    let mut output = BTreeMap::new();
    if root.is_dir() {
        tree_files_at(root, root, &mut output)?;
    }
    Ok(output)
}
fn tree_files_at(
    root: &Path,
    directory: &Path,
    output: &mut BTreeMap<PathBuf, Vec<u8>>,
) -> Result<(), WorkingCopyError> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(WorkingCopyError::Invalid(format!(
                "symlink {}",
                entry.path().display()
            )));
        }
        if file_type.is_dir() {
            tree_files_at(root, &entry.path(), output)?;
        } else {
            output.insert(
                entry.path().strip_prefix(root).unwrap().into(),
                fs::read(entry.path())?,
            );
        }
    }
    Ok(())
}
fn copy_directory(source: &Path, destination: &Path) -> Result<(), WorkingCopyError> {
    for (path, content) in tree_files(source)? {
        let target = destination.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, content)?;
    }
    Ok(())
}
fn replace_directory(source: &Path, destination: &Path) -> Result<(), WorkingCopyError> {
    let parent = destination
        .parent()
        .ok_or_else(|| WorkingCopyError::Invalid("base has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let candidate = parent.join(format!(".base-candidate-{}", std::process::id()));
    let _ = fs::remove_dir_all(&candidate);
    fs::create_dir(&candidate)?;
    copy_directory(source, &candidate)?;
    let old = parent.join(format!(".base-old-{}", std::process::id()));
    let _ = fs::remove_dir_all(&old);
    if destination.exists() {
        fs::rename(destination, &old)?;
    }
    fs::rename(candidate, destination)?;
    let _ = fs::remove_dir_all(old);
    Ok(())
}
fn remove_if_exists(path: &Path) -> Result<(), WorkingCopyError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
fn update_path(update: &FileUpdate) -> &Path {
    match update {
        FileUpdate::Apply { path, .. }
        | FileUpdate::Delete { path }
        | FileUpdate::PreserveLocal { path }
        | FileUpdate::Tombstone { path } => path,
        FileUpdate::Conflict(conflict) => &conflict.path,
    }
}

#[derive(Debug)]
pub enum WorkingCopyError {
    Io(std::io::Error),
    Package(PackageValidationError),
    Validation(String),
    Invalid(String),
}
impl fmt::Display for WorkingCopyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}
impl std::error::Error for WorkingCopyError {}
impl From<std::io::Error> for WorkingCopyError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<PackageValidationError> for WorkingCopyError {
    fn from(value: PackageValidationError) -> Self {
        Self::Package(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conflicting_update_keeps_local_and_writes_candidate() {
        let root = std::env::temp_dir().join(format!("prism-update-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for name in ["base", "local", "incoming"] {
            fs::create_dir_all(root.join(name)).unwrap();
        }
        fs::write(root.join("base/a.toml"), "timeout=10\n").unwrap();
        fs::write(root.join("local/a.toml"), "timeout=20\n").unwrap();
        fs::write(root.join("incoming/a.toml"), "timeout=30\n").unwrap();
        let copy = WorkingCopy::new(root.join("local"), root.join("base"), root.join("meta"));
        let plan = copy.plan_update(&root.join("incoming")).unwrap();
        assert!(plan.has_conflicts());
        copy.apply_update(&root.join("incoming"), &plan, |_| Ok(()))
            .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("local/a.toml")).unwrap(),
            "timeout=20\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("meta/conflicts/a.toml.incoming")).unwrap(),
            "timeout=30\n"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn deleted_resource_is_tombstoned() {
        let root = std::env::temp_dir().join(format!("prism-delete-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for name in ["base", "local", "incoming"] {
            fs::create_dir_all(root.join(name)).unwrap();
        }
        fs::write(root.join("base/a"), "old").unwrap();
        fs::write(root.join("incoming/a"), "new").unwrap();
        let copy = WorkingCopy::new(root.join("local"), root.join("base"), root.join("meta"));
        let plan = copy.plan_update(&root.join("incoming")).unwrap();
        assert!(matches!(&plan.updates[0], FileUpdate::Tombstone { .. }));
        copy.apply_update(&root.join("incoming"), &plan, |_| Ok(()))
            .unwrap();
        assert!(!root.join("local/a").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repeated_toml_tables_merge_by_occurrence() {
        let base = b"[[steps]]\nid='one'\ntimeout=10\n[[steps]]\nid='two'\ntimeout=10\n";
        let local = b"[[steps]]\nid='one'\ntimeout=20\n[[steps]]\nid='two'\ntimeout=10\n";
        let incoming = b"[[steps]]\nid='one'\ntimeout=10\n[[steps]]\nid='two'\ntimeout=30\n";
        let merged = merge_text(base, local, incoming).unwrap();
        assert_eq!(
            String::from_utf8(merged).unwrap(),
            "[[steps]]\nid='one'\ntimeout=20\n[[steps]]\nid='two'\ntimeout=30\n"
        );
    }
}
