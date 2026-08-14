use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct QualifiedIdentity(String);

impl QualifiedIdentity {
    pub fn new(value: impl Into<String>) -> Result<Self, ResourceError> {
        let value = value.into();
        let Some((namespace, resource)) = value.split_once('/') else {
            return Err(ResourceError::InvalidIdentity(value));
        };
        let valid_segment = |segment: &str| {
            !segment.is_empty()
                && !segment.starts_with('.')
                && !segment.ends_with('.')
                && segment.split('.').all(|part| {
                    !part.is_empty()
                        && part
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                })
        };
        if !valid_segment(namespace) || !valid_segment(resource) {
            return Err(ResourceError::InvalidIdentity(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for QualifiedIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for QualifiedIdentity {
    type Err = ResourceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ResourceScope {
    Global,
    Repository,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ResourceKind {
    Skill,
    Template,
}

impl ResourceKind {
    fn directory(self) -> &'static str {
        match self {
            Self::Skill => "skills",
            Self::Template => "templates",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredResource {
    pub identity: QualifiedIdentity,
    pub kind: ResourceKind,
    pub scope: ResourceScope,
    pub path: PathBuf,
    /// Repository resources carry their captured content so consumers never reopen a mutable path.
    pub captured_bytes: Option<Vec<u8>>,
}

#[derive(Debug)]
pub enum ResourceError {
    Io(std::io::Error),
    InvalidIdentity(String),
    InvalidSource {
        path: PathBuf,
        message: String,
    },
    IdentityConflict {
        identity: QualifiedIdentity,
        first: PathBuf,
        second: PathBuf,
    },
    InvalidRevision(String),
    DigestMismatch {
        expected: String,
        actual: String,
    },
    ReferencedRevision(String),
}

impl fmt::Display for ResourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::InvalidIdentity(value) => {
                write!(formatter, "invalid qualified resource identity `{value}`")
            }
            Self::InvalidSource { path, message } => {
                write!(formatter, "invalid resource {}: {message}", path.display())
            }
            Self::IdentityConflict {
                identity,
                first,
                second,
            } => write!(
                formatter,
                "resource identity `{identity}` is defined by both {} and {}",
                first.display(),
                second.display()
            ),
            Self::InvalidRevision(value) => write!(formatter, "invalid content revision `{value}`"),
            Self::DigestMismatch { expected, actual } => write!(
                formatter,
                "digest mismatch: expected {expected}, got {actual}"
            ),
            Self::ReferencedRevision(revision) => {
                write!(formatter, "content revision {revision} is still referenced")
            }
        }
    }
}

impl std::error::Error for ResourceError {}

impl From<std::io::Error> for ResourceError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Deserialize)]
struct IdentityHeader {
    id: String,
}

/// Ensure the user-owned global drop-in locations exist.
///
/// Loose resources placed here are discovered directly; they do not need a package manifest or
/// installation record.
pub fn ensure_global_drop_in_directories(global_root: &Path) -> Result<(), ResourceError> {
    for directory in ["workflows", "triggers", "skills", "templates"] {
        fs::create_dir_all(global_root.join(directory))?;
    }
    Ok(())
}

pub fn discover(
    global_root: &Path,
    repository: Option<&crate::TrustedRepositoryResources>,
) -> Result<Vec<DiscoveredResource>, ResourceError> {
    let mut resources = Vec::new();
    discover_scope(global_root, ResourceScope::Global, &mut resources)?;
    if let Some(repository) = repository {
        discover_repository_snapshot(repository, &mut resources)?;
    }
    let mut identities = BTreeMap::<QualifiedIdentity, PathBuf>::new();
    for resource in &resources {
        if let Some(first) = identities.insert(resource.identity.clone(), resource.path.clone()) {
            return Err(ResourceError::IdentityConflict {
                identity: resource.identity.clone(),
                first,
                second: resource.path.clone(),
            });
        }
    }
    resources.sort_by(|left, right| left.identity.cmp(&right.identity));
    Ok(resources)
}

fn discover_repository_snapshot(
    repository: &crate::TrustedRepositoryResources,
    output: &mut Vec<DiscoveredResource>,
) -> Result<(), ResourceError> {
    for (path, bytes) in repository.snapshot().resource_files() {
        let kind = match path.components().next().map(|part| part.as_os_str()) {
            Some(value) if value == "skills" => ResourceKind::Skill,
            Some(value) if value == "templates" => ResourceKind::Template,
            _ => continue,
        };
        let source = std::str::from_utf8(bytes).map_err(|error| ResourceError::InvalidSource {
            path: repository.snapshot().path_for(path),
            message: error.to_string(),
        })?;
        let value: toml::Value =
            toml::from_str(source).map_err(|error| ResourceError::InvalidSource {
                path: repository.snapshot().path_for(path),
                message: error.to_string(),
            })?;
        let header: IdentityHeader =
            value
                .try_into()
                .map_err(|error| ResourceError::InvalidSource {
                    path: repository.snapshot().path_for(path),
                    message: error.to_string(),
                })?;
        output.push(DiscoveredResource {
            identity: QualifiedIdentity::new(header.id)?,
            kind,
            scope: ResourceScope::Repository,
            path: repository.snapshot().path_for(path),
            captured_bytes: Some(bytes.to_vec()),
        });
    }
    Ok(())
}

fn discover_scope(
    root: &Path,
    scope: ResourceScope,
    output: &mut Vec<DiscoveredResource>,
) -> Result<(), ResourceError> {
    for kind in [ResourceKind::Skill, ResourceKind::Template] {
        let directory = root.join(kind.directory());
        if !directory.is_dir() {
            continue;
        }
        visit_files(&directory, &mut |path| {
            let source = fs::read_to_string(path).map_err(ResourceError::Io)?;
            let value: toml::Value =
                toml::from_str(&source).map_err(|error| ResourceError::InvalidSource {
                    path: path.to_owned(),
                    message: error.to_string(),
                })?;
            let header: IdentityHeader =
                value
                    .try_into()
                    .map_err(|error| ResourceError::InvalidSource {
                        path: path.to_owned(),
                        message: error.to_string(),
                    })?;
            output.push(DiscoveredResource {
                identity: QualifiedIdentity::new(header.id)?,
                kind,
                scope,
                path: path.to_owned(),
                captured_bytes: None,
            });
            Ok(())
        })?;
    }
    Ok(())
}

fn visit_files(
    directory: &Path,
    visitor: &mut impl FnMut(&Path) -> Result<(), ResourceError>,
) -> Result<(), ResourceError> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(ResourceError::InvalidSource {
                path: entry.path(),
                message: "symbolic links are not resource files".into(),
            });
        }
        if file_type.is_dir() {
            visit_files(&entry.path(), visitor)?;
        } else if file_type.is_file() {
            visitor(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let root = std::env::temp_dir().canonicalize().unwrap();
        let path = root.join(format!("prism-resource-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn global_drop_in_directories_are_ready_for_uninstalled_resources() {
        let global = temp("drop-ins");

        ensure_global_drop_in_directories(&global).unwrap();

        for directory in ["workflows", "triggers", "skills", "templates"] {
            assert!(global.join(directory).is_dir());
        }
        fs::remove_dir_all(global).unwrap();
    }

    #[test]
    fn scope_does_not_shadow_identity() {
        let global = temp("global");
        let repository = temp("repo");
        fs::create_dir(global.join("skills")).unwrap();
        fs::create_dir(repository.join("skills")).unwrap();
        fs::write(global.join("skills/a.toml"), "id='acme.release/publish'").unwrap();
        fs::write(
            repository.join("skills/a.toml"),
            "id='acme.release/publish'",
        )
        .unwrap();
        let snapshot = crate::RepositoryResourceSnapshot::capture(&repository).unwrap();
        let trusted = crate::workflow::source::TrustedRepositoryResources(snapshot);
        assert!(matches!(
            discover(&global, Some(&trusted)),
            Err(ResourceError::IdentityConflict { .. })
        ));
        fs::remove_dir_all(global).unwrap();
        fs::remove_dir_all(repository).unwrap();
    }
}
