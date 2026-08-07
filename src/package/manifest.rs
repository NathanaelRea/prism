use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use crate::resource::{ContentRevision, QualifiedIdentity};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageManifest {
    pub schema_version: u32,
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub source: Option<PackageSource>,
    #[serde(default)]
    pub resources: Vec<PackageResource>,
    #[serde(default)]
    pub extensions: Vec<ExtensionArtifact>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    #[serde(default)]
    pub update: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageResource {
    pub id: String,
    pub kind: ResourceType,
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceType {
    Workflow,
    Extension,
    ArtifactSchema,
    Skill,
    Template,
    Trigger,
    Notification,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionArtifact {
    pub id: String,
    #[serde(default)]
    pub targets: Vec<TargetArtifact>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetArtifact {
    pub target: String,
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub id: String,
    pub source: String,
    pub revision: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum PackageSource {
    Location(String),
    Detailed { kind: String, location: String },
}

impl PackageManifest {
    pub fn parse(source: &str) -> Result<Self, PackageValidationError> {
        let manifest: Self = toml::from_str(source)
            .map_err(|error| PackageValidationError::Syntax(error.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), PackageValidationError> {
        if self.schema_version != 1 {
            return Err(PackageValidationError::UnsupportedSchema(
                self.schema_version,
            ));
        }
        validate_package_id(&self.id)?;
        if self.version.trim().is_empty() {
            return Err(PackageValidationError::InvalidField(
                "version must not be empty".into(),
            ));
        }
        let mut identities = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for resource in &self.resources {
            let identity = QualifiedIdentity::new(resource.id.clone())
                .map_err(|error| PackageValidationError::InvalidField(error.to_string()))?;
            if !identity.as_str().starts_with(&format!("{}/", self.id)) {
                return Err(PackageValidationError::InvalidField(format!(
                    "resource {} is outside package namespace {}",
                    resource.id, self.id
                )));
            }
            if !identities.insert(resource.id.as_str()) {
                return Err(PackageValidationError::DuplicateIdentity(
                    resource.id.clone(),
                ));
            }
            validate_relative_path(&resource.path)?;
            if !paths.insert(resource.path.as_str()) {
                return Err(PackageValidationError::DuplicatePath(resource.path.clone()));
            }
            validate_sha256(&resource.sha256)?;
        }
        for extension in &self.extensions {
            QualifiedIdentity::new(extension.id.clone())
                .map_err(|error| PackageValidationError::InvalidField(error.to_string()))?;
            let mut targets = BTreeSet::new();
            for artifact in &extension.targets {
                if artifact.target.trim().is_empty() || !targets.insert(artifact.target.as_str()) {
                    return Err(PackageValidationError::InvalidField(format!(
                        "duplicate or empty target for {}",
                        extension.id
                    )));
                }
                validate_relative_path(&artifact.path)?;
                validate_sha256(&artifact.sha256)?;
            }
            if let Some(source) = &extension.source {
                validate_relative_path(source)?;
            }
        }
        let mut dependencies = BTreeSet::new();
        for dependency in &self.dependencies {
            validate_package_id(&dependency.id)?;
            if !dependencies.insert(dependency.id.as_str()) {
                return Err(PackageValidationError::DuplicateIdentity(
                    dependency.id.clone(),
                ));
            }
            if dependency.source.trim().is_empty() {
                return Err(PackageValidationError::InvalidField(
                    "dependency source must be exact".into(),
                ));
            }
            if dependency.revision.trim().is_empty() {
                return Err(PackageValidationError::InvalidField(format!(
                    "dependency {} has no exact revision",
                    dependency.id
                )));
            }
            validate_sha256(&dependency.sha256)?;
        }
        Ok(())
    }

    pub fn target_artifact(
        &self,
        target: &str,
        extension_id: &str,
    ) -> Result<&TargetArtifact, PackageValidationError> {
        let extension = self
            .extensions
            .iter()
            .find(|value| value.id == extension_id)
            .ok_or_else(|| {
                PackageValidationError::InvalidField(format!("unknown extension {extension_id}"))
            })?;
        extension
            .targets
            .iter()
            .find(|artifact| artifact.target == target)
            .ok_or_else(|| PackageValidationError::MissingTarget {
                extension: extension_id.into(),
                target: target.into(),
            })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageLock {
    pub schema_version: u32,
    #[serde(default)]
    pub packages: Vec<LockedPackage>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedPackage {
    pub id: String,
    pub source: String,
    pub revision: String,
    pub sha256: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
}

impl PackageLock {
    pub fn parse(source: &str) -> Result<Self, PackageValidationError> {
        let lock: Self = toml::from_str(source)
            .map_err(|error| PackageValidationError::Syntax(error.to_string()))?;
        lock.validate()?;
        Ok(lock)
    }

    pub fn validate(&self) -> Result<(), PackageValidationError> {
        if self.schema_version != 1 {
            return Err(PackageValidationError::UnsupportedSchema(
                self.schema_version,
            ));
        }
        let mut ids = BTreeSet::new();
        for package in &self.packages {
            validate_package_id(&package.id)?;
            if !ids.insert(package.id.as_str()) {
                return Err(PackageValidationError::DuplicateIdentity(
                    package.id.clone(),
                ));
            }
            if package.source.trim().is_empty() || package.revision.trim().is_empty() {
                return Err(PackageValidationError::InvalidField(format!(
                    "locked package {} must use an exact source and revision",
                    package.id
                )));
            }
            validate_sha256(&package.sha256)?;
        }
        for package in &self.packages {
            for dependency in &package.dependencies {
                if !ids.contains(dependency.as_str()) {
                    return Err(PackageValidationError::InvalidField(format!(
                        "locked package {} references missing dependency {dependency}",
                        package.id
                    )));
                }
            }
        }
        detect_dependency_cycles(&self.packages)
    }
}

fn detect_dependency_cycles(packages: &[LockedPackage]) -> Result<(), PackageValidationError> {
    let graph: BTreeMap<_, _> = packages
        .iter()
        .map(|package| (package.id.as_str(), package.dependencies.as_slice()))
        .collect();
    fn visit<'a>(
        id: &'a str,
        graph: &BTreeMap<&'a str, &'a [String]>,
        active: &mut BTreeSet<&'a str>,
        done: &mut BTreeSet<&'a str>,
    ) -> Result<(), PackageValidationError> {
        if done.contains(id) {
            return Ok(());
        }
        if !active.insert(id) {
            return Err(PackageValidationError::DependencyCycle(id.into()));
        }
        if let Some(dependencies) = graph.get(id) {
            for dependency in *dependencies {
                visit(dependency, graph, active, done)?;
            }
        }
        active.remove(id);
        done.insert(id);
        Ok(())
    }
    let mut active = BTreeSet::new();
    let mut done = BTreeSet::new();
    for id in graph.keys() {
        visit(id, &graph, &mut active, &mut done)?;
    }
    Ok(())
}

fn validate_package_id(value: &str) -> Result<(), PackageValidationError> {
    if value.contains('/')
        || value.split('.').count() < 2
        || value.split('.').any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
    {
        return Err(PackageValidationError::InvalidField(format!(
            "invalid package identity `{value}`"
        )));
    }
    Ok(())
}

pub(crate) fn validate_relative_path(value: &str) -> Result<(), PackageValidationError> {
    let path = Path::new(value);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(PackageValidationError::UnsafePath(value.into()));
    }
    Ok(())
}

pub(crate) fn validate_sha256(value: &str) -> Result<(), PackageValidationError> {
    ContentRevision::parse(if value.starts_with("sha256:") {
        value.into()
    } else {
        format!("sha256:{value}")
    })
    .map(|_| ())
    .map_err(|_| PackageValidationError::InvalidDigest(value.into()))
}

#[derive(Debug, Eq, PartialEq)]
pub enum PackageValidationError {
    Syntax(String),
    UnsupportedSchema(u32),
    InvalidField(String),
    DuplicateIdentity(String),
    DuplicatePath(String),
    UnsafePath(String),
    InvalidDigest(String),
    DependencyCycle(String),
    MissingTarget { extension: String, target: String },
}

impl fmt::Display for PackageValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}
impl std::error::Error for PackageValidationError {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lock_requires_exact_acyclic_graph() {
        let source = "schema_version=1\n[[packages]]\nid='acme.one'\nsource='git+x'\nrevision='abc'\nsha256='0000000000000000000000000000000000000000000000000000000000000000'\ndependencies=['acme.two']\n[[packages]]\nid='acme.two'\nsource='git+y'\nrevision='def'\nsha256='1111111111111111111111111111111111111111111111111111111111111111'\ndependencies=['acme.one']";
        assert!(matches!(
            PackageLock::parse(source),
            Err(PackageValidationError::DependencyCycle(_))
        ));
    }
    #[test]
    fn manifest_rejects_traversal() {
        let source = "schema_version=1\nid='acme.release'\nversion='1'\n[[resources]]\nid='acme.release/publish'\nkind='workflow'\npath='../publish.toml'\nsha256='0000000000000000000000000000000000000000000000000000000000000000'";
        assert!(matches!(
            PackageManifest::parse(source),
            Err(PackageValidationError::UnsafePath(_))
        ));
    }
}
