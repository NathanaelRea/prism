use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde::Serialize;

use crate::extension::host::{ExtensionClient, ExtensionHostError, HostDispatcher, HostLimits};
use crate::resource::{ContentRevision, ContentStore, QualifiedIdentity, Reference};

/// Deep application-facing seam for extension authoring and lifecycle operations.
pub struct ExtensionOperations {
    working_root: PathBuf,
    executable_root: PathBuf,
    store: ContentStore,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiagnosticReport {
    pub schema_version: u32,
    pub kind: String,
    pub extension_id: String,
    pub executable_revision: Option<String>,
    pub healthy: bool,
    pub diagnostics: Vec<String>,
}

impl ExtensionOperations {
    pub fn new(working_root: impl Into<PathBuf>, state_root: impl Into<PathBuf>) -> Self {
        let state_root = state_root.into();
        Self {
            working_root: working_root.into(),
            executable_root: state_root.join("extension-executables"),
            store: ContentStore::new(state_root.join("store")),
        }
    }

    pub fn scaffold(&self, id: &str) -> Result<PathBuf, ExtensionOperationError> {
        let identity = QualifiedIdentity::new(id.to_owned())
            .map_err(|error| ExtensionOperationError::Invalid(error.to_string()))?;
        let name = identity.as_str().replace(['/', '.'], "-");
        let root = self.working_root.join(identity.as_str());
        if root.exists() {
            return Err(ExtensionOperationError::Invalid(format!(
                "extension working copy '{}' already exists",
                root.display()
            )));
        }
        fs::create_dir_all(root.join("src"))?;
        fs::write(
            root.join("prism-extension.toml"),
            format!("schema_version = 1\nid = \"{}\"\n", identity.as_str()),
        )?;
        fs::write(root.join("Cargo.toml"), scaffold_manifest(&name))?;
        fs::write(root.join("src/main.rs"), scaffold_source(identity.as_str()))?;
        Ok(root)
    }

    pub fn check(&self, source: impl AsRef<Path>) -> Result<(), ExtensionOperationError> {
        run_cargo(source.as_ref(), &["check"])
    }

