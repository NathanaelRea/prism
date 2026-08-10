//! Schema-v2 Workflow Definition parsing, validation, cataloging, and immutable snapshots.

mod condition;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub use condition::{ConditionError, ConditionExpr, ConditionValue};
use prism_extension_protocol::{
    ArtifactSchemaDescriptor, EffectBoundary, ImplementationDescriptor, StepClass,
};
use serde::{Deserialize, Serialize};

use crate::extension::registry::DescriptorRegistry;
use crate::package::{LockedPackage, PackageLock, PackageManifest};
use crate::resource::{
    ContentRevision, ContentStore, QualifiedIdentity, Reference, ResourceError, ResourceKind,
    ResourceScope, TrustStore, discover,
};

pub const DEFINITION_SCHEMA_VERSION: u32 = 2;
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDefinition {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub launch: Vec<LaunchMode>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub inputs: BTreeMap<String, PortDefinition>,
    #[serde(default)]
    pub outputs: BTreeMap<String, PortDefinition>,
    #[serde(default)]
    pub parameters: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub budgets: BudgetDefinition,
    pub steps: Vec<StepDefinition>,
}

impl WorkflowDefinition {
    pub fn parse(source: &str) -> Result<Self, DefinitionError> {
        let definition: Self =
            toml::from_str(source).map_err(|error| DefinitionError::Syntax(error.to_string()))?;
        definition.validate_parsed()
    }

