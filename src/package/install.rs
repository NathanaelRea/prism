use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::resource::{
    ContentRevision, ContentStore, Reference, ResourceError, ResourceScope, TrustStore,
};

use super::manifest::{LockedPackage, PackageLock, PackageManifest, PackageValidationError};
use super::source::{ResolvedSource, SourceLimits, canonical_tree};

#[derive(Clone, Debug)]
pub struct InstallOutcome {
    pub package_id: String,
    pub revision: ContentRevision,
    pub working_copy: PathBuf,
}

pub struct PackageInstaller {
    scope_root: PathBuf,
    state_root: PathBuf,
    store: ContentStore,
}

impl PackageInstaller {
    pub fn new(
        scope_root: impl Into<PathBuf>,
        state_root: impl Into<PathBuf>,
        store_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            scope_root: scope_root.into(),
            state_root: state_root.into(),
            store: ContentStore::new(store_root),
        }
    }

    pub fn install(
        &self,
        source: &ResolvedSource,
        target: Option<&str>,
    ) -> Result<InstallOutcome, InstallError> {
        let manifest_path = ["prism-package.toml", "package.toml"]
            .iter()
            .map(|name| source.root.join(name))
            .find(|path| path.is_file())
            .ok_or_else(|| InstallError::Invalid("package has no prism-package.toml".into()))?;
        let canonical = canonical_tree(&source.root, SourceLimits::default())?;
        let manifest = PackageManifest::parse(&fs::read_to_string(manifest_path)?)?;
        verify_files(&source.root, &manifest, target)?;
        let revision = self.store.retain(&canonical)?;
        if revision != source.digest {
            return Err(InstallError::Invalid(format!(
                "resolved source changed before install: expected {}, got {revision}",
                source.digest
            )));
        }

        fs::create_dir_all(self.scope_root.join("packages"))?;
        fs::create_dir_all(self.state_root.join("package-bases"))?;
        let destination = self.scope_root.join("packages").join(&manifest.id);
        let base = self.state_root.join("package-bases").join(&manifest.id);
        if destination.exists() {
            return Err(InstallError::Invalid(format!(
                "package {} is already installed",
                manifest.id
            )));
        }
        let candidate = self.scope_root.join("packages").join(format!(
            ".{}-install-{}",
            manifest.id,
            std::process::id()
        ));
        let base_candidate = self.state_root.join("package-bases").join(format!(
            ".{}-install-{}",
            manifest.id,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&candidate);
        let _ = fs::remove_dir_all(&base_candidate);
        fs::create_dir(&candidate)?;
        fs::create_dir(&base_candidate)?;
        copy_directory(&source.root, &candidate)?;
        copy_directory(&source.root, &base_candidate)?;
        let old_lock = read_lock(&self.scope_root.join("package.lock"))?;
        validate_dependency_lock(&manifest, &old_lock)?;
        let mut new_lock = old_lock.clone();
        new_lock
            .packages
            .retain(|package| package.id != manifest.id);
        new_lock.packages.push(LockedPackage {
            id: manifest.id.clone(),
            source: source.origin.clone(),
            revision: source.revision.clone(),
            sha256: revision.as_str().trim_start_matches("sha256:").into(),
            dependencies: manifest
                .dependencies
                .iter()
                .map(|dependency| dependency.id.clone())
                .collect(),
        });
        new_lock
            .packages
            .sort_by(|left, right| left.id.cmp(&right.id));
        new_lock.validate()?;
        let lock_candidate = self
            .scope_root
            .join(format!(".package-lock-{}.tmp", std::process::id()));
        fs::write(
            &lock_candidate,
            toml::to_string_pretty(&new_lock)
                .map_err(|error| InstallError::Invalid(error.to_string()))?,
        )?;
        let reference = Reference {
            owner: format!("package-base-{}", safe_owner(&manifest.id)),
            revision: revision.clone(),
        };
        self.store.add_reference(&reference)?;
        if let Err(error) = activate_install(
            &candidate,
            &destination,
            &base_candidate,
            &base,
            &lock_candidate,
            &self.scope_root.join("package.lock"),
        ) {
            let _ = fs::remove_dir_all(candidate);
            let _ = fs::remove_dir_all(base_candidate);
            let _ = fs::remove_file(lock_candidate);
            let _ = self.store.remove_reference(&reference);
            return Err(error);
        }
        Ok(InstallOutcome {
            package_id: manifest.id,
            revision,
            working_copy: destination,
        })
    }

    pub fn install_repository(
        &self,
        source: &ResolvedSource,
        target: Option<&str>,
        repository: &Path,
        trust: &TrustStore,
    ) -> Result<InstallOutcome, InstallError> {
        if !trust.is_trusted(ResourceScope::Repository, Some(repository), &source.digest)? {
            return Err(InstallError::Invalid(format!(
                "repository package revision {} is not trusted",
                source.digest
            )));
        }
        self.install(source, target)
    }

    pub fn remove(&self, package_id: &str) -> Result<(), InstallError> {
        let destination = self.scope_root.join("packages").join(package_id);
        if !destination.is_dir() {
            return Ok(());
        }
        let disabled = self.state_root.join("removed-packages").join(format!(
            "{}-{}",
            safe_owner(package_id),
            std::process::id()
        ));
        fs::create_dir_all(disabled.parent().unwrap())?;
        let mut lock = read_lock(&self.scope_root.join("package.lock"))?;
        let retained = lock
            .packages
            .iter()
            .find(|package| package.id == package_id)
            .map(|package| {
                ContentRevision::parse(format!(
                    "sha256:{}",
                    package.sha256.trim_start_matches("sha256:")
                ))
            })
            .transpose()?;
        lock.packages.retain(|package| package.id != package_id);
        lock.validate()?;
        let lock_candidate = self
            .scope_root
            .join(format!(".package-lock-{}.tmp", std::process::id()));
        fs::write(
            &lock_candidate,
            toml::to_string_pretty(&lock)
                .map_err(|error| InstallError::Invalid(error.to_string()))?,
        )?;
        fs::rename(&destination, &disabled)?;
        if let Some(revision) = &retained {
            self.store.remove_reference(&Reference {
                owner: format!("package-base-{}", safe_owner(package_id)),
                revision: revision.clone(),
            })?;
        }
        if let Err(error) = replace_file(&lock_candidate, &self.scope_root.join("package.lock")) {
            let _ = fs::rename(&disabled, &destination);
            if let Some(revision) = retained {
                let _ = self.store.add_reference(&Reference {
                    owner: format!("package-base-{}", safe_owner(package_id)),
                    revision,
                });
            }
            return Err(error);
        }
        fs::remove_dir_all(disabled)?;
        let base = self.state_root.join("package-bases").join(package_id);
        if base.is_dir() {
            fs::remove_dir_all(base)?;
        }
        Ok(())
    }

    pub fn reconstruct(
        &self,
        revision: &ContentRevision,
        destination: &Path,
    ) -> Result<(), InstallError> {
        if destination.exists() {
            return Err(InstallError::Invalid(
                "reconstruction destination already exists".into(),
            ));
        }
        let canonical = self.store.load(revision)?;
        fs::create_dir_all(destination)?;
        decode_canonical_tree(&canonical, destination).inspect_err(|_| {
            let _ = fs::remove_dir_all(destination);
        })
    }
}