    pub fn build(
        &self,
        id: &str,
        source: impl AsRef<Path>,
    ) -> Result<(ContentRevision, PathBuf), ExtensionOperationError> {
        let identity = QualifiedIdentity::new(id.to_owned())
            .map_err(|error| ExtensionOperationError::Invalid(error.to_string()))?;
        let source = source.as_ref();
        let arguments = if source.join("Cargo.lock").is_file() {
            vec!["build", "--release", "--locked"]
        } else {
            vec!["build", "--release"]
        };
        run_cargo(source, &arguments)?;
        let manifest_path = source.join("Cargo.toml");
        let manifest = fs::read_to_string(&manifest_path)?;
        let value: toml::Value = toml::from_str(&manifest).map_err(|error| {
            ExtensionOperationError::Invalid(format!("parse extension manifest: {error}"))
        })?;
        let binary = value
            .get("package")
            .and_then(|value| value.get("name"))
            .and_then(toml::Value::as_str)
            .ok_or_else(|| {
                ExtensionOperationError::Invalid("extension Cargo.toml has no package.name".into())
            })?;
        let metadata = Command::new("cargo")
            .args(["metadata", "--format-version=1", "--no-deps"])
            .current_dir(source)
            .output()
            .map_err(|error| {
                ExtensionOperationError::Command(format!("run cargo metadata: {error}"))
            })?;
        if !metadata.status.success() {
            return Err(ExtensionOperationError::Command(
                String::from_utf8_lossy(&metadata.stderr)
                    .chars()
                    .take(16 * 1024)
                    .collect(),
            ));
        }
        let metadata: serde_json::Value =
            serde_json::from_slice(&metadata.stdout).map_err(|error| {
                ExtensionOperationError::Invalid(format!("parse Cargo metadata: {error}"))
            })?;
        let target_directory = metadata
            .get("target_directory")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ExtensionOperationError::Invalid("Cargo metadata has no target_directory".into())
            })?;
        let built = Path::new(target_directory).join("release").join(binary);
        if !built.is_file() {
            return Err(ExtensionOperationError::Invalid(format!(
                "extension build did not produce {}",
                built.display()
            )));
        }
        let bytes = fs::read(&built)?;
        let revision = self
            .store
            .retain(&bytes)
            .map_err(|error| ExtensionOperationError::Store(error.to_string()))?;
        let path = self.materialize(&revision, &bytes)?;
        self.store
            .add_reference(&Reference {
                owner: reference_owner(identity.as_str()),
                revision: revision.clone(),
            })
            .map_err(|error| ExtensionOperationError::Store(error.to_string()))?;
        Ok((revision, path))
    }

    pub async fn reload(
        &self,
        id: &str,
        source: impl AsRef<Path>,
        dispatcher: Arc<dyn HostDispatcher>,
        limits: HostLimits,
    ) -> Result<Arc<ExtensionClient>, ExtensionOperationError> {
        let (_, executable) = self.build(id, source)?;
        ExtensionClient::launch(executable, dispatcher, limits)
            .await
            .map_err(Into::into)
    }

    pub async fn doctor(
        &self,
        id: &str,
        executable: impl AsRef<Path>,
        dispatcher: Arc<dyn HostDispatcher>,
        limits: HostLimits,
    ) -> DiagnosticReport {
        match ExtensionClient::launch(executable, dispatcher, limits).await {
            Ok(client) => {
                let healthy = client.heartbeat().await.is_ok();
                let report = DiagnosticReport {
                    schema_version: 1,
                    kind: "extension.doctor".into(),
                    extension_id: id.into(),
                    executable_revision: Some(client.revision().into()),
                    healthy,
                    diagnostics: client.diagnostics(),
                };
                let _ = client.shutdown().await;
                report
            }
            Err(error) => DiagnosticReport {
                schema_version: 1,
                kind: "extension.doctor".into(),
                extension_id: id.into(),
                executable_revision: None,
                healthy: false,
                diagnostics: vec![error.to_string()],
            },
        }
    }

    pub fn retained_executable(
        &self,
        revision: &ContentRevision,
    ) -> Result<PathBuf, ExtensionOperationError> {
        let bytes = self
            .store
            .load(revision)
            .map_err(|error| ExtensionOperationError::Store(error.to_string()))?;
        self.materialize(revision, &bytes)
    }

    pub fn pin_executable(
        &self,
        owner: &str,
        revision: &ContentRevision,
    ) -> Result<(), ExtensionOperationError> {
        self.store
            .add_reference(&Reference {
                owner: reference_owner(&format!("pin:{owner}")),
                revision: revision.clone(),
            })
            .map_err(|error| ExtensionOperationError::Store(error.to_string()))
    }

    pub fn snapshot_executable(
        &self,
        id: &str,
        executable: impl AsRef<Path>,
    ) -> Result<(ContentRevision, PathBuf), ExtensionOperationError> {
        let identity = QualifiedIdentity::new(id.to_owned())
            .map_err(|error| ExtensionOperationError::Invalid(error.to_string()))?;
        let bytes = fs::read(executable)?;
        let revision = self
            .store
            .retain(&bytes)
            .map_err(|error| ExtensionOperationError::Store(error.to_string()))?;
        let retained = self.materialize(&revision, &bytes)?;
        self.store
            .add_reference(&Reference {
                owner: reference_owner(identity.as_str()),
                revision: revision.clone(),
            })
            .map_err(|error| ExtensionOperationError::Store(error.to_string()))?;
        Ok((revision, retained))
    }

    pub fn remove_working_copy(&self, id: &str) -> Result<(), ExtensionOperationError> {
        let identity = QualifiedIdentity::new(id.to_owned())
            .map_err(|error| ExtensionOperationError::Invalid(error.to_string()))?;
        let path = self.working_root.join(identity.as_str());
        match fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn materialize(
        &self,
        revision: &ContentRevision,
        bytes: &[u8],
    ) -> Result<PathBuf, ExtensionOperationError> {
        fs::create_dir_all(&self.executable_root)?;
        let path = self
            .executable_root
            .join(revision.as_str().trim_start_matches("sha256:"));
        if !path.is_file() {
            fs::write(&path, bytes)?;
            let mut permissions = fs::metadata(&path)?.permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&path, permissions)?;
        }
        Ok(path)
    }
}