    fn validate_parsed(self) -> Result<Self, DefinitionError> {
        if self.schema_version != DEFINITION_SCHEMA_VERSION {
            return Err(DefinitionError::UnsupportedSchema(self.schema_version));
        }
        QualifiedIdentity::new(self.id.clone())
            .map_err(|_| DefinitionError::InvalidIdentity(self.id.clone()))?;
        if self.name.trim().is_empty() {
            return Err(DefinitionError::InvalidField(
                "name must not be empty".into(),
            ));
        }
        if self.steps.is_empty() {
            return Err(DefinitionError::InvalidField(
                "steps must not be empty".into(),
            ));
        }
        if self.launch.is_empty() {
            return Err(DefinitionError::InvalidField(
                "launch must contain manual, child, or trigger".into(),
            ));
        }
        validate_names("input", self.inputs.keys())?;
        validate_names("output", self.outputs.keys())?;
        if self.inputs.values().any(|input| input.from.is_some()) {
            return Err(DefinitionError::InvalidField(
                "workflow inputs cannot declare from".into(),
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SourceDiagnostic {
    pub path: PathBuf,
    pub message: String,
    pub byte_start: Option<usize>,
    pub byte_end: Option<usize>,
}

pub fn diagnose_source(
    path: impl Into<PathBuf>,
    source: &str,
) -> Result<WorkflowDefinition, Vec<SourceDiagnostic>> {
    let path = path.into();
    let definition: WorkflowDefinition = toml::from_str(source).map_err(|error| {
        let span = error.span();
        vec![SourceDiagnostic {
            path: path.clone(),
            message: error.message().into(),
            byte_start: span.as_ref().map(|span| span.start),
            byte_end: span.map(|span| span.end),
        }]
    })?;
    definition.validate_parsed().map_err(|error| {
        vec![SourceDiagnostic {
            path,
            message: error.to_string(),
            byte_start: None,
            byte_end: None,
        }]
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    Manual,
    Child,
    Trigger,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PortDefinition {
    #[serde(rename = "type")]
    pub schema: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub from_context: bool,
    #[serde(default)]
    pub from: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetDefinition {
    #[serde(default)]
    pub max_child_depth: Option<u32>,
    #[serde(default)]
    pub max_attempts: Option<u32>,
    #[serde(default)]
    pub max_mutations: Option<u32>,
    #[serde(default)]
    pub max_fan_out: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StepDefinition {
    pub id: String,
    pub class: StepClass,
    #[serde(rename = "use", default)]
    pub implementation: Option<String>,
    #[serde(default)]
    pub workflow: Option<String>,
    #[serde(default)]
    pub depends_on: Option<Vec<String>>,
    #[serde(default)]
    pub inputs: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
    #[serde(default)]
    pub settings: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub condition: Option<String>,
    #[serde(default)]
    pub on_unknown: UnknownConditionPolicy,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default)]
    pub target: Option<String>,
    pub skippable: Option<bool>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub on_timeout: TimeoutPolicy,
    #[serde(default)]
    pub retry: RetryDefinition,
    #[serde(default)]
    pub repeat: Option<RepeatDefinition>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownConditionPolicy {
    #[default]
    Wait,
    Skip,
    Fail,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutPolicy {
    #[default]
    Fail,
    InputRequired,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryFailure {
    Transient,
    Timeout,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetryDefinition {
    #[serde(default = "one_attempt")]
    pub max_attempts: u32,
    #[serde(default = "transient_failures")]
    pub on: BTreeSet<RetryFailure>,
    #[serde(default = "default_initial_retry_delay")]
    pub initial_delay_seconds: u64,
    #[serde(default = "default_max_retry_delay")]
    pub max_delay_seconds: u64,
}

impl Default for RetryDefinition {
    fn default() -> Self {
        Self {
            max_attempts: one_attempt(),
            on: transient_failures(),
            initial_delay_seconds: default_initial_retry_delay(),
            max_delay_seconds: default_max_retry_delay(),
        }
    }
}

const fn one_attempt() -> u32 {
    1
}

fn transient_failures() -> BTreeSet<RetryFailure> {
    BTreeSet::from([RetryFailure::Transient])
}

const fn default_initial_retry_delay() -> u64 {
    2
}

const fn default_max_retry_delay() -> u64 {
    60
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepeatDefinition {
    pub until: String,
    pub max_iterations: u32,
    pub on_exhausted: ExhaustedPolicy,
    #[serde(default)]
    pub successor: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExhaustedPolicy {
    InputRequired,
    Approval,
    Fail,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompiledStep {
    pub id: String,
    pub class: StepClass,
    pub implementation: Option<String>,
    pub workflow: Option<String>,
    pub effect_boundary: EffectBoundary,
    pub dependencies: Vec<String>,
    pub inputs: BTreeMap<String, Binding>,
    pub outputs: BTreeMap<String, String>,
    pub settings: BTreeMap<String, serde_json::Value>,
    pub condition: Option<ConditionExpr>,
    pub on_unknown: UnknownConditionPolicy,
    pub capabilities: BTreeSet<String>,
    pub resources: BTreeSet<String>,
    pub target: Option<String>,
    pub skippable: bool,
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub on_timeout: TimeoutPolicy,
    pub retry: RetryDefinition,
    pub repeat: Option<CompiledRepeat>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Binding {
    Reference {
        reference: String,
        schema: String,
    },
    Literal {
        value: serde_json::Value,
    },
    Parameter {
        parameter: String,
        value: serde_json::Value,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompiledRepeat {
    pub until: ConditionExpr,
    pub max_iterations: u32,
    pub on_exhausted: ExhaustedPolicy,
    pub successor: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DefinitionSnapshot {
    pub snapshot_schema_version: u32,
    pub definition: SnapshotDefinition,
    pub sources: BTreeMap<String, SnapshotSource>,
    pub implementations: BTreeMap<String, ResolvedImplementation>,
    pub schemas: BTreeMap<String, serde_json::Value>,
    pub children: BTreeMap<String, Box<DefinitionSnapshot>>,
    pub package_revisions: BTreeMap<String, String>,
    pub capabilities: BTreeSet<String>,
    pub trusted: bool,
    pub digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LaunchCompatibility {
    pub compatible: bool,
    pub context_bindings: BTreeMap<String, String>,
    pub missing_required_inputs: Vec<String>,
}

impl DefinitionSnapshot {
    /// Infer context inputs by exact Artifact schema; ambiguous matches stay missing.
    pub fn launch_compatibility(&self, context: &BTreeMap<String, String>) -> LaunchCompatibility {
        let mut context_bindings = BTreeMap::new();
        let mut missing_required_inputs = Vec::new();
        for (name, input) in &self.definition.inputs {
            let matches: Vec<_> = context
                .iter()
                .filter(|(_, schema)| *schema == &input.schema)
                .map(|(name, _)| name.clone())
                .collect();
            if input.from_context && matches.len() == 1 {
                context_bindings.insert(name.clone(), matches[0].clone());
            } else if input.required {
                missing_required_inputs.push(name.clone());
            }
        }
        LaunchCompatibility {
            compatible: missing_required_inputs.is_empty(),
            context_bindings,
            missing_required_inputs,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotDefinition {
    pub id: String,
    pub name: String,
    pub description: String,
    pub launch: BTreeSet<LaunchMode>,
    pub tags: BTreeSet<String>,
    pub declared_capabilities: BTreeSet<String>,
    pub inputs: BTreeMap<String, PortDefinition>,
    pub outputs: BTreeMap<String, PortDefinition>,
    pub parameters: BTreeMap<String, serde_json::Value>,
    pub budgets: BudgetDefinition,
    pub steps: Vec<CompiledStep>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SnapshotSource {
    pub revision: String,
    pub scope: SnapshotScope,
    pub trusted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotScope {
    Global,
    Repository,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolvedImplementation {
    pub descriptor: ImplementationDescriptor,
    pub executable_revision: String,
    pub trusted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutableResolution {
    pub revision: String,
    pub trusted: bool,
}

#[derive(Clone, Debug)]
struct CatalogEntry {
    definition: WorkflowDefinition,
    path: PathBuf,
    scope: ResourceScope,
    revision: ContentRevision,
    trusted: bool,
    package_id: Option<String>,
}

/// Read-only catalog. Discovery and compilation do not build, fetch, execute, or retain data.
pub struct DefinitionCatalog {
    entries: BTreeMap<String, CatalogEntry>,
    registry: DescriptorRegistry,
    executable_revisions: BTreeMap<String, ExecutableResolution>,
    locked_packages: BTreeMap<String, LockedPackage>,
    available_targets: BTreeSet<String>,
    available_capabilities: Option<BTreeSet<String>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CatalogDefinition {
    pub id: String,
    pub name: String,
    pub description: String,
    pub launch: BTreeSet<LaunchMode>,
    pub path: PathBuf,
    pub revision: String,
    pub trusted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DefinitionUpdate {
    pub id: String,
    pub previous_revision: Option<String>,
    pub current_revision: Option<String>,
}

/// Filesystem authoring operations. Editor/TTY selection remains an application concern.
pub struct DefinitionAuthoringOperations {
    workflow_root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DefinitionMigrationPreview {
    pub path: PathBuf,
    pub from_version: u32,
    pub to_version: u32,
    pub changed: bool,
    pub migrated_source: String,
}

impl DefinitionAuthoringOperations {
    pub fn new(workflow_root: impl Into<PathBuf>) -> Self {
        Self {
            workflow_root: workflow_root.into(),
        }
    }

    pub fn create(&self, id: &str, name: &str) -> Result<PathBuf, DefinitionError> {
        QualifiedIdentity::new(id.to_owned())
            .map_err(|_| DefinitionError::InvalidIdentity(id.into()))?;
        let path = self.path_for(id)?;
        if path.exists() {
            return Err(DefinitionError::InvalidField(format!(
                "workflow source {} already exists",
                path.display()
            )));
        }
        fs::create_dir_all(&self.workflow_root)?;
        fs::write(&path, commented_template(id, name))?;
        Ok(path)
    }

    pub fn copy(
        &self,
        source: &Path,
        new_id: &str,
        new_name: &str,
    ) -> Result<PathBuf, DefinitionError> {
        let source = fs::read_to_string(source)?;
        let mut definition = WorkflowDefinition::parse(&source)?;
        QualifiedIdentity::new(new_id.to_owned())
            .map_err(|_| DefinitionError::InvalidIdentity(new_id.into()))?;
        definition.id = new_id.into();
        definition.name = new_name.into();
        let path = self.path_for(new_id)?;
        if path.exists() {
            return Err(DefinitionError::InvalidField(format!(
                "workflow source {} already exists",
                path.display()
            )));
        }
        fs::create_dir_all(&self.workflow_root)?;
        let encoded = toml::to_string_pretty(&definition)
            .map_err(|error| DefinitionError::Serialization(error.to_string()))?;
        fs::write(&path, encoded)?;
        Ok(path)
    }

    pub fn edit_path(&self, id: &str) -> Result<PathBuf, DefinitionError> {
        let path = self.path_for(id)?;
        if !path.is_file() {
            return Err(DefinitionError::UnknownWorkflow(id.into()));
        }
        Ok(path)
    }

    /// Replace a source only when its canonical base revision still matches, preserving a backup.
    pub fn update(
        &self,
        path: &Path,
        expected_revision: &str,
        new_source: &str,
    ) -> Result<PathBuf, DefinitionError> {
        let current_source = fs::read_to_string(path)?;
        let current = WorkflowDefinition::parse(&current_source)?;
        let actual = canonical_source_revision(&current)?;
        if actual.as_str() != expected_revision {
            return Err(DefinitionError::InvalidField(format!(
                "workflow update base changed: expected {expected_revision}, got {actual}"
            )));
        }
        let replacement = WorkflowDefinition::parse(new_source)?;
        if replacement.id != current.id {
            return Err(DefinitionError::InvalidField(format!(
                "workflow update cannot replace identity {} with {}; use copy to create a fork",
                current.id, replacement.id
            )));
        }
        let backup = path.with_extension(format!(
            "toml.pre-update-{}",
            actual.as_str().trim_start_matches("sha256:")
        ));
        if backup.exists() {
            return Err(DefinitionError::InvalidField(format!(
                "workflow update backup {} already exists",
                backup.display()
            )));
        }
        fs::copy(path, &backup)?;
        let candidate = path.with_extension(format!("toml.{}.tmp", std::process::id()));
        fs::write(&candidate, new_source)?;
        if let Err(error) = fs::rename(&candidate, path) {
            let _ = fs::remove_file(candidate);
            return Err(error.into());
        }
        Ok(backup)
    }

    pub fn migration_preview(
        &self,
        path: &Path,
    ) -> Result<DefinitionMigrationPreview, DefinitionError> {
        let source = fs::read_to_string(path)?;
        let value: toml::Value =
            toml::from_str(&source).map_err(|error| DefinitionError::Syntax(error.to_string()))?;
        let version = value
            .get("schema_version")
            .and_then(toml::Value::as_integer)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| DefinitionError::InvalidField("missing schema_version".into()))?;
        let (changed, migrated_source) = match version {
            DEFINITION_SCHEMA_VERSION => {
                WorkflowDefinition::parse(&source)?;
                (false, source.clone())
            }
            1 => {
                let migrated = migrate_v1(value)?;
                WorkflowDefinition::parse(&migrated)?;
                (true, migrated)
            }
            version => return Err(DefinitionError::UnsupportedSchema(version)),
        };
        Ok(DefinitionMigrationPreview {
            path: path.into(),
            from_version: version,
            to_version: DEFINITION_SCHEMA_VERSION,
            changed,
            migrated_source,
        })
    }

    pub fn apply_migration(
        &self,
        preview: &DefinitionMigrationPreview,
    ) -> Result<Option<PathBuf>, DefinitionError> {
        if !preview.changed {
            return Ok(None);
        }
        let backup = preview.path.with_extension("toml.pre-v2-backup");
        if backup.exists() {
            return Err(DefinitionError::InvalidField(format!(
                "migration backup {} already exists",
                backup.display()
            )));
        }
        fs::copy(&preview.path, &backup)?;
        let candidate = preview
            .path
            .with_extension(format!("toml.{}.tmp", std::process::id()));
        fs::write(&candidate, &preview.migrated_source)?;
        if let Err(error) = fs::rename(&candidate, &preview.path) {
            let _ = fs::remove_file(candidate);
            return Err(error.into());
        }
        Ok(Some(backup))
    }

    fn path_for(&self, id: &str) -> Result<PathBuf, DefinitionError> {
        let identity = QualifiedIdentity::new(id.to_owned())
            .map_err(|_| DefinitionError::InvalidIdentity(id.into()))?;
        let file = identity
            .as_str()
            .replace(['/', '.'], "-")
            .trim_matches('-')
            .to_owned();
        Ok(self.workflow_root.join(format!("{file}.toml")))
    }
}

fn migrate_v1(mut source: toml::Value) -> Result<String, DefinitionError> {
    let root = source
        .as_table_mut()
        .ok_or_else(|| DefinitionError::InvalidField("definition root must be a table".into()))?;
    root.insert("schema_version".into(), toml::Value::Integer(2));
    root.entry("launch")
        .or_insert_with(|| toml::Value::Array(vec![toml::Value::String("manual".into())]));
    if let Some(inputs) = root.get_mut("inputs").and_then(toml::Value::as_table_mut) {
        for (_, input) in inputs.iter_mut() {
            let table = input.as_table_mut().ok_or_else(|| {
                DefinitionError::InvalidField("schema-v1 input must be a table".into())
            })?;
            if let Some(artifact_type) = table.remove("artifact_type") {
                table.insert("type".into(), normalize_v1_type(artifact_type)?);
            }
            table
                .entry("required")
                .or_insert(toml::Value::Boolean(true));
        }
    }
    let steps = root
        .get_mut("steps")
        .and_then(toml::Value::as_array_mut)
        .ok_or_else(|| DefinitionError::InvalidField("schema-v1 steps must be an array".into()))?;
    for step in steps {
        let table = step.as_table_mut().ok_or_else(|| {
            DefinitionError::InvalidField("schema-v1 Step must be a table".into())
        })?;
        table
            .entry("skippable")
            .or_insert(toml::Value::Boolean(false));
        let class = table
            .get("class")
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if class == "workflow_call" {
            table.remove("implementation");
            let child = table
                .remove("child_workflow")
                .and_then(|value| value.as_table().cloned())
                .and_then(|mut child| child.remove("qualified_name"))
                .and_then(|value| value.as_str().map(str::to_owned))
                .ok_or_else(|| {
                    DefinitionError::InvalidField(
                        "schema-v1 workflow_call has no child qualified_name".into(),
                    )
                })?;
            table.insert(
                "workflow".into(),
                toml::Value::String(normalize_v1_identity(&child)),
            );
        } else if let Some(implementation) = table.remove("implementation") {
            let implementation = implementation.as_str().ok_or_else(|| {
                DefinitionError::InvalidField("schema-v1 implementation must be a string".into())
            })?;
            table.insert(
                "use".into(),
                toml::Value::String(normalize_v1_identity(implementation)),
            );
        }
        if let Some(inputs) = table.get_mut("inputs").and_then(toml::Value::as_table_mut) {
            for (_, input) in inputs.iter_mut() {
                let binding = input
                    .as_table()
                    .and_then(|table| table.get("from"))
                    .and_then(toml::Value::as_str)
                    .ok_or_else(|| {
                        DefinitionError::InvalidField(
                            "schema-v1 Step input has no from binding".into(),
                        )
                    })?;
                *input = toml::Value::String(normalize_v1_binding(binding));
            }
        }
        if let Some(outputs) = table.get_mut("outputs").and_then(toml::Value::as_table_mut) {
            for (_, output) in outputs.iter_mut() {
                let artifact_type = output
                    .as_table()
                    .and_then(|table| table.get("artifact_type"))
                    .cloned()
                    .ok_or_else(|| {
                        DefinitionError::InvalidField(
                            "schema-v1 Step output has no artifact_type".into(),
                        )
                    })?;
                *output = normalize_v1_type(artifact_type)?;
            }
        }
        if let Some(condition) = table.get_mut("condition")
            && let Some(value) = condition.as_str()
        {
            *condition = toml::Value::String(value.replace("step.", "steps."));
        }
        if let Some(continuation) = table.remove("continuation") {
            let settings = table
                .entry("settings")
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            settings
                .as_table_mut()
                .expect("settings initialized as table")
                .insert("continuation".into(), continuation);
        }
    }
    toml::to_string_pretty(&source)
        .map_err(|error| DefinitionError::Serialization(error.to_string()))
}

fn normalize_v1_type(value: toml::Value) -> Result<toml::Value, DefinitionError> {
    let value = value.as_str().ok_or_else(|| {
        DefinitionError::InvalidField("schema-v1 artifact_type must be a string".into())
    })?;
    Ok(toml::Value::String(normalize_v1_identity(value)))
}

fn normalize_v1_identity(value: &str) -> String {
    if let Some(value) = value.strip_prefix("builtin:") {
        format!("prism.legacy/{}", value.replace('@', "-v"))
    } else {
        value.into()
    }
}

fn normalize_v1_binding(value: &str) -> String {
    if let Some(value) = value.strip_prefix("run.") {
        format!("inputs.{value}")
    } else if let Some((step, output)) = value.split_once('.') {
        format!("steps.{step}.outputs.{output}")
    } else {
        value.into()
    }
}

impl DefinitionCatalog {
    pub fn discover(
        global_root: &Path,
        repository_root: Option<&Path>,
        trust_store: &TrustStore,
        registry: DescriptorRegistry,
        executable_revisions: BTreeMap<String, ExecutableResolution>,
    ) -> Result<Self, DefinitionError> {
        let resources = discover(global_root, repository_root)?;
        let locked_packages = load_locked_packages(global_root, repository_root)?;
        let mut entries = BTreeMap::new();
        for resource in resources
            .into_iter()
            .filter(|resource| resource.kind == ResourceKind::Workflow)
        {
            let bytes = fs::read(&resource.path)?;
            let source = std::str::from_utf8(&bytes).map_err(|error| {
                DefinitionError::Source(resource.path.clone(), error.to_string())
            })?;
            let definition =
                diagnose_source(resource.path.clone(), source).map_err(|diagnostics| {
                    let diagnostic = &diagnostics[0];
                    DefinitionError::Source(diagnostic.path.clone(), diagnostic.message.clone())
                })?;
            if resource.identity.as_str() != definition.id {
                return Err(DefinitionError::Source(
                    resource.path,
                    "discovered identity does not match definition id".into(),
                ));
            }
            let revision = canonical_source_revision(&definition)?;
            let trusted = trust_store.is_trusted(resource.scope, repository_root, &revision)?;
            let package_id = package_id_for_resource(&resource.path, global_root, repository_root)?;
            entries.insert(
                definition.id.clone(),
                CatalogEntry {
                    definition,
                    path: resource.path,
                    scope: resource.scope,
                    revision,
                    trusted,
                    package_id,
                },
            );
        }
        Ok(Self {
            entries,
            registry,
            executable_revisions,
            locked_packages,
            available_targets: BTreeSet::from(["local".into()]),
            available_capabilities: None,
        })
    }

    pub fn from_sources(
        sources: impl IntoIterator<Item = (String, String)>,
        registry: DescriptorRegistry,
    ) -> Result<Self, DefinitionError> {
        let mut entries = BTreeMap::new();
        for (label, source) in sources {
            let definition = WorkflowDefinition::parse(&source).map_err(|error| {
                DefinitionError::Source(PathBuf::from(&label), error.to_string())
            })?;
            if entries.contains_key(&definition.id) {
                return Err(DefinitionError::DuplicateIdentity(definition.id));
            }
            let revision = canonical_source_revision(&definition)?;
            entries.insert(
                definition.id.clone(),
                CatalogEntry {
                    definition,
                    path: PathBuf::from(label),
                    scope: ResourceScope::Global,
                    revision,
                    trusted: true,
                    package_id: None,
                },
            );
        }
        let executable_revisions = registry
            .implementations()
            .map(|implementation| {
                let bytes = serde_json::to_vec(implementation).expect("descriptor serializes");
                (
                    implementation.id.clone(),
                    ExecutableResolution {
                        revision: format!("fixture:{}", ContentRevision::digest(&bytes)),
                        trusted: true,
                    },
                )
            })
            .collect();
        Ok(Self {
            entries,
            registry,
            executable_revisions,
            locked_packages: BTreeMap::new(),
            available_targets: BTreeSet::from(["local".into()]),
            available_capabilities: None,
        })
    }

    pub fn with_available_targets(mut self, targets: BTreeSet<String>) -> Self {
        self.available_targets = targets;
        self.available_targets.insert("local".into());
        self
    }

    pub fn with_available_capabilities(mut self, capabilities: BTreeSet<String>) -> Self {
        self.available_capabilities = Some(capabilities);
        self
    }

    pub fn list(&self) -> Vec<CatalogDefinition> {
        self.entries
            .values()
            .map(|entry| CatalogDefinition {
                id: entry.definition.id.clone(),
                name: entry.definition.name.clone(),
                description: entry.definition.description.clone(),
                launch: entry.definition.launch.iter().copied().collect(),
                path: entry.path.clone(),
                revision: entry.revision.to_string(),
                trusted: entry.trusted,
            })
            .collect()
    }

    pub fn source(&self, id: &str) -> Option<&WorkflowDefinition> {
        self.entries.get(id).map(|entry| &entry.definition)
    }

    pub fn updates(&self, previous_revisions: &BTreeMap<String, String>) -> Vec<DefinitionUpdate> {
        let ids: BTreeSet<_> = self
            .entries
            .keys()
            .chain(previous_revisions.keys())
            .cloned()
            .collect();
        ids.into_iter()
            .filter_map(|id| {
                let previous_revision = previous_revisions.get(&id).cloned();
                let current_revision = self
                    .entries
                    .get(&id)
                    .map(|entry| entry.revision.to_string());
                (previous_revision != current_revision).then_some(DefinitionUpdate {
                    id,
                    previous_revision,
                    current_revision,
                })
            })
            .collect()
    }

    pub fn compile(&self, id: &str) -> Result<DefinitionSnapshot, DefinitionError> {
        self.compile_inner(id, &mut Vec::new())
    }

    pub fn preview(&self, id: &str) -> Result<DefinitionSnapshot, DefinitionError> {
        self.compile(id)
    }

    pub fn retain(
        &self,
        snapshot: &DefinitionSnapshot,
        store: &ContentStore,
        owner: &str,
    ) -> Result<ContentRevision, DefinitionError> {
        let bytes = serde_json::to_vec(snapshot)
            .map_err(|error| DefinitionError::Serialization(error.to_string()))?;
        let revision = store.retain(&bytes)?;
        store.add_reference(&Reference {
            owner: owner.into(),
            revision: revision.clone(),
        })?;
        Ok(revision)
    }

    fn compile_inner(
        &self,
        id: &str,
        active: &mut Vec<String>,
    ) -> Result<DefinitionSnapshot, DefinitionError> {
        if active.iter().any(|value| value == id) {
            active.push(id.into());
            return Err(DefinitionError::Recursion(active.join(" -> ")));
        }
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| DefinitionError::UnknownWorkflow(id.into()))?;
        active.push(id.into());
        let result = self.compile_entry(entry, active);
        active.pop();
        result
    }

    fn compile_entry(
        &self,
        entry: &CatalogEntry,
        active: &mut Vec<String>,
    ) -> Result<DefinitionSnapshot, DefinitionError> {
        let definition = &entry.definition;
        let dependencies = expand_and_validate_graph(&definition.steps)?;
        let ancestors = graph_ancestors(&definition.steps, &dependencies);
        let parameters: BTreeMap<String, serde_json::Value> = definition
            .parameters
            .iter()
            .map(|(key, value)| toml_to_json(value).map(|value| (key.clone(), value)))
            .collect::<Result<_, _>>()?;
        let mut known_schemas: BTreeMap<String, String> = definition
            .inputs
            .iter()
            .map(|(name, port)| (format!("inputs.{name}"), port.schema.clone()))
            .collect();
        let mut children = BTreeMap::new();
        let mut implementations = BTreeMap::new();
        let mut schemas = BTreeMap::new();
        let mut capabilities: BTreeSet<String> = definition.capabilities.iter().cloned().collect();
        let mut compiled_steps = Vec::new();

        for index in topological_order(&definition.steps, &dependencies) {
            let step = &definition.steps[index];
            validate_step_id(&step.id)?;
            if let Some(target) = &step.target
                && !self.available_targets.contains(target)
            {
                return Err(DefinitionError::UnavailableTarget {
                    step: step.id.clone(),
                    target: target.clone(),
                });
            }
            let dependency_set = ancestors.get(&step.id).cloned().unwrap_or_default();
            let (descriptor, child) = self.resolve_step(step, active)?;
            let effect_boundary = descriptor
                .map(|descriptor| descriptor.effect_boundary)
                .unwrap_or_default();
            let expected_inputs: BTreeMap<String, String>;
            let required_inputs: BTreeSet<String>;
            let declared_outputs: BTreeMap<String, String>;
            let mut resolved_target = step.target.clone();
            let mut resolved_capabilities: BTreeSet<String> =
                step.capabilities.iter().cloned().collect();
            if let Some(descriptor) = descriptor {
                if !descriptor.targets.is_empty() {
                    let target = resolved_target
                        .clone()
                        .or_else(|| {
                            descriptor
                                .targets
                                .iter()
                                .find(|target| *target == "local")
                                .cloned()
                        })
                        .ok_or_else(|| {
                            DefinitionError::Step(
                                step.id.clone(),
                                format!(
                                    "implementation {} requires an explicit compatible target",
                                    descriptor.id
                                ),
                            )
                        })?;
                    if !descriptor.targets.contains(&target) {
                        return Err(DefinitionError::Step(
                            step.id.clone(),
                            format!(
                                "implementation {} does not support target {target}",
                                descriptor.id
                            ),
                        ));
                    }
                    if !self.available_targets.contains(&target) {
                        return Err(DefinitionError::UnavailableTarget {
                            step: step.id.clone(),
                            target,
                        });
                    }
                    resolved_target = Some(target);
                }
                expected_inputs = descriptor
                    .inputs
                    .iter()
                    .map(|port| (port.name.clone(), port.schema.clone()))
                    .collect();
                required_inputs = descriptor
                    .inputs
                    .iter()
                    .filter(|port| port.required)
                    .map(|port| port.name.clone())
                    .collect();
                declared_outputs = descriptor
                    .outputs
                    .iter()
                    .map(|port| (port.name.clone(), port.schema.clone()))
                    .collect();
                resolved_capabilities.extend(descriptor.capabilities.iter().cloned());
                let executable = self
                    .executable_revisions
                    .get(&descriptor.id)
                    .cloned()
                    .ok_or_else(|| {
                        DefinitionError::UnknownExecutableRevision(descriptor.id.clone())
                    })?;
                implementations.insert(
                    descriptor.id.clone(),
                    ResolvedImplementation {
                        executable_revision: executable.revision,
                        trusted: executable.trusted,
                        descriptor: descriptor.clone(),
                    },
                );
            } else if let Some(child) = &child {
                expected_inputs = child
                    .definition
                    .inputs
                    .iter()
                    .map(|(name, port)| (name.clone(), port.schema.clone()))
                    .collect();
                required_inputs = child
                    .definition
                    .inputs
                    .iter()
                    .filter(|(_, port)| port.required)
                    .map(|(name, _)| name.clone())
                    .collect();
                declared_outputs = child
                    .definition
                    .outputs
                    .iter()
                    .map(|(name, port)| (name.clone(), port.schema.clone()))
                    .collect();
                resolved_capabilities.extend(child.capabilities.iter().cloned());
            } else {
                unreachable!();
            }
            capabilities.extend(resolved_capabilities.iter().cloned());
            if step.retry.max_attempts == 0 {
                return Err(DefinitionError::Step(
                    step.id.clone(),
                    "retry.max_attempts is the total Attempt count and must be at least 1".into(),
                ));
            }
            if step.retry.initial_delay_seconds > step.retry.max_delay_seconds {
                return Err(DefinitionError::Step(
                    step.id.clone(),
                    "retry.initial_delay_seconds cannot exceed retry.max_delay_seconds".into(),
                ));
            }
            if step.retry.max_attempts > 1 && effect_boundary == EffectBoundary::Unbrokered {
                return Err(DefinitionError::Step(
                    step.id.clone(),
                    "unbrokered workspace mutations cannot be retried automatically; recover or resume the existing Attempt instead".into(),
                ));
            }
            if step.timeout_seconds.is_none() && step.on_timeout != TimeoutPolicy::Fail {
                return Err(DefinitionError::Step(
                    step.id.clone(),
                    "on_timeout requires timeout_seconds".into(),
                ));
            }
            let inputs = compile_bindings(
                &definition.id,
                step,
                &expected_inputs,
                &known_schemas,
                &dependency_set,
                &self.registry,
                &parameters,
            )?;
            for required in required_inputs {
                if !inputs.contains_key(&required) {
                    return Err(DefinitionError::Step(
                        step.id.clone(),
                        format!("missing required input {required}"),
                    ));
                }
            }
            let outputs = if step.outputs.is_empty() {
                declared_outputs.clone()
            } else {
                for (name, schema) in &step.outputs {
                    if declared_outputs.get(name) != Some(schema) {
                        return Err(DefinitionError::Step(
                            step.id.clone(),
                            format!("output {name} does not match implementation schema {schema}"),
                        ));
                    }
                }
                step.outputs.clone()
            };
            for (name, schema_id) in &outputs {
                known_schemas.insert(
                    format!("steps.{}.outputs.{name}", step.id),
                    schema_id.clone(),
                );
                add_schema(&self.registry, schema_id, &mut schemas)?;
            }
            for schema_id in expected_inputs.values() {
                add_schema(&self.registry, schema_id, &mut schemas)?;
            }
            let condition = step
                .condition
                .as_deref()
                .map(ConditionExpr::parse)
                .transpose()
                .map_err(|error| DefinitionError::Step(step.id.clone(), error.to_string()))?;
            if let Some(condition) = &condition {
                validate_expression_refs(
                    condition,
                    &known_schemas,
                    &parameters,
                    &dependency_set,
                    &step.id,
                )?;
                validate_condition_type(
                    condition,
                    &known_schemas,
                    &parameters,
                    &self.registry,
                    &step.id,
                )?;
            }
            let repeat = compile_repeat(
                step,
                child.as_deref(),
                &known_schemas,
                &parameters,
                &dependency_set,
                &self.registry,
            )?;
            if let Some(child) = child {
                children.insert(step.workflow.clone().expect("child has workflow"), child);
            }
            let settings = step
                .settings
                .iter()
                .map(|(key, value)| toml_to_json(value).map(|value| (key.clone(), value)))
                .collect::<Result<_, _>>()?;
            compiled_steps.push(CompiledStep {
                id: step.id.clone(),
                class: step.class,
                implementation: step.implementation.clone(),
                workflow: step.workflow.clone(),
                effect_boundary,
                dependencies: dependencies[index].clone(),
                inputs,
                outputs,
                settings,
                condition,
                on_unknown: step.on_unknown,
                capabilities: resolved_capabilities,
                resources: step.resources.iter().cloned().collect(),
                target: resolved_target,
                skippable: step.skippable.ok_or_else(|| {
                    DefinitionError::Step(step.id.clone(), "skippable must be explicit".into())
                })?,
                timeout_seconds: step.timeout_seconds,
                on_timeout: step.on_timeout,
                retry: step.retry.clone(),
                repeat,
            });
        }
        for port in definition
            .inputs
            .values()
            .chain(definition.outputs.values())
        {
            add_schema(&self.registry, &port.schema, &mut schemas)?;
        }
        for (name, output) in &definition.outputs {
            let reference = output.from.as_deref().ok_or_else(|| {
                DefinitionError::InvalidField(format!("workflow output {name} requires from"))
            })?;
            let actual = known_schemas.get(reference).ok_or_else(|| {
                DefinitionError::InvalidField(format!(
                    "workflow output {name} references unknown binding {reference}"
                ))
            })?;
            if actual != &output.schema {
                return Err(DefinitionError::InvalidField(format!(
                    "workflow output {name} expects {}, got {actual}",
                    output.schema
                )));
            }
        }
        validate_terminal_reachability(&compiled_steps)?;
        validate_budget_proof(definition, &compiled_steps, &children)?;
        let declared_capabilities: BTreeSet<String> = definition
            .capabilities
            .iter()
            .chain(
                definition
                    .steps
                    .iter()
                    .flat_map(|step| step.capabilities.iter()),
            )
            .cloned()
            .collect();
        if let Some(capability) = capabilities.difference(&declared_capabilities).next() {
            return Err(DefinitionError::InvalidField(format!(
                "transitive capability {capability} is not disclosed by the definition or calling Step"
            )));
        }
        if let Some(available) = &self.available_capabilities
            && let Some(capability) = capabilities.difference(available).next()
        {
            return Err(DefinitionError::InvalidField(format!(
                "required capability {capability} is unavailable"
            )));
        }
        let snapshot_definition = SnapshotDefinition {
            id: definition.id.clone(),
            name: definition.name.clone(),
            description: definition.description.clone(),
            launch: definition.launch.iter().copied().collect(),
            tags: definition.tags.iter().cloned().collect(),
            declared_capabilities: definition.capabilities.iter().cloned().collect(),
            inputs: definition.inputs.clone(),
            outputs: definition.outputs.clone(),
            parameters,
            budgets: definition.budgets.clone(),
            steps: compiled_steps,
        };
        let mut sources = BTreeMap::new();
        let mut package_revisions = self.package_closure(entry.package_id.as_deref())?;
        sources.insert(
            definition.id.clone(),
            SnapshotSource {
                revision: entry.revision.to_string(),
                scope: match entry.scope {
                    ResourceScope::Global => SnapshotScope::Global,
                    ResourceScope::Repository => SnapshotScope::Repository,
                },
                trusted: entry.trusted,
            },
        );
        for child in children.values() {
            sources.extend(child.sources.clone());
            schemas.extend(child.schemas.clone());
            implementations.extend(child.implementations.clone());
            package_revisions.extend(child.package_revisions.clone());
        }
        let trusted = sources.values().all(|source| source.trusted)
            && implementations
                .values()
                .all(|implementation| implementation.trusted);
        let mut snapshot = DefinitionSnapshot {
            snapshot_schema_version: SNAPSHOT_SCHEMA_VERSION,
            definition: snapshot_definition,
            sources,
            implementations,
            schemas,
            children,
            package_revisions,
            capabilities,
            trusted,
            digest: String::new(),
        };
        let canonical = serde_json::to_vec(&snapshot)
            .map_err(|error| DefinitionError::Serialization(error.to_string()))?;
        snapshot.digest = ContentRevision::digest(&canonical).to_string();
        Ok(snapshot)
    }

    fn resolve_step(
        &self,
        step: &StepDefinition,
        active: &mut Vec<String>,
    ) -> Result<
        (
            Option<&ImplementationDescriptor>,
            Option<Box<DefinitionSnapshot>>,
        ),
        DefinitionError,
    > {
        if step.class == StepClass::WorkflowCall {
            if step.implementation.is_some() {
                return Err(DefinitionError::Step(
                    step.id.clone(),
                    "workflow_call cannot use an implementation".into(),
                ));
            }
            let workflow = step.workflow.as_deref().ok_or_else(|| {
                DefinitionError::Step(step.id.clone(), "workflow_call requires workflow".into())
            })?;
            let child_entry = self
                .entries
                .get(workflow)
                .ok_or_else(|| DefinitionError::UnknownWorkflow(workflow.into()))?;
            if !child_entry.definition.launch.contains(&LaunchMode::Child) {
                return Err(DefinitionError::Step(
                    step.id.clone(),
                    format!("workflow {workflow} is not child-launchable"),
                ));
            }
            let child = self.compile_inner(workflow, active)?;
            return Ok((None, Some(Box::new(child))));
        }
        if step.workflow.is_some() {
            return Err(DefinitionError::Step(
                step.id.clone(),
                "only workflow_call may name workflow".into(),
            ));
        }
        let implementation = step
            .implementation
            .as_deref()
            .ok_or_else(|| DefinitionError::Step(step.id.clone(), "step requires use".into()))?;
        let descriptor = self
            .registry
            .implementation(implementation)
            .ok_or_else(|| DefinitionError::UnknownImplementation(implementation.into()))?;
        if descriptor.class != step.class {
            return Err(DefinitionError::Step(
                step.id.clone(),
                format!(
                    "implementation {implementation} has class {:?}, expected {:?}",
                    descriptor.class, step.class
                ),
            ));
        }
        Ok((Some(descriptor), None))
    }

    fn package_closure(
        &self,
        root: Option<&str>,
    ) -> Result<BTreeMap<String, String>, DefinitionError> {
        let Some(root) = root else {
            return Ok(BTreeMap::new());
        };
        let mut pending = vec![root.to_owned()];
        let mut closure = BTreeMap::new();
        while let Some(id) = pending.pop() {
            if closure.contains_key(&id) {
                continue;
            }
            let package = self.locked_packages.get(&id).ok_or_else(|| {
                DefinitionError::InvalidField(format!(
                    "package resource {id} has no exact package.lock entry"
                ))
            })?;
            closure.insert(id, package.revision.clone());
            pending.extend(package.dependencies.iter().cloned());
        }
        Ok(closure)
    }
}

fn load_locked_packages(
    global_root: &Path,
    repository_root: Option<&Path>,
) -> Result<BTreeMap<String, LockedPackage>, DefinitionError> {
    let mut output = BTreeMap::new();
    for root in std::iter::once(global_root).chain(repository_root) {
        let path = root.join("package.lock");
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let lock = PackageLock::parse(&source)
            .map_err(|error| DefinitionError::Source(path.clone(), error.to_string()))?;
        for package in lock.packages {
            if output.insert(package.id.clone(), package).is_some() {
                return Err(DefinitionError::DuplicateIdentity(format!(
                    "package lock entry {}",
                    path.display()
                )));
            }
        }
    }
    Ok(output)
}

fn package_id_for_resource(
    path: &Path,
    global_root: &Path,
    repository_root: Option<&Path>,
) -> Result<Option<String>, DefinitionError> {
    let package_roots = std::iter::once(global_root.join("packages"))
        .chain(repository_root.map(|root| root.join("packages")));
    for packages in package_roots {
        if !path.starts_with(&packages) {
            continue;
        }
        let relative = path.strip_prefix(&packages).expect("prefix checked");
        let Some(directory) = relative.components().next() else {
            return Ok(None);
        };
        let manifest_path = packages.join(directory).join("prism-package.toml");
        let manifest = PackageManifest::parse(&fs::read_to_string(&manifest_path)?)
            .map_err(|error| DefinitionError::Source(manifest_path, error.to_string()))?;
        return Ok(Some(manifest.id));
    }
    Ok(None)
}

fn add_schema(
    registry: &DescriptorRegistry,
    id: &str,
    output: &mut BTreeMap<String, serde_json::Value>,
) -> Result<(), DefinitionError> {
    let ArtifactSchemaDescriptor { schema, .. } = registry
        .artifact_schema(id)
        .ok_or_else(|| DefinitionError::UnknownSchema(id.into()))?;
    output.insert(id.into(), schema.clone());
    Ok(())
}

fn expand_and_validate_graph(
    steps: &[StepDefinition],
) -> Result<Vec<Vec<String>>, DefinitionError> {
    let mut ids = BTreeSet::new();
    for step in steps {
        validate_step_id(&step.id)?;
        if !ids.insert(step.id.clone()) {
            return Err(DefinitionError::DuplicateStep(step.id.clone()));
        }
    }
    let dependencies: Vec<Vec<String>> = steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            step.depends_on.clone().unwrap_or_else(|| {
                if index == 0 {
                    Vec::new()
                } else {
                    vec![steps[index - 1].id.clone()]
                }
            })
        })
        .collect();
    for (step, dependencies) in steps.iter().zip(&dependencies) {
        let mut unique = BTreeSet::new();
        for dependency in dependencies {
            if dependency == &step.id {
                return Err(DefinitionError::Cycle(step.id.clone()));
            }
            if !ids.contains(dependency) {
                return Err(DefinitionError::MissingDependency {
                    step: step.id.clone(),
                    dependency: dependency.clone(),
                });
            }
            if !unique.insert(dependency) {
                return Err(DefinitionError::Step(
                    step.id.clone(),
                    format!("duplicate dependency {dependency}"),
                ));
            }
        }
    }
    fn visit(
        id: &str,
        steps: &[StepDefinition],
        deps: &[Vec<String>],
        active: &mut BTreeSet<String>,
        done: &mut BTreeSet<String>,
    ) -> Result<(), DefinitionError> {
        if done.contains(id) {
            return Ok(());
        }
        if !active.insert(id.into()) {
            return Err(DefinitionError::Cycle(id.into()));
        }
        let index = steps
            .iter()
            .position(|step| step.id == id)
            .expect("known step");
        for dependency in &deps[index] {
            visit(dependency, steps, deps, active, done)?;
        }
        active.remove(id);
        done.insert(id.into());
        Ok(())
    }
    let mut done = BTreeSet::new();
    for step in steps {
        visit(
            &step.id,
            steps,
            &dependencies,
            &mut BTreeSet::new(),
            &mut done,
        )?;
    }
    Ok(dependencies)
}

fn graph_ancestors(
    steps: &[StepDefinition],
    dependencies: &[Vec<String>],
) -> BTreeMap<String, BTreeSet<String>> {
    fn collect(
        id: &str,
        steps: &[StepDefinition],
        dependencies: &[Vec<String>],
        output: &mut BTreeSet<String>,
    ) {
        let index = steps
            .iter()
            .position(|step| step.id == id)
            .expect("validated dependency has a Step");
        for dependency in &dependencies[index] {
            if output.insert(dependency.clone()) {
                collect(dependency, steps, dependencies, output);
            }
        }
    }
    steps
        .iter()
        .map(|step| {
            let mut output = BTreeSet::new();
            collect(&step.id, steps, dependencies, &mut output);
            (step.id.clone(), output)
        })
        .collect()
}

fn topological_order(steps: &[StepDefinition], dependencies: &[Vec<String>]) -> Vec<usize> {
    let mut remaining: BTreeSet<usize> = (0..steps.len()).collect();
    let mut emitted = BTreeSet::new();
    let mut output = Vec::with_capacity(steps.len());
    while !remaining.is_empty() {
        let next = remaining
            .iter()
            .copied()
            .find(|index| {
                dependencies[*index]
                    .iter()
                    .all(|dependency| emitted.contains(dependency))
            })
            .expect("acyclic graph always has a ready Step");
        remaining.remove(&next);
        emitted.insert(steps[next].id.clone());
        output.push(next);
    }
    output
}

fn compile_bindings(
    _workflow: &str,
    step: &StepDefinition,
    expected: &BTreeMap<String, String>,
    known: &BTreeMap<String, String>,
    ancestors: &BTreeSet<String>,
    registry: &DescriptorRegistry,
    parameters: &BTreeMap<String, serde_json::Value>,
) -> Result<BTreeMap<String, Binding>, DefinitionError> {
    let mut output = BTreeMap::new();
    for (name, schema) in expected {
        let Some(value) = step.inputs.get(name) else {
            // Requiredness is checked by descriptors before reaching here only when a binding is absent.
            continue;
        };
        let binding = if let Some(parameter) = value
            .as_str()
            .and_then(|value| value.strip_prefix("parameters."))
        {
            let value = parameters.get(parameter).ok_or_else(|| {
                DefinitionError::Step(step.id.clone(), format!("unknown parameter {parameter}"))
            })?;
            let schema_value = &registry
                .artifact_schema(schema)
                .ok_or_else(|| DefinitionError::UnknownSchema(schema.clone()))?
                .schema;
            if !literal_matches_schema(value, schema_value) {
                return Err(DefinitionError::Step(
                    step.id.clone(),
                    format!("parameter {parameter} does not match {schema}"),
                ));
            }
            Binding::Parameter {
                parameter: parameter.into(),
                value: value.clone(),
            }
        } else if let Some(reference) = value.as_str().filter(|value| is_reference(value)) {
            let actual = known.get(reference).ok_or_else(|| {
                DefinitionError::Step(
                    step.id.clone(),
                    format!("unknown binding reference {reference}"),
                )
            })?;
            validate_producer(reference, ancestors, &step.id)?;
            if actual != schema {
                return Err(DefinitionError::Step(
                    step.id.clone(),
                    format!("binding {name} expects {schema}, got {actual}"),
                ));
            }
            Binding::Reference {
                reference: reference.into(),
                schema: schema.clone(),
            }
        } else {
            let literal = toml_to_json(value)?;
            let schema_value = &registry
                .artifact_schema(schema)
                .ok_or_else(|| DefinitionError::UnknownSchema(schema.clone()))?
                .schema;
            if !literal_matches_schema(&literal, schema_value) {
                return Err(DefinitionError::Step(
                    step.id.clone(),
                    format!("literal input {name} does not match {schema}"),
                ));
            }
            Binding::Literal { value: literal }
        };
        output.insert(name.clone(), binding);
    }
    for name in step.inputs.keys() {
        if !expected.contains_key(name) {
            return Err(DefinitionError::Step(
                step.id.clone(),
                format!("undeclared input {name}"),
            ));
        }
    }
    Ok(output)
}

fn validate_expression_refs(
    expression: &ConditionExpr,
    known: &BTreeMap<String, String>,
    parameters: &BTreeMap<String, serde_json::Value>,
    ancestors: &BTreeSet<String>,
    step: &str,
) -> Result<(), DefinitionError> {
    let mut references = Vec::new();
    expression.references(&mut references);
    for reference in references {
        if let Some(parameter) = reference.strip_prefix("parameters.") {
            if !parameters.contains_key(parameter) {
                return Err(DefinitionError::Step(
                    step.into(),
                    format!("unknown condition parameter {parameter}"),
                ));
            }
            continue;
        }
        if reference.starts_with("steps.") {
            validate_producer(&reference, ancestors, step)?;
        }
        let exact_outcome = reference
            .strip_prefix("steps.")
            .and_then(|value| value.strip_suffix(".outcome"))
            .is_some_and(|producer| !producer.is_empty() && !producer.contains('.'));
        if !known.contains_key(&reference) && !exact_outcome {
            return Err(DefinitionError::Step(
                step.into(),
                format!("unknown condition reference {reference}"),
            ));
        }
    }
    Ok(())
}

fn validate_producer(
    reference: &str,
    ancestors: &BTreeSet<String>,
    step: &str,
) -> Result<(), DefinitionError> {
    if let Some(producer) = reference
        .strip_prefix("steps.")
        .and_then(|value| value.split('.').next())
        && !ancestors.contains(producer)
    {
        return Err(DefinitionError::Step(
            step.into(),
            format!("reference {reference} is not produced by a dependency"),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConditionType {
    Boolean,
    Number,
    String,
    Object,
    Array,
    Unknown,
}

fn validate_condition_type(
    expression: &ConditionExpr,
    known: &BTreeMap<String, String>,
    parameters: &BTreeMap<String, serde_json::Value>,
    registry: &DescriptorRegistry,
    step: &str,
) -> Result<(), DefinitionError> {
    fn infer(
        expression: &ConditionExpr,
        known: &BTreeMap<String, String>,
        parameters: &BTreeMap<String, serde_json::Value>,
        registry: &DescriptorRegistry,
        step: &str,
    ) -> Result<ConditionType, DefinitionError> {
        let mismatch = |message: &str| DefinitionError::Step(step.into(), message.into());
        match expression {
            ConditionExpr::Bool(_) => Ok(ConditionType::Boolean),
            ConditionExpr::Number(_) => Ok(ConditionType::Number),
            ConditionExpr::String(_) => Ok(ConditionType::String),
            ConditionExpr::Reference(reference) if reference.ends_with(".outcome") => {
                Ok(ConditionType::Boolean)
            }
            ConditionExpr::Reference(reference) if reference.starts_with("parameters.") => {
                let parameter = reference.trim_start_matches("parameters.");
                Ok(json_value_type(parameters.get(parameter).ok_or_else(
                    || mismatch("condition parameter is unresolved"),
                )?))
            }
            ConditionExpr::Reference(reference) => {
                let schema = known
                    .get(reference)
                    .and_then(|id| registry.artifact_schema(id))
                    .ok_or_else(|| mismatch("condition reference has no resolved schema"))?;
                Ok(json_schema_type(&schema.schema))
            }
            ConditionExpr::Not(value) => {
                if infer(value, known, parameters, registry, step)? != ConditionType::Boolean {
                    return Err(mismatch("'!' requires a boolean value"));
                }
                Ok(ConditionType::Boolean)
            }
            ConditionExpr::And(left, right) | ConditionExpr::Or(left, right) => {
                if infer(left, known, parameters, registry, step)? != ConditionType::Boolean
                    || infer(right, known, parameters, registry, step)? != ConditionType::Boolean
                {
                    return Err(mismatch("logical operators require boolean values"));
                }
                Ok(ConditionType::Boolean)
            }
            ConditionExpr::Equal(left, right) | ConditionExpr::NotEqual(left, right) => {
                let left = infer(left, known, parameters, registry, step)?;
                let right = infer(right, known, parameters, registry, step)?;
                if left != ConditionType::Unknown
                    && right != ConditionType::Unknown
                    && left != right
                {
                    return Err(mismatch("equality compares incompatible value types"));
                }
                Ok(ConditionType::Boolean)
            }
        }
    }
    if infer(expression, known, parameters, registry, step)? != ConditionType::Boolean {
        return Err(DefinitionError::Step(
            step.into(),
            "condition must produce a boolean".into(),
        ));
    }
    Ok(())
}

fn json_schema_type(schema: &serde_json::Value) -> ConditionType {
    match schema.get("type").and_then(serde_json::Value::as_str) {
        Some("boolean") => ConditionType::Boolean,
        Some("integer" | "number") => ConditionType::Number,
        Some("string") => ConditionType::String,
        Some("object") => ConditionType::Object,
        Some("array") => ConditionType::Array,
        _ => ConditionType::Unknown,
    }
}

fn json_value_type(value: &serde_json::Value) -> ConditionType {
    if value.is_boolean() {
        ConditionType::Boolean
    } else if value.is_number() {
        ConditionType::Number
    } else if value.is_string() {
        ConditionType::String
    } else if value.is_object() {
        ConditionType::Object
    } else if value.is_array() {
        ConditionType::Array
    } else {
        ConditionType::Unknown
    }
}

fn literal_matches_schema(value: &serde_json::Value, schema: &serde_json::Value) -> bool {
    match schema.get("type").and_then(serde_json::Value::as_str) {
        Some("boolean") => value.is_boolean(),
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("number") => value.is_number(),
        Some("string") => value.is_string(),
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("null") => value.is_null(),
        _ => true,
    }
}

fn compile_repeat(
    step: &StepDefinition,
    child: Option<&DefinitionSnapshot>,
    known: &BTreeMap<String, String>,
    parameters: &BTreeMap<String, serde_json::Value>,
    ancestors: &BTreeSet<String>,
    registry: &DescriptorRegistry,
) -> Result<Option<CompiledRepeat>, DefinitionError> {
    let Some(repeat) = &step.repeat else {
        return Ok(None);
    };
    if step.class != StepClass::WorkflowCall {
        return Err(DefinitionError::Step(
            step.id.clone(),
            "repeat is only valid on workflow_call".into(),
        ));
    }
    if repeat.max_iterations == 0 {
        return Err(DefinitionError::Step(
            step.id.clone(),
            "repeat max_iterations must be bounded above zero".into(),
        ));
    }
    let until = ConditionExpr::parse(&repeat.until)
        .map_err(|error| DefinitionError::Step(step.id.clone(), error.to_string()))?;
    let mut repeat_dependencies = ancestors.clone();
    repeat_dependencies.insert(step.id.clone());
    validate_expression_refs(&until, known, parameters, &repeat_dependencies, &step.id)?;
    validate_condition_type(&until, known, parameters, registry, &step.id)?;
    let child = child.expect("repeat workflow call has child");
    for (input, output) in &repeat.successor {
        let input_schema = child.definition.inputs.get(input).ok_or_else(|| {
            DefinitionError::Step(step.id.clone(), format!("unknown successor input {input}"))
        })?;
        let reference = format!("steps.{}.outputs.{output}", step.id);
        let output_schema = known.get(&reference).ok_or_else(|| {
            DefinitionError::Step(
                step.id.clone(),
                format!("unknown successor output {output}"),
            )
        })?;
        if input_schema.schema != *output_schema {
            return Err(DefinitionError::Step(
                step.id.clone(),
                format!("successor {input} type does not match {output}"),
            ));
        }
    }
    Ok(Some(CompiledRepeat {
        until,
        max_iterations: repeat.max_iterations,
        on_exhausted: repeat.on_exhausted,
        successor: repeat.successor.clone(),
    }))
}

fn is_reference(value: &str) -> bool {
    value.starts_with("inputs.") || value.starts_with("parameters.") || value.starts_with("steps.")
}

fn toml_to_json(value: &toml::Value) -> Result<serde_json::Value, DefinitionError> {
    serde_json::to_value(value).map_err(|error| DefinitionError::Serialization(error.to_string()))
}

fn canonical_source_revision(
    definition: &WorkflowDefinition,
) -> Result<ContentRevision, DefinitionError> {
    let mut canonical = definition.clone();
    canonical.launch.sort();
    canonical.launch.dedup();
    canonical.tags.sort();
    canonical.tags.dedup();
    canonical.capabilities.sort();
    canonical.capabilities.dedup();
    for step in &mut canonical.steps {
        if let Some(dependencies) = &mut step.depends_on {
            dependencies.sort();
        }
        step.capabilities.sort();
        step.capabilities.dedup();
        step.resources.sort();
        step.resources.dedup();
    }
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|error| DefinitionError::Serialization(error.to_string()))?;
    Ok(ContentRevision::digest(&bytes))
}

fn validate_budget_proof(
    definition: &WorkflowDefinition,
    steps: &[CompiledStep],
    children: &BTreeMap<String, Box<DefinitionSnapshot>>,
) -> Result<(), DefinitionError> {
    let local_attempts: u32 = steps
        .iter()
        .map(|step| step.retry.max_attempts.max(1))
        .sum();
    let child_attempts = steps
        .iter()
        .filter_map(|step| {
            let child = step
                .workflow
                .as_ref()
                .and_then(|workflow| children.get(workflow))?;
            let attempts = child.definition.budgets.max_attempts.unwrap_or(1);
            Some(
                attempts.saturating_mul(
                    step.repeat
                        .as_ref()
                        .map_or(1, |repeat| repeat.max_iterations),
                ),
            )
        })
        .fold(0_u32, u32::saturating_add);
    if let Some(bound) = definition.budgets.max_attempts
        && bound < local_attempts.saturating_add(child_attempts)
    {
        return Err(DefinitionError::InvalidField(format!(
            "budget max_attempts {bound} cannot cover the compiled bound {}",
            local_attempts.saturating_add(child_attempts)
        )));
    }
    let required_child_depth = children
        .values()
        .map(|child| 1_u32.saturating_add(snapshot_child_depth(child)))
        .max()
        .unwrap_or(0);
    if let Some(bound) = definition.budgets.max_child_depth
        && bound < required_child_depth
    {
        return Err(DefinitionError::InvalidField(format!(
            "budget max_child_depth {bound} cannot cover compiled depth {required_child_depth}"
        )));
    }
    for child in children.values() {
        if let (Some(parent), Some(child_bound)) = (
            definition.budgets.max_child_depth,
            child.definition.budgets.max_child_depth,
        ) && child_bound >= parent
        {
            return Err(DefinitionError::InvalidField(format!(
                "child budget max_child_depth={child_bound} does not fit below parent bound {parent}"
            )));
        }
        for (name, parent, child) in [
            (
                "max_attempts",
                definition.budgets.max_attempts,
                child.definition.budgets.max_attempts,
            ),
            (
                "max_mutations",
                definition.budgets.max_mutations,
                child.definition.budgets.max_mutations,
            ),
            (
                "max_fan_out",
                definition.budgets.max_fan_out,
                child.definition.budgets.max_fan_out,
            ),
        ] {
            if let (Some(parent), Some(child)) = (parent, child)
                && child > parent
            {
                return Err(DefinitionError::InvalidField(format!(
                    "child budget {name}={child} expands parent bound {parent}"
                )));
            }
        }
    }
    Ok(())
}

fn validate_terminal_reachability(steps: &[CompiledStep]) -> Result<(), DefinitionError> {
    let depended_on: BTreeSet<_> = steps
        .iter()
        .flat_map(|step| step.dependencies.iter().cloned())
        .collect();
    let mut reachable: BTreeSet<String> = steps
        .iter()
        .filter(|step| !depended_on.contains(&step.id))
        .filter(|step| {
            !matches!(
                step.condition
                    .as_ref()
                    .map(|condition| condition.evaluate(&BTreeMap::new())),
                Some(ConditionValue::Known(serde_json::Value::Bool(false)))
            )
        })
        .map(|step| step.id.clone())
        .collect();
    if reachable.is_empty() {
        return Err(DefinitionError::InvalidField(
            "workflow has no reachable terminal outcome".into(),
        ));
    }
    loop {
        let before = reachable.len();
        for step in steps {
            if reachable.contains(&step.id) {
                reachable.extend(step.dependencies.iter().cloned());
            }
        }
        if reachable.len() == before {
            break;
        }
    }
    if let Some(step) = steps.iter().find(|step| !reachable.contains(&step.id)) {
        return Err(DefinitionError::Step(
            step.id.clone(),
            "cannot reach a terminal outcome".into(),
        ));
    }
    Ok(())
}

fn snapshot_child_depth(snapshot: &DefinitionSnapshot) -> u32 {
    snapshot
        .children
        .values()
        .map(|child| 1_u32.saturating_add(snapshot_child_depth(child)))
        .max()
        .unwrap_or(0)
}

fn validate_names<'a>(
    kind: &str,
    names: impl Iterator<Item = &'a String>,
) -> Result<(), DefinitionError> {
    for name in names {
        if !valid_local_name(name) {
            return Err(DefinitionError::InvalidField(format!(
                "invalid {kind} name {name}"
            )));
        }
    }
    Ok(())
}

fn validate_step_id(id: &str) -> Result<(), DefinitionError> {
    if valid_local_name(id) {
        Ok(())
    } else {
        Err(DefinitionError::InvalidStep(id.into()))
    }
}

fn valid_local_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[derive(Debug)]
pub enum DefinitionError {
    Io(std::io::Error),
    Resource(ResourceError),
    Syntax(String),
    UnsupportedSchema(u32),
    InvalidIdentity(String),
    InvalidField(String),
    InvalidStep(String),
    DuplicateStep(String),
    DuplicateIdentity(String),
    MissingDependency { step: String, dependency: String },
    Cycle(String),
    Recursion(String),
    UnknownWorkflow(String),
    UnknownImplementation(String),
    UnknownSchema(String),
    UnknownExecutableRevision(String),
    UnavailableTarget { step: String, target: String },
    Step(String, String),
    Source(PathBuf, String),
    Serialization(String),
}

impl fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::Resource(error) => error.fmt(f),
            Self::Syntax(error) => write!(f, "invalid Workflow Definition TOML: {error}"),
            Self::UnsupportedSchema(version) => write!(
                f,
                "unsupported Workflow Definition schema version {version}"
            ),
            Self::InvalidIdentity(id) => write!(f, "invalid workflow identity {id}"),
            Self::InvalidField(message) => message.fmt(f),
            Self::InvalidStep(id) => write!(f, "invalid Step id {id}"),
            Self::DuplicateStep(id) => write!(f, "duplicate Step id {id}"),
            Self::DuplicateIdentity(id) => write!(f, "duplicate workflow identity {id}"),
            Self::MissingDependency { step, dependency } => {
                write!(f, "Step {step} depends on missing Step {dependency}")
            }
            Self::Cycle(id) => write!(f, "workflow graph contains a cycle at Step {id}"),
            Self::Recursion(chain) => write!(f, "recursive workflow call: {chain}"),
            Self::UnknownWorkflow(id) => write!(f, "unknown child workflow {id}"),
            Self::UnknownImplementation(id) => write!(f, "unknown Step Implementation {id}"),
            Self::UnknownSchema(id) => write!(f, "unknown Artifact schema {id}"),
            Self::UnknownExecutableRevision(id) => {
                write!(
                    f,
                    "Step Implementation {id} has no resolved executable revision"
                )
            }
            Self::UnavailableTarget { step, target } => {
                write!(f, "Step {step}: execution target {target} is unavailable")
            }
            Self::Step(id, message) => write!(f, "Step {id}: {message}"),
            Self::Source(path, message) => write!(f, "{}: {message}", path.display()),
            Self::Serialization(message) => message.fmt(f),
        }
    }
}

impl std::error::Error for DefinitionError {}
impl From<std::io::Error> for DefinitionError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<ResourceError> for DefinitionError {
    fn from(value: ResourceError) -> Self {
        Self::Resource(value)
    }
}

pub fn schema_json() -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://prism.dev/schemas/workflow-definition-v2.json",
        "title": "Prism Workflow Definition", "type": "object",
        "required": ["schema_version", "id", "name", "launch", "steps"],
        "properties": { "schema_version": {"const": 2}, "id": {"type":"string", "pattern":"^[^/]+/[^/]+$"},
          "name":{"type":"string", "minLength":1}, "description":{"type":"string"},
          "launch":{"type":"array", "items":{"enum":["manual","child","trigger"]}, "minItems":1, "uniqueItems":true},
          "tags":{"type":"array", "items":{"type":"string"}, "uniqueItems":true},
          "capabilities":{"type":"array", "items":{"type":"string"}, "uniqueItems":true},
          "inputs":{"type":"object", "additionalProperties":{"$ref":"#/$defs/port"}},
          "outputs":{"type":"object", "additionalProperties":{"$ref":"#/$defs/port"}},
          "parameters":{"type":"object"},
          "budgets":{"$ref":"#/$defs/budgets"},
          "steps":{"type":"array", "minItems":1, "items":{"$ref":"#/$defs/step"}}
        },
        "$defs": {
          "port":{"type":"object", "required":["type"], "properties":{"type":{"type":"string"},"required":{"type":"boolean"},"from_context":{"type":"boolean"},"from":{"type":"string"}},"additionalProperties":false},
          "budgets":{"type":"object","properties":{"max_child_depth":{"type":"integer","minimum":0},"max_attempts":{"type":"integer","minimum":0},"max_mutations":{"type":"integer","minimum":0},"max_fan_out":{"type":"integer","minimum":0}},"additionalProperties":false},
          "retry":{"type":"object","properties":{"max_attempts":{"type":"integer","minimum":1},"on":{"type":"array","items":{"enum":["transient","timeout"]},"uniqueItems":true},"initial_delay_seconds":{"type":"integer","minimum":0},"max_delay_seconds":{"type":"integer","minimum":0}},"additionalProperties":false},
          "repeat":{"type":"object","required":["until","max_iterations","on_exhausted"],"properties":{"until":{"type":"string"},"max_iterations":{"type":"integer","minimum":1},"on_exhausted":{"enum":["input_required","approval","fail"]},"successor":{"type":"object","additionalProperties":{"type":"string"}}},"additionalProperties":false},
          "step":{"type":"object","required":["id","class","skippable"],"properties":{"id":{"type":"string"},"class":{"enum":["action","gate","approval","wait","notification","workflow_call"]},"use":{"type":"string"},"workflow":{"type":"string"},"depends_on":{"type":"array","items":{"type":"string"},"uniqueItems":true},"inputs":{"type":"object"},"outputs":{"type":"object","additionalProperties":{"type":"string"}},"settings":{"type":"object"},"condition":{"type":"string"},"on_unknown":{"enum":["wait","skip","fail"]},"capabilities":{"type":"array","items":{"type":"string"},"uniqueItems":true},"resources":{"type":"array","items":{"type":"string"},"uniqueItems":true},"target":{"type":"string"},"skippable":{"type":"boolean"},"timeout_seconds":{"type":"integer","minimum":1},"on_timeout":{"enum":["fail","input_required"]},"retry":{"$ref":"#/$defs/retry"},"repeat":{"$ref":"#/$defs/repeat"}},"additionalProperties":false,
            "allOf":[{"if":{"properties":{"class":{"const":"workflow_call"}}},"then":{"required":["workflow"],"not":{"required":["use"]}},"else":{"required":["use"],"not":{"required":["workflow"]}}}]
          }
        }, "additionalProperties": false
    })
}

pub fn commented_template(id: &str, name: &str) -> String {
    format!(
        "# Prism Workflow Definition schema v2\nschema_version = 2\nid = \"{id}\"\nname = \"{name}\"\ndescription = \"\"\nlaunch = [\"manual\"]\n\n[[steps]]\nid = \"first\"\nclass = \"action\"\nuse = \"prism.standard/echo\"\nskippable = false\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_extension_protocol::{ExtensionDescriptor, PortDescriptor};
    use serde_json::json;

    fn registry() -> DescriptorRegistry {
        let mut registry = DescriptorRegistry::default();
        registry
            .register(&ExtensionDescriptor {
                artifact_schemas: vec![
                    ArtifactSchemaDescriptor {
                        id: "acme.test/value".into(),
                        schema: json!({"type":"object"}),
                    },
                    ArtifactSchemaDescriptor {
                        id: "acme.test/ready".into(),
                        schema: json!({"type":"boolean"}),
                    },
                ],
                implementations: vec![
                    ImplementationDescriptor {
                        id: "acme.test/source".into(),
                        class: StepClass::Action,
                        inputs: vec![],
                        outputs: vec![PortDescriptor {
                            name: "value".into(),
                            schema: "acme.test/value".into(),
                            required: true,
                        }],
                        capabilities: vec!["repository_read".into()],
                        targets: vec!["local".into()],
                        effect_boundary: Default::default(),
                    },
                    ImplementationDescriptor {
                        id: "acme.test/check".into(),
                        class: StepClass::Gate,
                        inputs: vec![PortDescriptor {
                            name: "value".into(),
                            schema: "acme.test/value".into(),
                            required: true,
                        }],
                        outputs: vec![PortDescriptor {
                            name: "ready".into(),
                            schema: "acme.test/ready".into(),
                            required: true,
                        }],
                        capabilities: vec![],
                        targets: vec!["local".into()],
                        effect_boundary: Default::default(),
                    },
                    ImplementationDescriptor {
                        id: "acme.test/notify".into(),
                        class: StepClass::Notification,
                        inputs: vec![],
                        outputs: vec![],
                        capabilities: vec![],
                        targets: vec![],
                        effect_boundary: Default::default(),
                    },
                ],
                ..ExtensionDescriptor::default()
            })
            .unwrap();
        registry
    }

    fn header(id: &str, launch: &str) -> String {
        format!(
            "schema_version = 2\nid = \"{id}\"\nname = \"test\"\nlaunch = [\"{launch}\"]\ncapabilities = [\"repository_read\"]\n"
        )
    }

    #[test]
    fn ordered_shorthand_and_explicit_branches_compile_to_one_dag() {
        let source = format!(
            "{}\n[[steps]]\nid='source'\nclass='action'\nuse='acme.test/source'\nskippable=false\n\n[[steps]]\nid='left'\nclass='gate'\nuse='acme.test/check'\nskippable=false\ndepends_on=['source']\ninputs={{value='steps.source.outputs.value'}}\n\n[[steps]]\nid='right'\nclass='gate'\nuse='acme.test/check'\nskippable=false\ndepends_on=['source']\ninputs={{value='steps.source.outputs.value'}}\n\n[[steps]]\nid='join'\nclass='notification'\nuse='acme.test/notify'\nskippable=false\ndepends_on=['left','right']\n",
            header("acme.test/branches", "manual")
        );
        let catalog =
            DefinitionCatalog::from_sources([("branches.toml".into(), source)], registry())
                .unwrap();
        let snapshot = catalog.compile("acme.test/branches").unwrap();
        assert_eq!(
            snapshot.definition.steps[0].dependencies,
            Vec::<String>::new()
        );
        assert_eq!(snapshot.definition.steps[1].dependencies, ["source"]);
        assert_eq!(snapshot.definition.steps[2].dependencies, ["source"]);
        assert_eq!(snapshot.definition.steps[3].dependencies, ["left", "right"]);
        assert!(snapshot.capabilities.contains("repository_read"));
    }

    #[test]
    fn source_map_order_does_not_change_snapshot_digest() {
        let first = format!(
            "{}\ntags=['z','a']\n[[steps]]\nid='source'\nclass='action'\nuse='acme.test/source'\nskippable=false\n",
            header("acme.test/stable", "manual")
        );
        let second = "schema_version=2\nname='test'\nid='acme.test/stable'\nlaunch=['manual']\ntags=['a','z']\ncapabilities=['repository_read']\n[[steps]]\nuse='acme.test/source'\nclass='action'\nid='source'\nskippable=false\n".to_owned();
        let left = DefinitionCatalog::from_sources([("a".into(), first)], registry())
            .unwrap()
            .compile("acme.test/stable")
            .unwrap();
        let right = DefinitionCatalog::from_sources([("b".into(), second)], registry())
            .unwrap()
            .compile("acme.test/stable")
            .unwrap();
        assert_eq!(left.definition, right.definition);
        assert_eq!(left.digest, right.digest);
    }

    #[test]
    fn cycles_and_non_dependency_bindings_are_rejected() {
        let cycle = format!(
            "{}\n[[steps]]\nid='a'\nclass='action'\nuse='acme.test/source'\nskippable=false\ndepends_on=['b']\n[[steps]]\nid='b'\nclass='action'\nuse='acme.test/source'\nskippable=false\ndepends_on=['a']\n",
            header("acme.test/cycle", "manual")
        );
        let catalog =
            DefinitionCatalog::from_sources([("cycle".into(), cycle)], registry()).unwrap();
        assert!(matches!(
            catalog.compile("acme.test/cycle"),
            Err(DefinitionError::Cycle(_))
        ));

        let invalid = format!(
            "{}\n[[steps]]\nid='source'\nclass='action'\nuse='acme.test/source'\nskippable=false\n[[steps]]\nid='other'\nclass='action'\nuse='acme.test/source'\nskippable=false\ndepends_on=[]\n[[steps]]\nid='check'\nclass='gate'\nuse='acme.test/check'\nskippable=false\ndepends_on=['other']\ninputs={{value='steps.source.outputs.value'}}\n",
            header("acme.test/binding", "manual")
        );
        let catalog =
            DefinitionCatalog::from_sources([("binding".into(), invalid)], registry()).unwrap();
        assert!(
            catalog
                .compile("acme.test/binding")
                .unwrap_err()
                .to_string()
                .contains("not produced by a dependency")
        );
    }

    #[test]
    fn forward_edges_parameters_and_exact_outcomes_compile_without_loose_roots() {
        let forward = format!(
            "{}\n[parameters]\nready=true\n[[steps]]\nid='check'\nclass='gate'\nuse='acme.test/check'\nskippable=false\ndepends_on=['source']\ninputs={{value='steps.source.outputs.value'}}\ncondition='parameters.ready == true && steps.source.outcome == true'\n[[steps]]\nid='source'\nclass='action'\nuse='acme.test/source'\nskippable=false\ndepends_on=[]\n",
            header("acme.test/forward", "manual")
        );
        let snapshot = DefinitionCatalog::from_sources([("forward".into(), forward)], registry())
            .unwrap()
            .compile("acme.test/forward")
            .unwrap();
        assert_eq!(snapshot.definition.steps[0].id, "source");
        assert_eq!(snapshot.definition.steps[1].id, "check");

        let invalid = format!(
            "{}\n[[steps]]\nid='source'\nclass='action'\nuse='acme.test/source'\nskippable=false\ncondition='garbage.outcome == true'\n",
            header("acme.test/outcome", "manual")
        );
        let error = DefinitionCatalog::from_sources([("invalid".into(), invalid)], registry())
            .unwrap()
            .compile("acme.test/outcome")
            .unwrap_err();
        assert!(error.to_string().contains("unknown condition reference"));
    }

    #[test]
    fn skip_policy_must_be_explicit() {
        let source = format!(
            "{}\n[[steps]]\nid='source'\nclass='action'\nuse='acme.test/source'\n",
            header("acme.test/skip", "manual")
        );
        let error = DefinitionCatalog::from_sources([("skip".into(), source)], registry())
            .unwrap()
            .compile("acme.test/skip")
            .unwrap_err();
        assert!(error.to_string().contains("skippable must be explicit"));
    }

    #[test]
    fn statically_unreachable_terminal_is_rejected() {
        let source = format!(
            "{}\n[[steps]]\nid='source'\nclass='action'\nuse='acme.test/source'\nskippable=false\ncondition='false'\n",
            header("acme.test/terminal", "manual")
        );
        let error = DefinitionCatalog::from_sources([("terminal".into(), source)], registry())
            .unwrap()
            .compile("acme.test/terminal")
            .unwrap_err();
        assert!(error.to_string().contains("no reachable terminal outcome"));
    }

    #[test]
    fn bounded_child_repeat_and_recursion_are_validated() {
        let child = format!(
            "{}\n[inputs.value]\ntype='acme.test/value'\nrequired=true\n[outputs.value]\ntype='acme.test/value'\nfrom='steps.source.outputs.value'\n[[steps]]\nid='source'\nclass='action'\nuse='acme.test/source'\nskippable=false\n",
            header("acme.test/child", "child")
        );
        let parent = format!(
            "{}\n[inputs.value]\ntype='acme.test/value'\nrequired=true\n[[steps]]\nid='call'\nclass='workflow_call'\nworkflow='acme.test/child'\nskippable=false\ninputs={{value='inputs.value'}}\n[steps.repeat]\nuntil='steps.call.outputs.value == steps.call.outputs.value'\nmax_iterations=3\non_exhausted='input_required'\nsuccessor={{value='value'}}\n",
            header("acme.test/parent", "manual")
        );
        let catalog = DefinitionCatalog::from_sources(
            [("child".into(), child), ("parent".into(), parent)],
            registry(),
        )
        .unwrap();
        let snapshot = catalog.compile("acme.test/parent").unwrap();
        assert_eq!(
            snapshot.definition.steps[0]
                .repeat
                .as_ref()
                .unwrap()
                .max_iterations,
            3
        );
        assert!(snapshot.children.contains_key("acme.test/child"));
    }

    #[test]
    fn retained_snapshot_survives_source_deletion() {
        let source = format!(
            "{}\n[[steps]]\nid='source'\nclass='action'\nuse='acme.test/source'\nskippable=false\n",
            header("acme.test/retained", "manual")
        );
        let catalog =
            DefinitionCatalog::from_sources([("source".into(), source)], registry()).unwrap();
        let snapshot = catalog.compile("acme.test/retained").unwrap();
        let root =
            std::env::temp_dir().join(format!("prism-definition-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = ContentStore::new(&root);
        let revision = catalog.retain(&snapshot, &store, "run-1").unwrap();
        let loaded: DefinitionSnapshot =
            serde_json::from_slice(&store.load(&revision).unwrap()).unwrap();
        assert_eq!(loaded, snapshot);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn launch_context_inference_reports_missing_typed_inputs() {
        let source = format!(
            "{}\n[inputs.worktree]\ntype='acme.test/value'\nrequired=true\nfrom_context=true\n[inputs.channel]\ntype='acme.test/ready'\nrequired=true\n[[steps]]\nid='source'\nclass='action'\nuse='acme.test/source'\nskippable=false\n",
            header("acme.test/context", "manual")
        );
        let snapshot = DefinitionCatalog::from_sources([("context".into(), source)], registry())
            .unwrap()
            .compile("acme.test/context")
            .unwrap();
        let compatibility = snapshot.launch_compatibility(&BTreeMap::from([(
            "selected_worktree".into(),
            "acme.test/value".into(),
        )]));
        assert_eq!(
            compatibility.context_bindings.get("worktree"),
            Some(&"selected_worktree".into())
        );
        assert_eq!(compatibility.missing_required_inputs, ["channel"]);
        assert!(!compatibility.compatible);
    }

    #[test]
    fn every_bootstrapped_standard_workflow_uses_the_public_catalog() {
        let mut standard_registry = DescriptorRegistry::default();
        standard_registry
            .register(&ExtensionDescriptor {
                implementations: vec![ImplementationDescriptor {
                    id: "prism.standard/echo".into(),
                    class: StepClass::Action,
                    inputs: vec![],
                    outputs: vec![],
                    capabilities: vec![],
                    targets: vec![],
                    effect_boundary: Default::default(),
                }],
                ..ExtensionDescriptor::default()
            })
            .unwrap();
        let sources = [
            ("plan", include_str!("../../../assets/workflows/plan.toml")),
            (
                "implement",
                include_str!("../../../assets/workflows/implement.toml"),
            ),
            ("auto", include_str!("../../../assets/workflows/auto.toml")),
            (
                "stabilize",
                include_str!("../../../assets/workflows/stabilize.toml"),
            ),
            (
                "stabilize-change-request",
                include_str!("../../../assets/workflows/stabilize-change-request.toml"),
            ),
            (
                "triage-issues",
                include_str!("../../../assets/workflows/triage-issues.toml"),
            ),
        ]
        .map(|(name, body)| {
            (
                name.to_owned(),
                format!("id = 'prism.standard/{name}'\n{body}"),
            )
        });
        let catalog = DefinitionCatalog::from_sources(sources, standard_registry).unwrap();
        assert_eq!(catalog.list().len(), 6);
        assert!(
            catalog
                .list()
                .iter()
                .all(|definition| definition.id.starts_with("prism.standard/"))
        );
    }

    #[test]
    fn discovered_package_workflows_pin_the_exact_lock_closure() {
        let root = std::env::temp_dir().join(format!(
            "prism-definition-package-catalog-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        crate::package::bootstrap_standard_pack(&root).unwrap();
        let mut standard_registry = DescriptorRegistry::default();
        standard_registry
            .register(&ExtensionDescriptor {
                implementations: vec![ImplementationDescriptor {
                    id: "prism.standard/echo".into(),
                    class: StepClass::Action,
                    inputs: vec![],
                    outputs: vec![],
                    capabilities: vec![],
                    targets: vec![],
                    effect_boundary: Default::default(),
                }],
                ..ExtensionDescriptor::default()
            })
            .unwrap();
        let catalog = DefinitionCatalog::discover(
            &root,
            None,
            &TrustStore::new(root.join("trust.json")),
            standard_registry,
            BTreeMap::from([(
                "prism.standard/echo".into(),
                ExecutableResolution {
                    revision: "sha256:executable".into(),
                    trusted: false,
                },
            )]),
        )
        .unwrap();
        assert_eq!(catalog.list().len(), 6);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn published_schema_and_template_are_generated_from_authoritative_types() {
        let published: serde_json::Value = serde_json::from_str(include_str!(
            "../../../assets/schemas/workflow-definition-v2.json"
        ))
        .unwrap();
        assert_eq!(published, schema_json());
        assert_eq!(
            include_str!("../../../assets/templates/workflow-v2.toml"),
            commented_template("your.package/workflow", "workflow")
        );
        let diagnostics = diagnose_source("broken.toml", "schema_version = [").unwrap_err();
        assert_eq!(diagnostics[0].path, PathBuf::from("broken.toml"));
        assert!(diagnostics[0].byte_start.is_some());
    }

    #[test]
    fn authoring_create_copy_and_migration_preview_are_non_destructive() {
        let root =
            std::env::temp_dir().join(format!("prism-definition-authoring-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let authoring = DefinitionAuthoringOperations::new(root.join("workflows"));
        let created = authoring.create("acme.test/first", "first").unwrap();
        WorkflowDefinition::parse(&fs::read_to_string(&created).unwrap()).unwrap();
        let copied = authoring
            .copy(&created, "acme.test/second", "second")
            .unwrap();
        let copy = WorkflowDefinition::parse(&fs::read_to_string(&copied).unwrap()).unwrap();
        assert_eq!(copy.id, "acme.test/second");
        let before = fs::read_to_string(&copied).unwrap();
        let preview = authoring.migration_preview(&copied).unwrap();
        assert!(!preview.changed);
        assert_eq!(authoring.apply_migration(&preview).unwrap(), None);
        assert_eq!(fs::read_to_string(copied).unwrap(), before);

        let legacy = root.join("legacy.toml");
        fs::write(
            &legacy,
            "schema_version=1\nid='acme.test/legacy'\nname='legacy'\n[inputs.task]\nartifact_type='builtin:task@1'\n[[steps]]\nid='act'\nclass='action'\nimplementation='builtin:command@1'\n[steps.inputs.task]\nfrom='run.task'\nartifact_type='builtin:task@1'\n[steps.outputs.result]\nartifact_type='builtin:task@1'\n",
        )
        .unwrap();
        let preview = authoring.migration_preview(&legacy).unwrap();
        assert!(preview.changed);
        let backup = authoring.apply_migration(&preview).unwrap().unwrap();
        assert!(backup.is_file());
        assert_eq!(
            WorkflowDefinition::parse(&fs::read_to_string(&legacy).unwrap())
                .unwrap()
                .schema_version,
            2
        );
        let created_source = fs::read_to_string(&created).unwrap();
        let revision =
            canonical_source_revision(&WorkflowDefinition::parse(&created_source).unwrap())
                .unwrap();
        let wrong_identity = created_source.replace("acme.test/first", "acme.test/replacement");
        assert!(
            authoring
                .update(&created, revision.as_str(), &wrong_identity)
                .unwrap_err()
                .to_string()
                .contains("cannot replace identity")
        );
        let updated_source = created_source.replace(
            "description = \"\"",
            "description = \"updated without overwriting a changed base\"",
        );
        let update_backup = authoring
            .update(&created, revision.as_str(), &updated_source)
            .unwrap();
        assert!(update_backup.is_file());
        assert!(
            fs::read_to_string(created)
                .unwrap()
                .contains("updated without")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
