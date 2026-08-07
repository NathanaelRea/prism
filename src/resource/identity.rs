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
    Workflow,
    Extension,
    ArtifactSchema,
    Skill,
    Template,
    Trigger,
    Notification,
}

impl ResourceKind {
    fn directory(self) -> &'static str {
        match self {
            Self::Workflow => "workflows",
            Self::Extension => "extensions",
            Self::ArtifactSchema | Self::Trigger | Self::Notification => "packages",
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
    for directory in ["workflows", "extensions", "skills", "templates"] {
        fs::create_dir_all(global_root.join(directory))?;
    }
    Ok(())
}

pub fn discover(
    global_root: &Path,
    repository_root: Option<&Path>,
) -> Result<Vec<DiscoveredResource>, ResourceError> {
    let mut resources = Vec::new();
    discover_scope(global_root, ResourceScope::Global, &mut resources)?;
    if let Some(root) = repository_root {
        discover_scope(root, ResourceScope::Repository, &mut resources)?;
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

fn discover_scope(
    root: &Path,
    scope: ResourceScope,
    output: &mut Vec<DiscoveredResource>,
) -> Result<(), ResourceError> {
    for kind in [
        ResourceKind::Workflow,
        ResourceKind::Skill,
        ResourceKind::Template,
    ] {
        let directory = root.join(kind.directory());
        if !directory.is_dir() {
            continue;
        }
        visit_files(&directory, &mut |path| {
            let source = fs::read_to_string(path).map_err(ResourceError::Io)?;
            let header: IdentityHeader =
                toml::from_str(&source).map_err(|error| ResourceError::InvalidSource {
                    path: path.to_owned(),
                    message: error.to_string(),
                })?;
            output.push(DiscoveredResource {
                identity: QualifiedIdentity::new(header.id)?,
                kind,
                scope,
                path: path.to_owned(),
            });
            Ok(())
        })?;
    }
    discover_loose_extensions(root, scope, output)?;
    discover_packages(root, scope, output)?;
    Ok(())
}

fn discover_loose_extensions(
    root: &Path,
    scope: ResourceScope,
    output: &mut Vec<DiscoveredResource>,
) -> Result<(), ResourceError> {
    let directory = root.join(ResourceKind::Extension.directory());
    if !directory.is_dir() {
        return Ok(());
    }
    let mut manifests = Vec::new();
    visit_files(&directory, &mut |path| {
        if path
            .file_name()
            .is_some_and(|name| name == "prism-extension.toml")
        {
            manifests.push(path.to_path_buf());
        }
        Ok(())
    })?;
    for path in manifests {
        let source = fs::read_to_string(&path)?;
        let header: IdentityHeader =
            toml::from_str(&source).map_err(|error| ResourceError::InvalidSource {
                path: path.clone(),
                message: error.to_string(),
            })?;
        output.push(DiscoveredResource {
            identity: QualifiedIdentity::new(header.id)?,
            kind: ResourceKind::Extension,
            scope,
            path: path
                .parent()
                .expect("extension manifest has a parent")
                .to_path_buf(),
        });
    }
    Ok(())
}

#[derive(Deserialize)]
struct DiscoveryManifest {
    resources: Vec<DiscoveryManifestResource>,
}

#[derive(Deserialize)]
struct DiscoveryManifestResource {
    id: String,
    kind: String,
    path: String,
}

fn discover_packages(
    root: &Path,
    scope: ResourceScope,
    output: &mut Vec<DiscoveredResource>,
) -> Result<(), ResourceError> {
    let packages = root.join("packages");
    if !packages.is_dir() {
        return Ok(());
    }
    let mut entries = fs::read_dir(packages)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let manifest_path = entry.path().join("prism-package.toml");
        if !manifest_path.is_file() {
            continue;
        }
        let source = fs::read_to_string(&manifest_path)?;
        let manifest: DiscoveryManifest =
            toml::from_str(&source).map_err(|error| ResourceError::InvalidSource {
                path: manifest_path.clone(),
                message: error.to_string(),
            })?;
        for resource in manifest.resources {
            let relative = Path::new(&resource.path);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|component| !matches!(component, std::path::Component::Normal(_)))
            {
                return Err(ResourceError::InvalidSource {
                    path: manifest_path.clone(),
                    message: format!("unsafe resource path {}", resource.path),
                });
            }
            let path = entry.path().join(relative);
            if !path.is_file() {
                return Err(ResourceError::InvalidSource {
                    path,
                    message: "manifest resource does not exist".into(),
                });
            }
            let kind = match resource.kind.as_str() {
                "workflow" => ResourceKind::Workflow,
                "extension" => ResourceKind::Extension,
                "artifact_schema" => ResourceKind::ArtifactSchema,
                "skill" => ResourceKind::Skill,
                "template" => ResourceKind::Template,
                "trigger" => ResourceKind::Trigger,
                "notification" => ResourceKind::Notification,
                value => {
                    return Err(ResourceError::InvalidSource {
                        path: manifest_path.clone(),
                        message: format!("unknown resource kind {value}"),
                    });
                }
            };
            output.push(DiscoveredResource {
                identity: QualifiedIdentity::new(resource.id)?,
                kind,
                scope,
                path,
            });
        }
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
        let path =
            std::env::temp_dir().join(format!("prism-resource-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn global_drop_in_directories_are_ready_for_uninstalled_resources() {
        let global = temp("drop-ins");

        ensure_global_drop_in_directories(&global).unwrap();

        for directory in ["workflows", "extensions", "skills", "templates"] {
            assert!(global.join(directory).is_dir());
        }
        fs::remove_dir_all(global).unwrap();
    }

    #[test]
    fn scope_does_not_shadow_identity() {
        let global = temp("global");
        let repository = temp("repo");
        fs::create_dir(global.join("workflows")).unwrap();
        fs::create_dir(repository.join("workflows")).unwrap();
        fs::write(global.join("workflows/a.toml"), "id='acme.release/publish'").unwrap();
        fs::write(
            repository.join("workflows/a.toml"),
            "id='acme.release/publish'",
        )
        .unwrap();
        assert!(matches!(
            discover(&global, Some(&repository)),
            Err(ResourceError::IdentityConflict { .. })
        ));
        fs::remove_dir_all(global).unwrap();
        fs::remove_dir_all(repository).unwrap();
    }
}