fn reference_owner(id: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("extension-{:x}", Sha256::digest(id.as_bytes()))
}

fn run_cargo(source: &Path, arguments: &[&str]) -> Result<(), ExtensionOperationError> {
    let output = Command::new("cargo")
        .args(arguments)
        .current_dir(source)
        .output()
        .map_err(|error| ExtensionOperationError::Command(format!("run cargo: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(ExtensionOperationError::Command(
        stderr.chars().take(16 * 1024).collect(),
    ))
}

fn scaffold_manifest(name: &str) -> String {
    format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"

[dependencies]
prism-extension-sdk = "0.1"
serde_json = "1"
tokio = {{ version = "1", features = ["macros", "rt-multi-thread"] }}
"#
    )
}

fn scaffold_source(id: &str) -> String {
    format!(
        r#"use prism_extension_sdk::{{ExecuteContext, ExecuteFuture, Extension, protocol::*}};

struct MyExtension;

impl Extension for MyExtension {{
    fn id(&self) -> &str {{ "{id}" }}
    fn revision(&self) -> &str {{ env!("CARGO_PKG_VERSION") }}
    fn descriptor(&self) -> ExtensionDescriptor {{ ExtensionDescriptor::default() }}
    fn execute(&self, _context: ExecuteContext, _attempt: AttemptEnvelope) -> ExecuteFuture {{
        Box::pin(async {{ Ok(serde_json::json!({{}})) }})
    }}
}}

#[tokio::main]
async fn main() {{
    if let Err(error) = prism_extension_sdk::serve(MyExtension).await {{
        eprintln!("{{error}}");
        std::process::exit(1);
    }}
}}
"#
    )
}

#[derive(Debug)]
pub enum ExtensionOperationError {
    Invalid(String),
    Io(std::io::Error),
    Command(String),
    Store(String),
    Host(ExtensionHostError),
}
impl fmt::Display for ExtensionOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(value) => value.fmt(f),
            Self::Io(error) => error.fmt(f),
            Self::Command(value) => write!(f, "extension Cargo command failed: {value}"),
            Self::Store(value) => write!(f, "retain extension executable: {value}"),
            Self::Host(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for ExtensionOperationError {}
impl From<std::io::Error> for ExtensionOperationError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<ExtensionHostError> for ExtensionOperationError {
    fn from(value: ExtensionHostError) -> Self {
        Self::Host(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffold_is_an_executable_protocol_package() {
        let root =
            std::env::temp_dir().join(format!("prism-extension-scaffold-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let operations = ExtensionOperations::new(root.join("work"), root.join("state"));
        let path = operations.scaffold("acme.test/example").unwrap();
        assert!(
            fs::read_to_string(path.join("src/main.rs"))
                .unwrap()
                .contains("prism_extension_sdk::serve")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pinned_executable_survives_working_copy_removal() {
        let root =
            std::env::temp_dir().join(format!("prism-extension-retention-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let operations = ExtensionOperations::new(root.join("work"), root.join("state"));
        operations.scaffold("acme.test/example").unwrap();
        let revision = operations.store.retain(b"executable-v1").unwrap();
        operations.pin_executable("run-1", &revision).unwrap();
        operations.remove_working_copy("acme.test/example").unwrap();
        let retained = operations.retained_executable(&revision).unwrap();
        assert_eq!(fs::read(retained).unwrap(), b"executable-v1");
        fs::remove_dir_all(root).unwrap();
    }
}