fn verify_files(
    root: &Path,
    manifest: &PackageManifest,
    target: Option<&str>,
) -> Result<(), InstallError> {
    for resource in &manifest.resources {
        verify_file(root, &resource.path, &resource.sha256)?;
    }
    for extension in &manifest.extensions {
        if extension.targets.is_empty() {
            if extension.source.is_none() {
                return Err(InstallError::Invalid(format!(
                    "extension {} has neither a prebuilt target nor source",
                    extension.id
                )));
            }
            continue;
        }
        let target = target.ok_or_else(|| {
            InstallError::Invalid(format!(
                "target triple is required to select extension {}",
                extension.id
            ))
        })?;
        let artifact = extension
            .targets
            .iter()
            .find(|artifact| artifact.target == target)
            .ok_or_else(|| {
                InstallError::Invalid(format!(
                    "extension {} has no artifact for {target}",
                    extension.id
                ))
            })?;
        verify_file(root, &artifact.path, &artifact.sha256)?;
    }
    Ok(())
}
fn verify_file(root: &Path, relative: &str, expected: &str) -> Result<(), InstallError> {
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_file() {
        return Err(InstallError::Invalid(format!(
            "package resource {relative} is not a regular file"
        )));
    }
    let actual = format!("{:x}", Sha256::digest(fs::read(path)?));
    if actual != expected.trim_start_matches("sha256:") {
        return Err(InstallError::Invalid(format!(
            "digest mismatch for {relative}"
        )));
    }
    Ok(())
}

fn activate_install(
    candidate: &Path,
    destination: &Path,
    base_candidate: &Path,
    base: &Path,
    lock_candidate: &Path,
    lock: &Path,
) -> Result<(), InstallError> {
    fs::rename(candidate, destination)?;
    if let Err(error) = fs::rename(base_candidate, base) {
        let _ = fs::remove_dir_all(destination);
        return Err(error.into());
    }
    if let Err(error) = replace_file(lock_candidate, lock) {
        let _ = fs::remove_dir_all(destination);
        let _ = fs::remove_dir_all(base);
        return Err(error);
    }
    Ok(())
}
fn replace_file(candidate: &Path, destination: &Path) -> Result<(), InstallError> {
    let backup = destination.with_extension(format!("backup-{}", std::process::id()));
    if destination.exists() {
        fs::rename(destination, &backup)?;
    }
    if let Err(error) = fs::rename(candidate, destination) {
        if backup.exists() {
            let _ = fs::rename(backup, destination);
        }
        return Err(error.into());
    }
    let _ = fs::remove_file(backup);
    Ok(())
}
fn read_lock(path: &Path) -> Result<PackageLock, InstallError> {
    match fs::read_to_string(path) {
        Ok(source) => Ok(PackageLock::parse(&source)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PackageLock {
            schema_version: 1,
            packages: Vec::new(),
        }),
        Err(error) => Err(error.into()),
    }
}

fn validate_dependency_lock(
    manifest: &PackageManifest,
    lock: &PackageLock,
) -> Result<(), InstallError> {
    for dependency in &manifest.dependencies {
        let Some(locked) = lock
            .packages
            .iter()
            .find(|package| package.id == dependency.id)
        else {
            return Err(InstallError::Invalid(format!(
                "dependency {} is not installed",
                dependency.id
            )));
        };
        if locked.source != dependency.source
            || locked.revision != dependency.revision
            || locked.sha256.trim_start_matches("sha256:")
                != dependency.sha256.trim_start_matches("sha256:")
        {
            return Err(InstallError::Invalid(format!(
                "dependency {} does not match its exact locked source, revision, and digest",
                dependency.id
            )));
        }
    }
    Ok(())
}
fn copy_directory(source: &Path, destination: &Path) -> Result<(), InstallError> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(InstallError::Invalid(format!(
                "package contains symlink {}",
                entry.path().display()
            )));
        }
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            fs::create_dir(&target)?;
            copy_directory(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
fn safe_owner(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || byte == b'-' {
                byte as char
            } else {
                '_'
            }
        })
        .collect()
}

fn decode_canonical_tree(mut bytes: &[u8], destination: &Path) -> Result<(), InstallError> {
    while !bytes.is_empty() {
        let path_length = take_u64(&mut bytes)? as usize;
        if path_length > bytes.len() {
            return Err(InstallError::Invalid(
                "corrupt retained package path".into(),
            ));
        }
        let path = std::str::from_utf8(&bytes[..path_length])
            .map_err(|_| InstallError::Invalid("retained package path is not UTF-8".into()))?;
        super::manifest::validate_relative_path(path)?;
        bytes = &bytes[path_length..];
        let content_length = take_u64(&mut bytes)? as usize;
        if content_length > bytes.len() {
            return Err(InstallError::Invalid(
                "corrupt retained package content".into(),
            ));
        }
        let target = destination.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, &bytes[..content_length])?;
        bytes = &bytes[content_length..];
    }
    Ok(())
}
fn take_u64(bytes: &mut &[u8]) -> Result<u64, InstallError> {
    if bytes.len() < 8 {
        return Err(InstallError::Invalid("corrupt retained package".into()));
    }
    let mut value = [0; 8];
    value.copy_from_slice(&bytes[..8]);
    *bytes = &bytes[8..];
    Ok(u64::from_be_bytes(value))
}

pub fn bootstrap_standard_pack(global_root: &Path) -> Result<bool, InstallError> {
    if global_root.join("packages/prism.standard").exists() {
        return promote_new_standard_workflows(global_root);
    }
    let executable = locate_standard_extension()?;
    bootstrap_standard_pack_with_extension(global_root, &executable)
}

fn standard_workflows() -> BTreeMap<&'static str, &'static str> {
    [
        ("plan", include_str!("../../assets/workflows/plan.toml")),
        (
            "implement",
            include_str!("../../assets/workflows/implement.toml"),
        ),
        ("auto", include_str!("../../assets/workflows/auto.toml")),
        (
            "stabilize",
            include_str!("../../assets/workflows/stabilize.toml"),
        ),
        (
            "stabilize-change-request",
            include_str!("../../assets/workflows/stabilize-change-request.toml"),
        ),
        (
            "triage-issues",
            include_str!("../../assets/workflows/triage-issues.toml"),
        ),
    ]
    .into_iter()
    .collect()
}

/// Make workflows added by a newer Prism binary available to an older, user-owned Standard Pack
/// without rewriting that package working copy. Each addition is promoted once into the ordinary
/// global drop-in directory; later edits or deletion remain user-owned.
fn promote_new_standard_workflows(global_root: &Path) -> Result<bool, InstallError> {
    let manifest_path = global_root.join("packages/prism.standard/prism-package.toml");
    let manifest = PackageManifest::parse(&fs::read_to_string(&manifest_path)?)?;
    let packaged = manifest
        .resources
        .iter()
        .map(|resource| resource.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let discovered = crate::resource::discover(global_root, None)?
        .into_iter()
        .map(|resource| resource.identity.to_string())
        .collect::<std::collections::BTreeSet<_>>();
    let marker_root = global_root.join("state/standard-workflow-promotions");
    let workflow_root = global_root.join("workflows");
    let mut changed = false;

    for (name, source) in standard_workflows() {
        let id = format!("prism.standard/{name}");
        let marker = marker_root.join(name);
        if packaged.contains(id.as_str()) || marker.exists() {
            continue;
        }
        fs::create_dir_all(&marker_root)?;
        if !discovered.contains(&id) {
            fs::create_dir_all(&workflow_root)?;
            fs::write(
                workflow_root.join(format!("prism-standard-{name}.toml")),
                format!(
                    "# Standard workflow added by a Prism upgrade; this is an editable global drop-in.\nid = \"{id}\"\n{source}"
                ),
            )?;
            changed = true;
        }
        fs::write(marker, format!("{id}\n"))?;
    }
    Ok(changed)
}

fn bootstrap_standard_pack_with_extension(
    global_root: &Path,
    executable: &Path,
) -> Result<bool, InstallError> {
    let root = global_root.join("packages/prism.standard");
    if root.exists() {
        return Ok(false);
    }
    let candidate = global_root.join(format!(
        "packages/.prism.standard-bootstrap-{}",
        std::process::id()
    ));
    fs::create_dir_all(candidate.join("workflows"))?;
    let workflows = standard_workflows();
    let mut resources = String::new();
    for (name, source) in workflows {
        let editable = format!(
            "# Standard Pack working copy; execution is enabled by the definition compiler.\nid = \"prism.standard/{name}\"\n{source}"
        );
        let relative = format!("workflows/{name}.toml");
        fs::write(candidate.join(&relative), &editable)?;
        resources.push_str(&format!("\n[[resources]]\nid = \"prism.standard/{name}\"\nkind = \"workflow\"\npath = \"{relative}\"\nsha256 = \"{:x}\"\n", Sha256::digest(editable.as_bytes())));
    }
    let extra_resources = [
        (
            "prism.standard/task-schema",
            "artifact_schema",
            "schemas/task-v1.json",
            "{\"$id\":\"prism.task/v1\",\"type\":\"object\"}\n",
        ),
        (
            "prism.standard/workflow-authoring",
            "skill",
            "skills/workflow-authoring.md",
            include_str!("../../assets/skills/workflow-authoring.md"),
        ),
        (
            "prism.standard/workflow-template",
            "template",
            "templates/workflow.toml",
            include_str!("../../assets/templates/workflow-v2.toml"),
        ),
        (
            "prism.standard/implementation-prompt",
            "template",
            "templates/implementation.md",
            include_str!("../../assets/templates/implementation.md"),
        ),
        (
            "prism.standard/review-repair-prompt",
            "template",
            "templates/review-repair.md",
            include_str!("../../assets/templates/review-repair.md"),
        ),
        (
            "prism.standard/extension-authoring",
            "skill",
            "skills/extension-authoring.md",
            include_str!("../../assets/skills/extension-authoring.md"),
        ),
        (
            "prism.standard/package-authoring",
            "skill",
            "skills/package-authoring.md",
            include_str!("../../assets/skills/package-authoring.md"),
        ),
        (
            "prism.standard/workflow-diagnostics",
            "skill",
            "skills/workflow-diagnostics.md",
            include_str!("../../assets/skills/workflow-diagnostics.md"),
        ),
    ];
    for (id, kind, relative, content) in extra_resources {
        let path = candidate.join(relative);
        fs::create_dir_all(path.parent().expect("standard resource parent"))?;
        fs::write(&path, content)?;
        resources.push_str(&format!(
            "\n[[resources]]\nid = \"{id}\"\nkind = \"{kind}\"\npath = \"{relative}\"\nsha256 = \"{:x}\"\n",
            Sha256::digest(content.as_bytes())
        ));
    }
    // Retain the exact executable bytes. A PATH launcher would make historical runs resolve a
    // mutable future binary and violate Definition Snapshot reproducibility.
    let standard_extension = fs::read(executable)?;
    let extension_path = candidate.join("extensions/prism-standard-extension");
    fs::create_dir_all(extension_path.parent().expect("standard extension parent"))?;
    fs::write(&extension_path, &standard_extension)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&extension_path, fs::Permissions::from_mode(0o755))?;
    }
    let extension_digest = format!("{:x}", Sha256::digest(&standard_extension));
    resources.push_str(&format!(
        "\n[[resources]]\nid = \"prism.standard/extension\"\nkind = \"extension\"\npath = \"extensions/prism-standard-extension\"\nsha256 = \"{extension_digest}\"\n"
    ));
    let extension = format!(
        "\n[[extensions]]\nid = \"prism.standard/extension\"\ncapabilities = [\"agent:run\", \"workspace:write\", \"process:run\", \"provider:read\", \"provider:write\", \"git:write\", \"worktrunk:write\"]\n\n[[extensions.targets]]\ntarget = \"{}\"\npath = \"extensions/prism-standard-extension\"\nsha256 = \"{extension_digest}\"\n",
        host_target_triple()?
    );
    let manifest = format!(
        "schema_version = 1\nid = \"prism.standard\"\nversion = \"{}\"\n{resources}{extension}",
        env!("CARGO_PKG_VERSION")
    );
    fs::write(candidate.join("prism-package.toml"), manifest)?;
    fs::create_dir_all(root.parent().unwrap())?;
    let canonical = canonical_tree(&candidate, SourceLimits::default())?;
    let store = ContentStore::new(global_root.join("store"));
    let revision = store.retain(&canonical)?;
    let reference = Reference {
        owner: "package-base-prism_standard".into(),
        revision: revision.clone(),
    };
    store.add_reference(&reference)?;
    let lock_path = global_root.join("package.lock");
    let mut lock = read_lock(&lock_path)?;
    lock.packages
        .retain(|package| package.id != "prism.standard");
    lock.packages.push(LockedPackage {
        id: "prism.standard".into(),
        source: "embedded:prism.standard".into(),
        revision: revision.to_string(),
        sha256: revision.as_str().trim_start_matches("sha256:").into(),
        dependencies: Vec::new(),
    });
    lock.packages.sort_by(|left, right| left.id.cmp(&right.id));
    lock.validate()?;
    let lock_candidate = global_root.join(format!(".package-lock-{}.tmp", std::process::id()));
    fs::write(
        &lock_candidate,
        toml::to_string_pretty(&lock).map_err(|error| InstallError::Invalid(error.to_string()))?,
    )?;
    match fs::rename(&candidate, &root) {
        Ok(()) => {
            if let Err(error) = replace_file(&lock_candidate, &lock_path) {
                let _ = fs::remove_dir_all(&root);
                let _ = store.remove_reference(&reference);
                return Err(error);
            }
            Ok(true)
        }
        Err(_error) if root.is_dir() => {
            let _ = fs::remove_dir_all(candidate);
            let _ = fs::remove_file(lock_candidate);
            Ok(false)
        }
        Err(error) => {
            let _ = fs::remove_dir_all(candidate);
            let _ = fs::remove_file(lock_candidate);
            let _ = store.remove_reference(&reference);
            Err(error.into())
        }
    }
}

pub(crate) fn locate_standard_extension() -> Result<PathBuf, InstallError> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("PRISM_STANDARD_EXTENSION") {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(directory) = current.parent()
    {
        candidates.push(directory.join("prism-standard-extension"));
        if directory.file_name().is_some_and(|name| name == "deps")
            && let Some(target_profile) = directory.parent()
        {
            candidates.push(target_profile.join("prism-standard-extension"));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates
            .extend(std::env::split_paths(&path).map(|path| path.join("prism-standard-extension")));
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            InstallError::Invalid(
                "prism-standard-extension is not installed beside Prism or on PATH".into(),
            )
        })
}

fn host_target_triple() -> Result<&'static str, InstallError> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-gnu"),
        ("aarch64", "macos") => Ok("aarch64-apple-darwin"),
        ("x86_64", "macos") => Ok("x86_64-apple-darwin"),
        (architecture, operating_system) => Err(InstallError::Invalid(format!(
            "unsupported Standard Pack host {architecture}-{operating_system}"
        ))),
    }
}

#[derive(Debug)]
pub enum InstallError {
    Io(std::io::Error),
    Validation(PackageValidationError),
    Resource(ResourceError),
    Invalid(String),
}
impl fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}
impl std::error::Error for InstallError {}
impl From<std::io::Error> for InstallError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<PackageValidationError> for InstallError {
    fn from(value: PackageValidationError) -> Self {
        Self::Validation(value)
    }
}
impl From<ResourceError> for InstallError {
    fn from(value: ResourceError) -> Self {
        Self::Resource(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{SourceLimits, SourceResolver, WorkingCopy};
    #[test]
    fn newer_standard_workflows_are_promoted_once_for_an_older_pack() {
        let root =
            std::env::temp_dir().join(format!("prism-standard-promotion-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let package = root.join("packages/prism.standard");
        fs::create_dir_all(package.join("workflows")).unwrap();
        fs::write(
            package.join("workflows/plan.toml"),
            "id='prism.standard/plan'\n",
        )
        .unwrap();
        fs::write(
            package.join("prism-package.toml"),
            "schema_version=1\nid='prism.standard'\nversion='old'\n[[resources]]\nid='prism.standard/plan'\nkind='workflow'\npath='workflows/plan.toml'\nsha256='0000000000000000000000000000000000000000000000000000000000000000'\n",
        )
        .unwrap();

        assert!(promote_new_standard_workflows(&root).unwrap());
        let stabilize = root.join("workflows/prism-standard-stabilize.toml");
        assert!(stabilize.is_file());
        assert!(
            fs::read_to_string(&stabilize)
                .unwrap()
                .contains("id = \"prism.standard/stabilize\"")
        );
        assert!(!promote_new_standard_workflows(&root).unwrap());

        fs::remove_file(&stabilize).unwrap();
        assert!(!promote_new_standard_workflows(&root).unwrap());
        assert!(
            !stabilize.exists(),
            "a user deletion must not be resurrected"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn standard_pack_is_editable_and_idempotent() {
        let root = std::env::temp_dir().join(format!("prism-bootstrap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let executable = root.join("fixture-extension");
        fs::write(&executable, b"pinned executable bytes").unwrap();
        assert!(bootstrap_standard_pack_with_extension(&root, &executable).unwrap());
        assert!(!bootstrap_standard_pack_with_extension(&root, &executable).unwrap());
        let launcher = root.join("packages/prism.standard/extensions/prism-standard-extension");
        assert_eq!(fs::read(&launcher).unwrap(), b"pinned executable bytes");
        let path = root.join("packages/prism.standard/workflows/auto.toml");
        fs::write(&path, "customized").unwrap();
        assert!(!bootstrap_standard_pack_with_extension(&root, &executable).unwrap());
        assert_eq!(fs::read_to_string(path).unwrap(), "customized");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_is_exact_and_reconstructable_after_source_removal() {
        let root = std::env::temp_dir().join(format!("prism-install-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let source = root.join("source");
        fs::create_dir_all(source.join("workflows")).unwrap();
        let workflow = b"id='acme.release/publish'\nschema_version=2\n";
        fs::write(source.join("workflows/publish.toml"), workflow).unwrap();
        fs::write(
            source.join("prism-package.toml"),
            format!("schema_version=1\nid='acme.release'\nversion='1'\n[[resources]]\nid='acme.release/publish'\nkind='workflow'\npath='workflows/publish.toml'\nsha256='{:x}'\n", Sha256::digest(workflow)),
        ).unwrap();
        let resolved = SourceResolver::new(root.join("stage"), SourceLimits::default())
            .resolve(source.to_str().unwrap())
            .unwrap();
        let installer =
            PackageInstaller::new(root.join("scope"), root.join("state"), root.join("store"));
        let outcome = installer.install(&resolved, None).unwrap();
        fs::remove_dir_all(&source).unwrap();
        fs::remove_dir_all(&outcome.working_copy).unwrap();
        let restored = root.join("restored");
        installer.reconstruct(&outcome.revision, &restored).unwrap();
        assert_eq!(
            fs::read(restored.join("workflows/publish.toml")).unwrap(),
            workflow
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn standard_update_conflict_preserves_customization_and_old_revision_restores() {
        let root =
            std::env::temp_dir().join(format!("prism-package-update-e2e-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let source_v1 = root.join("source-v1");
        let source_v2 = root.join("source-v2");
        for (source, timeout) in [(&source_v1, 10), (&source_v2, 30)] {
            fs::create_dir_all(source.join("workflows")).unwrap();
            let workflow = format!("id='acme.release/publish'\ntimeout={timeout}\n");
            fs::write(source.join("workflows/publish.toml"), &workflow).unwrap();
            fs::write(
                source.join("prism-package.toml"),
                format!(
                    "schema_version=1\nid='acme.release'\nversion='1'\n[[resources]]\nid='acme.release/publish'\nkind='workflow'\npath='workflows/publish.toml'\nsha256='{:x}'\n",
                    Sha256::digest(workflow.as_bytes())
                ),
            )
            .unwrap();
        }
        let resolver = SourceResolver::new(root.join("stage"), SourceLimits::default());
        let original = resolver.resolve(source_v1.to_str().unwrap()).unwrap();
        let incoming = resolver.resolve(source_v2.to_str().unwrap()).unwrap();
        let installer =
            PackageInstaller::new(root.join("scope"), root.join("state"), root.join("store"));
        let installed = installer.install(&original, None).unwrap();
        let workflow = installed.working_copy.join("workflows/publish.toml");
        fs::write(&workflow, "id='acme.release/publish'\ntimeout=20\n").unwrap();
        let working = WorkingCopy::new(
            &installed.working_copy,
            root.join("state/package-bases/acme.release"),
            root.join("state/package-updates/acme.release"),
        );
        let plan = working.plan_update(&incoming.root).unwrap();
        assert!(plan.has_conflicts());
        working
            .apply_update(&incoming.root, &plan, |_| Ok(()))
            .unwrap();
        assert!(
            fs::read_to_string(&workflow)
                .unwrap()
                .contains("timeout=20")
        );
        assert!(
            root.join(
                "state/package-updates/acme.release/conflicts/workflows/publish.toml.incoming"
            )
            .is_file()
        );

        installer.remove("acme.release").unwrap();
        let restored = root.join("restored");
        installer
            .reconstruct(&installed.revision, &restored)
            .unwrap();
        assert!(
            fs::read_to_string(restored.join("workflows/publish.toml"))
                .unwrap()
                .contains("timeout=10")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
