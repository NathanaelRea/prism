//! Prompt-first Workflow source parsing, graph compilation, discovery, and immutable snapshots.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub const DEFAULT_MAX_AGENT_RUNS: u32 = 10;
const MAX_WORKFLOW_STRING_INPUT_CHARS: usize = 16_384;
pub const DEFAULT_STABILIZE_SOURCE: &str = include_str!("../../assets/workflows/stabilize.toml");
pub const MULTI_MODEL_REVIEW_EXAMPLE: &str =
    include_str!("../../assets/workflows/multi-model-review.toml");
pub const PROMPT_WORKFLOW_TEMPLATE: &str = include_str!("../../assets/templates/workflow.toml");

pub fn prompt_workflow_schema() -> serde_json::Value {
    serde_json::from_str(include_str!("../../schemas/workflow.schema.json"))
        .expect("bundled prompt Workflow schema is valid JSON")
}

/// Validate explicit Agent selections against the configured harness adapters without
/// starting a process. Callers use this as the final start-time Workflow validation.
pub fn validate_workflow_agent_selection(
    workflow: &CompiledWorkflow,
    config: &crate::config::Config,
) -> Result<(), Vec<WorkflowDiagnostic>> {
    let mut diagnostics = Vec::new();
    for (index, step) in workflow.steps.iter().enumerate() {
        let harness_id = step
            .agent
            .harness
            .as_deref()
            .unwrap_or(&config.default_harness);
        let result = config
            .harness_config(harness_id)
            .map_err(|error| error.to_string())
            .and_then(|harness_config| {
                let harness = crate::harness::Harness::new(harness_id, &harness_config);
                let selection = crate::harness::AgentSelection {
                    model: step.agent.model.as_deref(),
                    variant: step.agent.variant.as_deref(),
                };
                harness.headless_with_model(
                    step.prompt.as_deref().unwrap_or("check-only"),
                    std::path::Path::new("."),
                    "Workflow validation",
                    None,
                    selection,
                    false,
                )?;
                if let Some(followup) = step.followups.first() {
                    harness.headless_resume_with_model(
                        followup,
                        std::path::Path::new("."),
                        "workflow-validation-session",
                        selection,
                    )?;
                }
                Ok(())
            });
        if let Err(message) = result {
            let (byte_start, byte_end) = step_span(&workflow.source, index);
            diagnostics.push(WorkflowDiagnostic {
                path: workflow.source_path.clone(),
                message: format!("Step '{}': {message}", step.key),
                byte_start,
                byte_end,
            });
        }
    }
    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(diagnostics)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDefaults {
    pub harness: Option<String>,
    pub model: Option<String>,
    pub variant: Option<String>,
    pub max_agent_runs: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStepSource {
    pub id: Option<String>,
    pub trigger: Option<String>,
    pub depends_on: Option<Vec<String>>,
    #[serde(default)]
    pub context: Vec<String>,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub variant: Option<String>,
    pub prompt: Option<String>,
    #[serde(default)]
    pub followups: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowInputType {
    File,
    String,
    Bool,
    Number,
    Enum,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum WorkflowInputDefault {
    String(String),
    Bool(bool),
    Number(serde_json::Number),
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowInputSource {
    #[serde(rename = "type")]
    pub kind: Option<WorkflowInputType>,
    pub description: Option<String>,
    pub default: Option<WorkflowInputDefault>,
    pub glob: Option<String>,
    pub options: Option<Vec<String>>,
    pub min: Option<serde_json::Number>,
    pub max: Option<serde_json::Number>,
    pub min_length: Option<usize>,
    pub max_length: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSource {
    #[serde(default)]
    pub defaults: WorkflowDefaults,
    #[serde(default)]
    pub inputs: BTreeMap<String, WorkflowInputSource>,
    #[serde(rename = "step")]
    pub steps: Vec<WorkflowStepSource>,
}

fn validate_workflow_input_source(
    name: &str,
    input: &WorkflowInputSource,
) -> Result<(), WorkflowSourceError> {
    const MAX_DESCRIPTION_CHARS: usize = 256;
    const MAX_OPTIONS: usize = 256;
    if !valid_name(name) || name.chars().count() > 64 {
        return Err(WorkflowSourceError::Invalid {
            message: format!(
                "input '{name}' must use at most 64 letters, numbers, '.', '_' or '-'"
            ),
            step: None,
        });
    }
    if let Some(description) = input.description.as_deref()
        && (description.trim().is_empty()
            || description.chars().count() > MAX_DESCRIPTION_CHARS
            || description.chars().any(char::is_control))
    {
        return Err(WorkflowSourceError::Invalid {
            message: format!(
                "input '{name}' description must be a non-empty single line of at most {MAX_DESCRIPTION_CHARS} characters"
            ),
            step: None,
        });
    }
    let kind = input.kind.unwrap_or(WorkflowInputType::File);
    let invalid_field = |field: &str| WorkflowSourceError::Invalid {
        message: format!(
            "input '{name}' of type '{}' does not support '{field}'",
            workflow_input_type_name(kind)
        ),
        step: None,
    };
    match kind {
        WorkflowInputType::File => {
            if input.options.is_some() {
                return Err(invalid_field("options"));
            }
            if input.min.is_some() {
                return Err(invalid_field("min"));
            }
            if input.max.is_some() {
                return Err(invalid_field("max"));
            }
            if input.min_length.is_some() {
                return Err(invalid_field("min_length"));
            }
            if input.max_length.is_some() {
                return Err(invalid_field("max_length"));
            }
            let Some(glob) = input.glob.as_deref() else {
                return Err(WorkflowSourceError::Invalid {
                    message: format!("input '{name}' of type 'file' requires 'glob'"),
                    step: None,
                });
            };
            if glob.trim().is_empty() || glob.chars().any(char::is_control) {
                return Err(WorkflowSourceError::Invalid {
                    message: format!("input '{name}' glob must be a non-empty single line"),
                    step: None,
                });
            }
            let glob_path = Path::new(glob);
            if glob_path.is_absolute()
                || glob_path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                return Err(WorkflowSourceError::Invalid {
                    message: format!("input '{name}' glob must stay within the worktree"),
                    step: None,
                });
            }
            validate_default_kind(
                name,
                input.default.as_ref(),
                "file",
                |default| matches!(default, WorkflowInputDefault::String(value) if !value.trim().is_empty() && !value.chars().any(char::is_control)),
            )?;
        }
        WorkflowInputType::String => {
            reject_file_and_enum_fields(name, input, kind)?;
            if input.min.is_some() {
                return Err(invalid_field("min"));
            }
            if input.max.is_some() {
                return Err(invalid_field("max"));
            }
            if input
                .min_length
                .is_some_and(|min| min == 0 || min > MAX_WORKFLOW_STRING_INPUT_CHARS)
                || input
                    .max_length
                    .is_some_and(|max| max == 0 || max > MAX_WORKFLOW_STRING_INPUT_CHARS)
            {
                return Err(WorkflowSourceError::Invalid {
                    message: format!(
                        "input '{name}' string lengths must be between 1 and {MAX_WORKFLOW_STRING_INPUT_CHARS}"
                    ),
                    step: None,
                });
            }
            if input
                .min_length
                .zip(input.max_length)
                .is_some_and(|(min, max)| min > max)
            {
                return Err(WorkflowSourceError::Invalid {
                    message: format!("input '{name}' min_length must not exceed max_length"),
                    step: None,
                });
            }
            validate_default_kind(
                name,
                input.default.as_ref(),
                "string",
                |default| matches!(default, WorkflowInputDefault::String(value) if valid_string_input(value, input.min_length, input.max_length)),
            )?;
        }
        WorkflowInputType::Bool => {
            reject_file_and_enum_fields(name, input, kind)?;
            for (field, present) in [
                ("min", input.min.is_some()),
                ("max", input.max.is_some()),
                ("min_length", input.min_length.is_some()),
                ("max_length", input.max_length.is_some()),
            ] {
                if present {
                    return Err(invalid_field(field));
                }
            }
            validate_default_kind(name, input.default.as_ref(), "bool", |default| {
                matches!(default, WorkflowInputDefault::Bool(_))
            })?;
        }
        WorkflowInputType::Number => {
            reject_file_and_enum_fields(name, input, kind)?;
            if input.min_length.is_some() {
                return Err(invalid_field("min_length"));
            }
            if input.max_length.is_some() {
                return Err(invalid_field("max_length"));
            }
            if number_range_invalid(input.min.as_ref(), input.max.as_ref()) {
                return Err(WorkflowSourceError::Invalid {
                    message: format!("input '{name}' min must not exceed max"),
                    step: None,
                });
            }
            validate_default_kind(
                name,
                input.default.as_ref(),
                "number",
                |default| matches!(default, WorkflowInputDefault::Number(value) if number_in_range(value, input.min.as_ref(), input.max.as_ref())),
            )?;
        }
        WorkflowInputType::Enum => {
            if input.glob.is_some() {
                return Err(invalid_field("glob"));
            }
            for (field, present) in [
                ("min", input.min.is_some()),
                ("max", input.max.is_some()),
                ("min_length", input.min_length.is_some()),
                ("max_length", input.max_length.is_some()),
            ] {
                if present {
                    return Err(invalid_field(field));
                }
            }
            let Some(options) = input.options.as_deref() else {
                return Err(WorkflowSourceError::Invalid {
                    message: format!("input '{name}' of type 'enum' requires 'options'"),
                    step: None,
                });
            };
            if options.is_empty() || options.len() > MAX_OPTIONS {
                return Err(WorkflowSourceError::Invalid {
                    message: format!(
                        "input '{name}' enum must declare between 1 and {MAX_OPTIONS} options"
                    ),
                    step: None,
                });
            }
            let mut seen = BTreeSet::new();
            for option in options {
                if option.trim().is_empty()
                    || option.chars().count() > 256
                    || option.chars().any(char::is_control)
                {
                    return Err(WorkflowSourceError::Invalid {
                        message: format!(
                            "input '{name}' enum options must be non-empty single lines of at most 256 characters"
                        ),
                        step: None,
                    });
                }
                if !seen.insert(option) {
                    return Err(WorkflowSourceError::Invalid {
                        message: format!("input '{name}' enum option '{option}' is repeated"),
                        step: None,
                    });
                }
            }
            validate_default_kind(
                name,
                input.default.as_ref(),
                "enum option",
                |default| matches!(default, WorkflowInputDefault::String(value) if options.contains(value)),
            )?;
        }
    }
    Ok(())
}

fn reject_file_and_enum_fields(
    name: &str,
    input: &WorkflowInputSource,
    kind: WorkflowInputType,
) -> Result<(), WorkflowSourceError> {
    for (field, present) in [
        ("glob", input.glob.is_some()),
        ("options", input.options.is_some()),
    ] {
        if present {
            return Err(WorkflowSourceError::Invalid {
                message: format!(
                    "input '{name}' of type '{}' does not support '{field}'",
                    workflow_input_type_name(kind)
                ),
                step: None,
            });
        }
    }
    Ok(())
}

fn validate_default_kind(
    name: &str,
    default: Option<&WorkflowInputDefault>,
    expected: &str,
    valid: impl FnOnce(&WorkflowInputDefault) -> bool,
) -> Result<(), WorkflowSourceError> {
    if default.is_some_and(|default| !valid(default)) {
        return Err(WorkflowSourceError::Invalid {
            message: format!("input '{name}' default must be a valid {expected}"),
            step: None,
        });
    }
    Ok(())
}

fn workflow_input_type_name(kind: WorkflowInputType) -> &'static str {
    match kind {
        WorkflowInputType::File => "file",
        WorkflowInputType::String => "string",
        WorkflowInputType::Bool => "bool",
        WorkflowInputType::Number => "number",
        WorkflowInputType::Enum => "enum",
    }
}

impl WorkflowSource {
    pub fn parse(source: &str) -> Result<Self, WorkflowSourceError> {
        let parsed: Self = toml::from_str(source).map_err(|error| WorkflowSourceError::Syntax {
            message: error.message().to_string(),
            span: error.span().map(|span| (span.start, span.end)),
        })?;
        if parsed.steps.is_empty() {
            return Err(WorkflowSourceError::Invalid {
                message: "workflow must contain at least one [[step]]".into(),
                step: None,
            });
        }
        if parsed.defaults.max_agent_runs == Some(0) {
            return Err(WorkflowSourceError::Invalid {
                message: "defaults.max_agent_runs must be at least 1".into(),
                step: None,
            });
        }
        if parsed.inputs.len() > 64 {
            return Err(WorkflowSourceError::Invalid {
                message: "workflow may declare at most 64 inputs".into(),
                step: None,
            });
        }
        for (name, input) in &parsed.inputs {
            validate_workflow_input_source(name, input)?;
        }
        for (field, value) in [
            ("defaults.harness", parsed.defaults.harness.as_deref()),
            ("defaults.model", parsed.defaults.model.as_deref()),
            ("defaults.variant", parsed.defaults.variant.as_deref()),
        ] {
            if value.is_some_and(|value| value.trim().is_empty()) {
                return Err(WorkflowSourceError::Invalid {
                    message: format!("{field} must not be empty"),
                    step: None,
                });
            }
        }
        for (index, step) in parsed.steps.iter().enumerate() {
            for (field, value) in [
                ("harness", step.harness.as_deref()),
                ("model", step.model.as_deref()),
                ("variant", step.variant.as_deref()),
            ] {
                if value.is_some_and(|value| value.trim().is_empty()) {
                    return Err(WorkflowSourceError::Invalid {
                        message: format!("step {field} must not be empty"),
                        step: Some(index),
                    });
                }
            }
        }
        Ok(parsed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkflowDiagnostic {
    pub path: PathBuf,
    pub message: String,
    pub byte_start: Option<usize>,
    pub byte_end: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkflowSourceError {
    Syntax {
        message: String,
        span: Option<(usize, usize)>,
    },
    Invalid {
        message: String,
        step: Option<usize>,
    },
    Io(String),
}

impl fmt::Display for WorkflowSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax { message, .. } | Self::Io(message) => formatter.write_str(message),
            Self::Invalid {
                message,
                step: Some(step),
            } => write!(formatter, "step {}: {message}", step + 1),
            Self::Invalid {
                message,
                step: None,
            } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for WorkflowSourceError {}

impl From<std::io::Error> for WorkflowSourceError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TriggerRevision {
    pub name: String,
    pub executable: Option<PathBuf>,
    pub digest: String,
    pub check_only: bool,
    pub repeatable_prepare: bool,
    pub repeatable_finalize: bool,
}

impl TriggerRevision {
    pub fn builtin(name: impl Into<String>, check_only: bool) -> Self {
        let name = name.into();
        Self {
            digest: format!("builtin:{name}:v1"),
            name,
            executable: None,
            check_only,
            repeatable_prepare: true,
            repeatable_finalize: true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TriggerCatalog {
    entries: BTreeMap<String, TriggerRevision>,
}

impl TriggerCatalog {
    pub fn builtins() -> Self {
        let entries = [
            TriggerRevision::builtin("merge_conflict", false),
            TriggerRevision::builtin("needs_review", false),
            TriggerRevision::builtin("ci_failure", false),
            TriggerRevision::builtin("ready_to_merge", true),
        ]
        .into_iter()
        .map(|entry| (entry.name.clone(), entry))
        .collect();
        Self { entries }
    }

    pub fn insert(&mut self, revision: TriggerRevision) {
        self.entries.insert(revision.name.clone(), revision);
    }

    pub fn get(&self, name: &str) -> Option<&TriggerRevision> {
        self.entries.get(name)
    }

    pub fn discover(
        global_root: &Path,
        repository_root: Option<&Path>,
        repository_trusted: bool,
    ) -> Result<Self, WorkflowSourceError> {
        let mut catalog = Self::builtins();
        for root in package_roots(global_root) {
            discover_trigger_directory(&root.join("triggers"), &mut catalog)?;
        }
        discover_trigger_directory(&global_root.join("triggers"), &mut catalog)?;
        if repository_trusted && let Some(repository) = repository_root {
            for root in package_roots(repository) {
                discover_trigger_directory(&root.join("triggers"), &mut catalog)?;
            }
            discover_trigger_directory(&repository.join("triggers"), &mut catalog)?;
        }
        Ok(catalog)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResolvedAgent {
    pub harness: Option<String>,
    pub model: Option<String>,
    pub variant: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompiledWorkflowStep {
    /// Stable runtime key. Anonymous source Steps use `step-<one-based-index>`.
    pub key: String,
    pub explicit_id: bool,
    /// Whether `depends_on` was authored, including an explicit empty root list.
    pub explicit_dependencies: bool,
    pub trigger: Option<TriggerRevision>,
    pub dependencies: Vec<String>,
    pub context: Vec<String>,
    pub agent: ResolvedAgent,
    pub prompt: Option<String>,
    pub followups: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum CompiledWorkflowInput {
    File {
        description: Option<String>,
        default: Option<String>,
        glob: String,
    },
    String {
        description: Option<String>,
        default: Option<String>,
        min_length: Option<usize>,
        max_length: Option<usize>,
    },
    Bool {
        description: Option<String>,
        default: Option<bool>,
    },
    Number {
        description: Option<String>,
        default: Option<serde_json::Number>,
        min: Option<serde_json::Number>,
        max: Option<serde_json::Number>,
    },
    Enum {
        description: Option<String>,
        default: Option<String>,
        options: Vec<String>,
    },
}

fn compile_workflow_input(input: &WorkflowInputSource) -> CompiledWorkflowInput {
    let description = input.description.clone();
    match input.kind.unwrap_or(WorkflowInputType::File) {
        WorkflowInputType::File => CompiledWorkflowInput::File {
            description,
            default: input.default.as_ref().and_then(|default| match default {
                WorkflowInputDefault::String(value) => Some(value.clone()),
                _ => None,
            }),
            glob: input.glob.clone().expect("validated file input has a glob"),
        },
        WorkflowInputType::String => CompiledWorkflowInput::String {
            description,
            default: input.default.as_ref().and_then(|default| match default {
                WorkflowInputDefault::String(value) => Some(value.clone()),
                _ => None,
            }),
            min_length: input.min_length,
            max_length: input.max_length,
        },
        WorkflowInputType::Bool => CompiledWorkflowInput::Bool {
            description,
            default: input.default.as_ref().and_then(|default| match default {
                WorkflowInputDefault::Bool(value) => Some(*value),
                _ => None,
            }),
        },
        WorkflowInputType::Number => CompiledWorkflowInput::Number {
            description,
            default: input.default.as_ref().and_then(|default| match default {
                WorkflowInputDefault::Number(value) => Some(value.clone()),
                _ => None,
            }),
            min: input.min.clone(),
            max: input.max.clone(),
        },
        WorkflowInputType::Enum => CompiledWorkflowInput::Enum {
            description,
            default: input.default.as_ref().and_then(|default| match default {
                WorkflowInputDefault::String(value) => Some(value.clone()),
                _ => None,
            }),
            options: input
                .options
                .clone()
                .expect("validated enum input has options"),
        },
    }
}

impl CompiledWorkflowInput {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::File { .. } => "file",
            Self::String { .. } => "string",
            Self::Bool { .. } => "bool",
            Self::Number { .. } => "number",
            Self::Enum { .. } => "enum",
        }
    }

    pub fn description(&self) -> Option<&str> {
        match self {
            Self::File { description, .. }
            | Self::String { description, .. }
            | Self::Bool { description, .. }
            | Self::Number { description, .. }
            | Self::Enum { description, .. } => description.as_deref(),
        }
    }

    pub fn default_value(&self) -> Option<String> {
        match self {
            Self::File { default, .. }
            | Self::String { default, .. }
            | Self::Enum { default, .. } => default.clone(),
            Self::Bool { default, .. } => default.map(|value| value.to_string()),
            Self::Number { default, .. } => default.as_ref().map(ToString::to_string),
        }
    }

    pub fn is_required(&self) -> bool {
        self.default_value().is_none()
    }

    pub fn file_glob(&self) -> Option<&str> {
        match self {
            Self::File { glob, .. } => Some(glob),
            _ => None,
        }
    }

    pub fn enum_options(&self) -> Option<&[String]> {
        match self {
            Self::Enum { options, .. } => Some(options),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompiledWorkflow {
    pub name: String,
    pub source_path: PathBuf,
    pub source_revision: String,
    pub source: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, CompiledWorkflowInput>,
    #[serde(default)]
    pub input_values: BTreeMap<String, String>,
    pub max_agent_runs: u32,
    pub steps: Vec<CompiledWorkflowStep>,
    pub topological_order: Vec<String>,
    pub digest: String,
}

impl CompiledWorkflow {
    pub fn step(&self, key: &str) -> Option<&CompiledWorkflowStep> {
        self.steps.iter().find(|step| step.key == key)
    }

    pub fn has_explicit_graph(&self) -> bool {
        self.steps.iter().any(|step| step.explicit_dependencies)
    }
}

pub fn compile_workflow(
    path: &Path,
    source: &str,
    triggers: &TriggerCatalog,
) -> Result<CompiledWorkflow, Vec<WorkflowDiagnostic>> {
    let parsed = match WorkflowSource::parse(source) {
        Ok(parsed) => parsed,
        Err(error) => return Err(vec![diagnostic(path, source, error)]),
    };
    compile_parsed(path, source, parsed, triggers)
        .map_err(|error| vec![diagnostic(path, source, error)])
}

fn compile_parsed(
    path: &Path,
    source: &str,
    parsed: WorkflowSource,
    triggers: &TriggerCatalog,
) -> Result<CompiledWorkflow, WorkflowSourceError> {
    let name = workflow_name(path)?;
    let mut explicit = BTreeMap::<String, usize>::new();
    let mut keys = Vec::with_capacity(parsed.steps.len());
    for (index, step) in parsed.steps.iter().enumerate() {
        let key = match step.id.as_deref() {
            Some(id) => {
                validate_name("step id", id, index)?;
                if explicit.insert(id.to_string(), index).is_some() {
                    return Err(invalid_step(index, format!("duplicate step id '{id}'")));
                }
                id.to_string()
            }
            None => format!("step-{}", index + 1),
        };
        keys.push(key);
    }

    let mut dependencies = Vec::with_capacity(parsed.steps.len());
    for (index, step) in parsed.steps.iter().enumerate() {
        let resolved = match &step.depends_on {
            Some(references) => {
                let mut seen = BTreeSet::new();
                let mut result = Vec::new();
                for reference in references {
                    validate_name("dependency", reference, index)?;
                    if !seen.insert(reference.clone()) {
                        return Err(invalid_step(
                            index,
                            format!("dependency '{reference}' is repeated"),
                        ));
                    }
                    let Some(&dependency_index) = explicit.get(reference) else {
                        return Err(invalid_step(
                            index,
                            format!("dependency '{reference}' does not name an explicit step id"),
                        ));
                    };
                    if dependency_index == index {
                        return Err(invalid_step(
                            index,
                            format!("step cannot depend on itself through '{reference}'"),
                        ));
                    }
                    result.push(reference.clone());
                }
                result
            }
            None if index > 0 => vec![keys[index - 1].clone()],
            None => Vec::new(),
        };
        dependencies.push(resolved);
    }

    let order =
        topological_order(&keys, &dependencies).map_err(|cycle| WorkflowSourceError::Invalid {
            message: format!("workflow graph contains a cycle through '{cycle}'"),
            step: keys.iter().position(|key| key == &cycle),
        })?;
    let ancestors = graph_ancestors(&keys, &dependencies, &order);
    let inputs = parsed
        .inputs
        .iter()
        .map(|(name, input)| (name.clone(), compile_workflow_input(input)))
        .collect::<BTreeMap<_, _>>();
    let mut used_inputs = BTreeSet::new();
    let mut steps = Vec::with_capacity(parsed.steps.len());
    for (index, step) in parsed.steps.into_iter().enumerate() {
        let trigger = step
            .trigger
            .as_deref()
            .map(|name| {
                validate_name("trigger", name, index)?;
                triggers
                    .get(name)
                    .cloned()
                    .ok_or_else(|| invalid_step(index, format!("unknown trigger '{name}'")))
            })
            .transpose()?;
        if step
            .prompt
            .as_deref()
            .is_some_and(|prompt| prompt.trim().is_empty())
        {
            return Err(invalid_step(index, "prompt must not be empty"));
        }
        if step.prompt.is_none() && !trigger.as_ref().is_some_and(|trigger| trigger.check_only) {
            return Err(invalid_step(
                index,
                "prompt may be omitted only for a check-only trigger",
            ));
        }
        if step.prompt.is_none() && !step.followups.is_empty() {
            return Err(invalid_step(index, "followups require an initial prompt"));
        }
        if step
            .followups
            .iter()
            .any(|followup| followup.trim().is_empty())
        {
            return Err(invalid_step(
                index,
                "followups must not contain empty prompts",
            ));
        }
        for turn in step.prompt.iter().chain(&step.followups) {
            let placeholders =
                template_placeholders(turn).map_err(|message| invalid_step(index, message))?;
            for placeholder in placeholders {
                if !inputs.contains_key(&placeholder) {
                    return Err(invalid_step(
                        index,
                        format!("prompt references undeclared input '{{{{{placeholder}}}}}'"),
                    ));
                }
                used_inputs.insert(placeholder);
            }
        }
        let mut context_seen = BTreeSet::new();
        for context in &step.context {
            if !context_seen.insert(context.clone()) {
                return Err(invalid_step(
                    index,
                    format!("context step '{context}' is repeated"),
                ));
            }
            let Some(&context_index) = explicit.get(context) else {
                return Err(invalid_step(
                    index,
                    format!("context '{context}' does not name an explicit step id"),
                ));
            };
            if !ancestors[index].contains(&keys[context_index]) {
                return Err(invalid_step(
                    index,
                    format!("context '{context}' is not a predecessor of this step"),
                ));
            }
        }
        let agent = ResolvedAgent {
            harness: step.harness.or_else(|| parsed.defaults.harness.clone()),
            model: step.model.or_else(|| parsed.defaults.model.clone()),
            variant: step.variant.or_else(|| parsed.defaults.variant.clone()),
        };
        if (agent.model.is_some() || agent.variant.is_some())
            && agent
                .harness
                .as_deref()
                .is_some_and(|harness| crate::harness::builtin_adapter(harness).is_none())
        {
            return Err(invalid_step(
                index,
                format!(
                    "generic harness '{}' does not declare model or variant override support",
                    agent.harness.as_deref().unwrap_or_default()
                ),
            ));
        }
        steps.push(CompiledWorkflowStep {
            key: keys[index].clone(),
            explicit_id: step.id.is_some(),
            explicit_dependencies: step.depends_on.is_some(),
            trigger,
            dependencies: dependencies[index].clone(),
            context: step.context,
            agent,
            prompt: step.prompt,
            followups: step.followups,
        });
    }

    if let Some(unused) = inputs.keys().find(|name| !used_inputs.contains(*name)) {
        return Err(WorkflowSourceError::Invalid {
            message: format!("input '{unused}' is never referenced by an Agent prompt"),
            step: None,
        });
    }
    let source_revision = sha256(source.as_bytes());
    let mut compiled = CompiledWorkflow {
        name,
        source_path: path.to_path_buf(),
        source_revision,
        source: source.to_string(),
        inputs,
        input_values: BTreeMap::new(),
        max_agent_runs: parsed
            .defaults
            .max_agent_runs
            .unwrap_or(DEFAULT_MAX_AGENT_RUNS),
        steps,
        topological_order: order,
        digest: String::new(),
    };
    refresh_workflow_digest(&mut compiled)?;
    Ok(compiled)
}

pub fn resolve_workflow_agent_selection(
    workflow: &mut CompiledWorkflow,
    config: &crate::config::Config,
) -> Result<(), Vec<WorkflowDiagnostic>> {
    for step in &mut workflow.steps {
        if step.agent.harness.is_none() {
            step.agent.harness = Some(config.default_harness.clone());
        }
    }
    if let Err(error) = refresh_workflow_digest(workflow) {
        return Err(vec![diagnostic(
            &workflow.source_path,
            &workflow.source,
            error,
        )]);
    }
    validate_workflow_agent_selection(workflow, config)
}

fn refresh_workflow_digest(workflow: &mut CompiledWorkflow) -> Result<(), WorkflowSourceError> {
    workflow.digest.clear();
    let canonical =
        serde_json::to_vec(workflow).map_err(|error| WorkflowSourceError::Io(error.to_string()))?;
    workflow.digest = sha256(&canonical);
    Ok(())
}

/// Apply defaults, validate and canonicalize launch inputs, bind every authored Agent turn,
/// and refresh the immutable digest.
pub fn bind_workflow_inputs(
    workflow: &CompiledWorkflow,
    worktree: &Path,
    supplied: &BTreeMap<String, String>,
) -> Result<CompiledWorkflow, WorkflowSourceError> {
    if let Some(unknown) = supplied
        .keys()
        .find(|name| !workflow.inputs.contains_key(*name))
    {
        return Err(WorkflowSourceError::Invalid {
            message: format!("unknown Workflow input '{unknown}'"),
            step: None,
        });
    }
    let mut values = BTreeMap::new();
    for (name, input) in &workflow.inputs {
        let raw = supplied
            .get(name)
            .cloned()
            .or_else(|| input.default_value())
            .ok_or_else(|| WorkflowSourceError::Invalid {
                message: format!("missing required Workflow input '{name}'"),
                step: None,
            })?;
        let value = validate_workflow_input(worktree, input, &raw).map_err(|error| {
            WorkflowSourceError::Invalid {
                message: format!("Workflow input '{name}': {error}"),
                step: None,
            }
        })?;
        values.insert(name.clone(), value);
    }
    let mut bound = workflow.clone();
    for step in &mut bound.steps {
        if let Some(prompt) = &mut step.prompt {
            *prompt = render_inputs(prompt, &values);
        }
        for followup in &mut step.followups {
            *followup = render_inputs(followup, &values);
        }
    }
    bound.input_values = values;
    refresh_workflow_digest(&mut bound)?;
    Ok(bound)
}

/// Validate one launch value and return the stable text inserted into prompts and snapshots.
pub fn validate_workflow_input(
    worktree: &Path,
    input: &CompiledWorkflowInput,
    value: &str,
) -> Result<String, WorkflowSourceError> {
    match input {
        CompiledWorkflowInput::File { glob, .. } => {
            validate_workflow_file_value(worktree, glob, value)
        }
        CompiledWorkflowInput::String {
            min_length,
            max_length,
            ..
        } => {
            if value.is_empty() {
                return Err(invalid_input_value("must not be empty"));
            }
            if value.chars().any(char::is_control) {
                return Err(invalid_input_value(
                    "must be a single line without control characters",
                ));
            }
            if value.chars().count() > MAX_WORKFLOW_STRING_INPUT_CHARS {
                return Err(invalid_input_value(format!(
                    "must contain at most {MAX_WORKFLOW_STRING_INPUT_CHARS} characters"
                )));
            }
            if !valid_string_input(value, *min_length, *max_length) {
                let requirement = match (min_length, max_length) {
                    (Some(min), Some(max)) => format!("must contain {min} to {max} characters"),
                    (Some(min), None) => format!("must contain at least {min} characters"),
                    (None, Some(max)) => format!("must contain at most {max} characters"),
                    (None, None) => "must not be empty".into(),
                };
                return Err(invalid_input_value(requirement));
            }
            Ok(value.to_string())
        }
        CompiledWorkflowInput::Bool { .. } => match value {
            "true" => Ok("true".into()),
            "false" => Ok("false".into()),
            _ => Err(invalid_input_value("must be 'true' or 'false'")),
        },
        CompiledWorkflowInput::Number { min, max, .. } => {
            let number = value
                .parse::<serde_json::Number>()
                .map_err(|_| invalid_input_value("must be a finite JSON number"))?;
            if !number_in_range(&number, min.as_ref(), max.as_ref()) {
                let requirement = match (min, max) {
                    (Some(min), Some(max)) => format!("must be between {min} and {max}"),
                    (Some(min), None) => format!("must be at least {min}"),
                    (None, Some(max)) => format!("must be at most {max}"),
                    (None, None) => "is outside the supported numeric range".into(),
                };
                return Err(invalid_input_value(requirement));
            }
            Ok(number.to_string())
        }
        CompiledWorkflowInput::Enum { options, .. } => {
            if options.iter().any(|option| option == value) {
                Ok(value.to_string())
            } else {
                Err(invalid_input_value(format!(
                    "must be one of: {}",
                    options.join(", ")
                )))
            }
        }
    }
}

/// Resolve one file input to a normalized worktree-relative path and enforce its authored glob.
pub fn validate_workflow_file_input(
    worktree: &Path,
    input: &CompiledWorkflowInput,
    value: &str,
) -> Result<String, WorkflowSourceError> {
    let Some(glob) = input.file_glob() else {
        return Err(invalid_input_value("is not a file input"));
    };
    validate_workflow_file_value(worktree, glob, value)
}

fn validate_workflow_file_value(
    worktree: &Path,
    glob: &str,
    value: &str,
) -> Result<String, WorkflowSourceError> {
    let root = worktree.canonicalize()?;
    let selected = Path::new(value);
    let selected = if selected.is_absolute() {
        selected.to_path_buf()
    } else {
        root.join(selected)
    };
    let selected = selected
        .canonicalize()
        .map_err(|error| invalid_input_value(format!("file '{value}' is unavailable: {error}")))?;
    if !selected.is_file() {
        return Err(invalid_input_value(format!("'{value}' is not a file")));
    }
    let relative = selected.strip_prefix(&root).map_err(|_| {
        invalid_input_value(format!(
            "file '{}' is outside the worktree",
            selected.display()
        ))
    })?;
    let relative = relative
        .to_str()
        .ok_or_else(|| invalid_input_value("file path is not valid UTF-8"))?;
    let relative = relative.replace(std::path::MAIN_SEPARATOR, "/");
    if relative.chars().any(char::is_control) {
        return Err(invalid_input_value("file path contains control characters"));
    }
    if !input_glob_matches(glob, &relative) {
        return Err(invalid_input_value(format!(
            "file '{relative}' does not match '{glob}'"
        )));
    }
    Ok(relative)
}

fn invalid_input_value(message: impl Into<String>) -> WorkflowSourceError {
    WorkflowSourceError::Invalid {
        message: message.into(),
        step: None,
    }
}

fn valid_string_input(value: &str, min: Option<usize>, max: Option<usize>) -> bool {
    let length = value.chars().count();
    !value.is_empty()
        && length <= MAX_WORKFLOW_STRING_INPUT_CHARS
        && !value.chars().any(char::is_control)
        && min.is_none_or(|min| length >= min)
        && max.is_none_or(|max| length <= max)
}

fn number_range_invalid(
    min: Option<&serde_json::Number>,
    max: Option<&serde_json::Number>,
) -> bool {
    min.zip(max)
        .is_some_and(|(min, max)| compare_json_numbers(min, max).is_gt())
}

fn number_in_range(
    value: &serde_json::Number,
    min: Option<&serde_json::Number>,
    max: Option<&serde_json::Number>,
) -> bool {
    min.is_none_or(|min| !compare_json_numbers(value, min).is_lt())
        && max.is_none_or(|max| !compare_json_numbers(value, max).is_gt())
}

#[derive(Debug)]
struct ComparableJsonNumber {
    negative: bool,
    digits: Vec<u8>,
    decimal_exponent: i64,
}

fn comparable_json_number(number: &serde_json::Number) -> ComparableJsonNumber {
    let text = number.to_string();
    let (negative, unsigned) = text
        .strip_prefix('-')
        .map_or((false, text.as_str()), |unsigned| (true, unsigned));
    let (mantissa, exponent) =
        unsigned
            .split_once(['e', 'E'])
            .map_or((unsigned, 0), |(mantissa, exponent)| {
                (
                    mantissa,
                    exponent
                        .parse::<i64>()
                        .expect("serde_json emits bounded number exponents"),
                )
            });
    let fractional_digits = mantissa
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    let mut digits = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .collect::<Vec<_>>();
    let first_nonzero = digits.iter().position(|digit| *digit != b'0');
    let Some(first_nonzero) = first_nonzero else {
        return ComparableJsonNumber {
            negative: false,
            digits: vec![b'0'],
            decimal_exponent: 0,
        };
    };
    digits.drain(..first_nonzero);
    let mut decimal_exponent = exponent - fractional_digits as i64;
    while digits.len() > 1 && digits.last() == Some(&b'0') {
        digits.pop();
        decimal_exponent += 1;
    }
    ComparableJsonNumber {
        negative,
        digits,
        decimal_exponent,
    }
}

fn compare_json_numbers(
    left: &serde_json::Number,
    right: &serde_json::Number,
) -> std::cmp::Ordering {
    let left = comparable_json_number(left);
    let right = comparable_json_number(right);
    if left.negative != right.negative {
        return if left.negative {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        };
    }
    let magnitude = |number: &ComparableJsonNumber| {
        i64::try_from(number.digits.len()).unwrap_or(i64::MAX) + number.decimal_exponent
    };
    let mut ordering = magnitude(&left).cmp(&magnitude(&right));
    if ordering.is_eq() {
        let width = left.digits.len().max(right.digits.len());
        for index in 0..width {
            let left_digit = left.digits.get(index).copied().unwrap_or(b'0');
            let right_digit = right.digits.get(index).copied().unwrap_or(b'0');
            ordering = left_digit.cmp(&right_digit);
            if !ordering.is_eq() {
                break;
            }
        }
    }
    if left.negative {
        ordering.reverse()
    } else {
        ordering
    }
}

/// Discover a bounded, sorted list of worktree files eligible for a file input picker.
pub fn workflow_file_input_candidates(
    worktree: &Path,
    input: &CompiledWorkflowInput,
) -> Result<Vec<String>, WorkflowSourceError> {
    let Some(glob) = input.file_glob() else {
        return Err(invalid_input_value(
            "cannot list files for a non-file input",
        ));
    };
    const MAX_CANDIDATES: usize = 10_000;
    const MAX_VISITED_ENTRIES: usize = 100_000;
    fn visit(
        root: &Path,
        directory: &Path,
        glob: &str,
        candidates: &mut Vec<String>,
        visited_entries: &mut usize,
    ) -> Result<(), WorkflowSourceError> {
        if candidates.len() >= MAX_CANDIDATES || *visited_entries >= MAX_VISITED_ENTRIES {
            return Ok(());
        }
        let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            *visited_entries = visited_entries.saturating_add(1);
            if *visited_entries > MAX_VISITED_ENTRIES {
                break;
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                let name = entry.file_name();
                if matches!(
                    name.to_str(),
                    Some(".git" | ".prism" | "node_modules" | "target")
                ) {
                    continue;
                }
                visit(root, &path, glob, candidates, visited_entries)?;
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let relative = path.strip_prefix(root).expect("visited path is below root");
            let Some(relative) = relative.to_str() else {
                continue;
            };
            let relative = relative.replace(std::path::MAIN_SEPARATOR, "/");
            if relative.chars().any(char::is_control) {
                continue;
            }
            if input_glob_matches(glob, &relative) {
                candidates.push(relative);
                if candidates.len() >= MAX_CANDIDATES {
                    break;
                }
            }
        }
        Ok(())
    }

    let root = worktree.canonicalize()?;
    let mut candidates = Vec::new();
    let mut visited_entries = 0;
    visit(&root, &root, glob, &mut candidates, &mut visited_entries)?;
    candidates.sort();
    Ok(candidates)
}

fn render_inputs(template: &str, values: &BTreeMap<String, String>) -> String {
    let mut rendered = String::with_capacity(template.len());
    let mut remaining = template;
    while let Some(start) = remaining.find("{{") {
        rendered.push_str(&remaining[..start]);
        let placeholder = &remaining[start + 2..];
        let end = placeholder
            .find("}}")
            .expect("compiled input placeholder is closed");
        let name = &placeholder[..end];
        rendered.push_str(
            values
                .get(name)
                .expect("compiled input placeholder has a bound value"),
        );
        remaining = &placeholder[end + 2..];
    }
    rendered.push_str(remaining);
    rendered
}

fn template_placeholders(template: &str) -> Result<BTreeSet<String>, String> {
    let mut placeholders = BTreeSet::new();
    let mut remaining = template;
    while let Some(start) = remaining.find("{{") {
        remaining = &remaining[start + 2..];
        let end = remaining.find("}}").ok_or_else(|| {
            "Agent prompt contains an unclosed '{{' input placeholder".to_string()
        })?;
        let name = &remaining[..end];
        if !valid_name(name) {
            return Err(format!(
                "Agent prompt contains invalid input placeholder '{{{{{name}}}}}'"
            ));
        }
        placeholders.insert(name.to_string());
        remaining = &remaining[end + 2..];
    }
    Ok(placeholders)
}

fn input_glob_matches(pattern: &str, relative: &str) -> bool {
    let value = if pattern.contains('/') {
        relative
    } else {
        relative.rsplit('/').next().unwrap_or(relative)
    };
    wildcard_matches(pattern, value)
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;
    for token in pattern {
        let mut current = vec![false; value.len() + 1];
        if *token == b'*' {
            current[0] = previous[0];
        }
        for index in 1..=value.len() {
            current[index] = match *token {
                b'*' => previous[index] || (value[index - 1] != b'/' && current[index - 1]),
                b'?' => previous[index - 1] && value[index - 1] != b'/',
                byte => previous[index - 1] && byte == value[index - 1],
            };
        }
        previous = current;
    }
    previous[value.len()]
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_name(kind: &str, value: &str, step: usize) -> Result<(), WorkflowSourceError> {
    if !valid_name(value) {
        return Err(invalid_step(
            step,
            format!("{kind} '{value}' must use letters, numbers, '.', '_' or '-'"),
        ));
    }
    Ok(())
}

fn invalid_step(step: usize, message: impl Into<String>) -> WorkflowSourceError {
    WorkflowSourceError::Invalid {
        message: message.into(),
        step: Some(step),
    }
}

fn workflow_name(path: &Path) -> Result<String, WorkflowSourceError> {
    let name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| WorkflowSourceError::Invalid {
            message: "workflow filename must have a non-empty UTF-8 stem".into(),
            step: None,
        })?;
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(WorkflowSourceError::Invalid {
            message: format!("workflow filename '{name}' contains unsupported characters"),
            step: None,
        });
    }
    Ok(name.to_string())
}

fn topological_order(keys: &[String], dependencies: &[Vec<String>]) -> Result<Vec<String>, String> {
    let indices: BTreeMap<_, _> = keys
        .iter()
        .enumerate()
        .map(|(index, key)| (key.as_str(), index))
        .collect();
    let mut degree = dependencies.iter().map(Vec::len).collect::<Vec<_>>();
    let mut ready = (0..keys.len())
        .filter(|index| degree[*index] == 0)
        .collect::<BTreeSet<_>>();
    let mut order = Vec::with_capacity(keys.len());
    while let Some(index) = ready.pop_first() {
        order.push(keys[index].clone());
        for (candidate, candidate_dependencies) in dependencies.iter().enumerate() {
            if candidate_dependencies
                .iter()
                .any(|dependency| indices.get(dependency.as_str()) == Some(&index))
            {
                degree[candidate] -= 1;
                if degree[candidate] == 0 {
                    ready.insert(candidate);
                }
            }
        }
    }
    if order.len() == keys.len() {
        Ok(order)
    } else {
        Err(keys
            .iter()
            .enumerate()
            .find(|(index, _)| degree[*index] > 0)
            .map(|(_, key)| key.clone())
            .unwrap_or_else(|| "unknown".into()))
    }
}

fn graph_ancestors(
    keys: &[String],
    dependencies: &[Vec<String>],
    order: &[String],
) -> Vec<BTreeSet<String>> {
    let indices: BTreeMap<_, _> = keys
        .iter()
        .enumerate()
        .map(|(index, key)| (key.as_str(), index))
        .collect();
    let mut ancestors = vec![BTreeSet::new(); keys.len()];
    for key in order {
        let index = indices[key.as_str()];
        for dependency in &dependencies[index] {
            let dependency_index = indices[dependency.as_str()];
            ancestors[index].insert(dependency.clone());
            let inherited = ancestors[dependency_index].clone();
            ancestors[index].extend(inherited);
        }
    }
    ancestors
}

fn diagnostic(path: &Path, source: &str, error: WorkflowSourceError) -> WorkflowDiagnostic {
    let (byte_start, byte_end) = match &error {
        WorkflowSourceError::Syntax { span, .. } => span
            .map(|(start, end)| (Some(start), Some(end)))
            .unwrap_or((None, None)),
        WorkflowSourceError::Invalid {
            step: Some(step), ..
        } => step_span(source, *step),
        WorkflowSourceError::Invalid {
            message,
            step: None,
        } => input_diagnostic_span(source, message).unwrap_or((None, None)),
        WorkflowSourceError::Io(_) => (None, None),
    };
    WorkflowDiagnostic {
        path: path.to_path_buf(),
        message: error.to_string(),
        byte_start,
        byte_end,
    }
}

fn input_diagnostic_span(source: &str, message: &str) -> Option<(Option<usize>, Option<usize>)> {
    let name = message.strip_prefix("input '")?.split_once('\'')?.0;
    let markers = [format!("[inputs.{name}]"), format!("[inputs.\"{name}\"]")];
    markers.iter().find_map(|marker| {
        source.find(marker).map(|start| {
            (
                Some(start),
                Some(start.saturating_add(marker.len()).min(source.len())),
            )
        })
    })
}

fn step_span(source: &str, target: usize) -> (Option<usize>, Option<usize>) {
    let starts = source
        .match_indices("[[step]]")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    starts.get(target).map_or((None, None), |start| {
        let end = starts.get(target + 1).copied().unwrap_or(source.len());
        (Some(*start), Some(end))
    })
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowScope {
    Installed,
    User,
    Repository,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiscoveredWorkflow {
    pub name: String,
    pub path: PathBuf,
    pub scope: WorkflowScope,
    pub revision: String,
}

#[derive(Clone, Debug, Default)]
pub struct WorkflowCatalog {
    entries: BTreeMap<String, (DiscoveredWorkflow, CompiledWorkflow)>,
}

impl WorkflowCatalog {
    pub fn discover(
        global_root: &Path,
        repository_root: Option<&Path>,
        repository_trusted: bool,
    ) -> Result<Self, Vec<WorkflowDiagnostic>> {
        let triggers = TriggerCatalog::discover(global_root, repository_root, repository_trusted)
            .map_err(|error| vec![diagnostic(global_root, "", error)])?;
        let mut candidates = Vec::<(WorkflowScope, PathBuf)>::new();
        for package in package_roots(global_root) {
            workflow_files(
                &package.join("workflows"),
                WorkflowScope::Installed,
                &mut candidates,
            )
            .map_err(|error| vec![diagnostic(&package, "", error)])?;
        }
        workflow_files(
            &global_root.join("workflows"),
            WorkflowScope::User,
            &mut candidates,
        )
        .map_err(|error| vec![diagnostic(global_root, "", error)])?;
        if repository_trusted && let Some(repository) = repository_root {
            for package in package_roots(repository) {
                workflow_files(
                    &package.join("workflows"),
                    WorkflowScope::Repository,
                    &mut candidates,
                )
                .map_err(|error| vec![diagnostic(&package, "", error)])?;
            }
            workflow_files(
                &repository.join("workflows"),
                WorkflowScope::Repository,
                &mut candidates,
            )
            .map_err(|error| vec![diagnostic(repository, "", error)])?;
        }
        candidates.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        let mut entries = BTreeMap::new();
        let mut diagnostics = Vec::new();
        for (scope, path) in candidates {
            let source = match fs::read_to_string(&path) {
                Ok(source) => source,
                Err(error) => {
                    diagnostics.push(diagnostic(&path, "", error.into()));
                    continue;
                }
            };
            match compile_workflow(&path, &source, &triggers) {
                Ok(workflow) => {
                    let discovered = DiscoveredWorkflow {
                        name: workflow.name.clone(),
                        path: path.clone(),
                        scope,
                        revision: workflow.source_revision.clone(),
                    };
                    // Candidates are ordered installed < user < repository, so insertion gives
                    // the documented precedence while retaining provenance in the selected entry.
                    entries.insert(workflow.name.clone(), (discovered, workflow));
                }
                Err(mut errors) => diagnostics.append(&mut errors),
            }
        }
        if diagnostics.is_empty() {
            Ok(Self { entries })
        } else {
            Err(diagnostics)
        }
    }

    pub fn from_sources(
        sources: impl IntoIterator<Item = (PathBuf, String)>,
        triggers: &TriggerCatalog,
    ) -> Result<Self, Vec<WorkflowDiagnostic>> {
        let mut entries = BTreeMap::new();
        let mut diagnostics = Vec::new();
        for (path, source) in sources {
            match compile_workflow(&path, &source, triggers) {
                Ok(workflow) => {
                    let discovered = DiscoveredWorkflow {
                        name: workflow.name.clone(),
                        path: path.clone(),
                        scope: WorkflowScope::User,
                        revision: workflow.source_revision.clone(),
                    };
                    if entries
                        .insert(workflow.name.clone(), (discovered, workflow))
                        .is_some()
                    {
                        diagnostics.push(WorkflowDiagnostic {
                            path,
                            message: "duplicate workflow filename identity".into(),
                            byte_start: None,
                            byte_end: None,
                        });
                    }
                }
                Err(mut errors) => diagnostics.append(&mut errors),
            }
        }
        if diagnostics.is_empty() {
            Ok(Self { entries })
        } else {
            Err(diagnostics)
        }
    }

    pub fn list(&self) -> Vec<DiscoveredWorkflow> {
        self.entries
            .values()
            .map(|(discovered, _)| discovered.clone())
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&CompiledWorkflow> {
        self.entries.get(name).map(|(_, workflow)| workflow)
    }
}

fn package_roots(root: &Path) -> Vec<PathBuf> {
    let packages = root.join("packages");
    let Ok(entries) = fs::read_dir(packages) else {
        return Vec::new();
    };
    let mut roots = entries
        .flatten()
        .filter_map(|entry| entry.file_type().ok()?.is_dir().then_some(entry.path()))
        .collect::<Vec<_>>();
    roots.sort();
    roots
}

fn workflow_files(
    directory: &Path,
    scope: WorkflowScope,
    output: &mut Vec<(WorkflowScope, PathBuf)>,
) -> Result<(), WorkflowSourceError> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(());
    };
    let mut entries = entries.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(WorkflowSourceError::Io(format!(
                "workflow source {} must not be a symbolic link",
                entry.path().display()
            )));
        }
        if file_type.is_file()
            && entry.path().extension().and_then(|value| value.to_str()) == Some("toml")
        {
            output.push((scope, entry.path()));
        }
    }
    Ok(())
}

fn discover_trigger_directory(
    directory: &Path,
    catalog: &mut TriggerCatalog,
) -> Result<(), WorkflowSourceError> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(());
    };
    let mut entries = entries.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        #[cfg(unix)]
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| WorkflowSourceError::Io("trigger filename is not UTF-8".into()))?
            .to_string();
        #[cfg(windows)]
        let name = path
            .file_stem()
            .filter(|_| {
                path.extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
            })
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                WorkflowSourceError::Io(format!(
                    "native Windows external trigger {} must use an .exe filename",
                    path.display()
                ))
            })?
            .to_string();
        let bytes = fs::read(&path)?;
        #[cfg(unix)]
        if !bytes.starts_with(b"#!") {
            return Err(WorkflowSourceError::Io(format!(
                "external trigger {} must begin with a shebang",
                path.display()
            )));
        }
        #[cfg(windows)]
        if !bytes.starts_with(b"MZ") {
            return Err(WorkflowSourceError::Io(format!(
                "native Windows external trigger {} must be a PE executable",
                path.display()
            )));
        }
        let directives = trigger_directives(&bytes);
        catalog.insert(TriggerRevision {
            name,
            executable: Some(path),
            digest: sha256(&bytes),
            check_only: directives.contains("check-only"),
            repeatable_prepare: directives.contains("repeatable-prepare"),
            repeatable_finalize: directives.contains("repeatable-finalize"),
        });
    }
    Ok(())
}

fn trigger_directives(bytes: &[u8]) -> BTreeSet<String> {
    String::from_utf8_lossy(&bytes[..bytes.len().min(4096)])
        .lines()
        .take(16)
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("# prism-trigger:")
                .or_else(|| line.strip_prefix("// prism-trigger:"))
        })
        .flat_map(|directives| directives.split(',').map(str::trim))
        .filter(|directive| !directive.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn repository_resource_revision(
    repository_resources: &Path,
) -> Result<Option<crate::resource::ContentRevision>, WorkflowSourceError> {
    let mut files = Vec::new();
    for directory in ["workflows", "triggers", "packages"] {
        collect_resource_files(
            repository_resources,
            &repository_resources.join(directory),
            &mut files,
        )?;
    }
    if files.is_empty() {
        return Ok(None);
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut bytes = Vec::new();
    for (relative, contents) in files {
        bytes.extend_from_slice(relative.to_string_lossy().as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&contents);
        bytes.push(0xff);
    }
    Ok(Some(crate::resource::ContentRevision::digest(&bytes)))
}

pub fn repository_resources_are_trusted(
    global_root: &Path,
    repository_root: &Path,
    repository_resources: &Path,
) -> Result<bool, WorkflowSourceError> {
    let Some(revision) = repository_resource_revision(repository_resources)? else {
        return Ok(false);
    };
    crate::resource::TrustStore::new(global_root.join("state/repository-resource-trust.json"))
        .is_trusted(
            crate::resource::ResourceScope::Repository,
            Some(repository_root),
            &revision,
        )
        .map_err(|error| WorkflowSourceError::Io(error.to_string()))
}

pub fn trust_repository_resources(
    global_root: &Path,
    repository_root: &Path,
    repository_resources: &Path,
) -> Result<crate::resource::TrustRecord, WorkflowSourceError> {
    let revision = repository_resource_revision(repository_resources)?.ok_or_else(|| {
        WorkflowSourceError::Invalid {
            message: "repository has no Workflow or Trigger resources to trust".into(),
            step: None,
        }
    })?;
    crate::resource::TrustStore::new(global_root.join("state/repository-resource-trust.json"))
        .trust(repository_root, &revision, std::time::SystemTime::now())
        .map_err(|error| WorkflowSourceError::Io(error.to_string()))
}

fn collect_resource_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(PathBuf, Vec<u8>)>,
) -> Result<(), WorkflowSourceError> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(());
    };
    let mut entries = entries.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(WorkflowSourceError::Io(format!(
                "repository resource {} must not be a symbolic link",
                entry.path().display()
            )));
        }
        if file_type.is_dir() {
            collect_resource_files(root, &entry.path(), files)?;
        } else if file_type.is_file() {
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|error| WorkflowSourceError::Io(error.to_string()))?
                .to_path_buf();
            files.push((relative, fs::read(entry.path())?));
        }
    }
    Ok(())
}

pub fn archive_legacy_workflow_sources(
    global_root: &Path,
) -> Result<Vec<PathBuf>, WorkflowSourceError> {
    let workflow_root = global_root.join("workflows");
    let Ok(entries) = fs::read_dir(&workflow_root) else {
        return Ok(Vec::new());
    };
    let mut entries = entries.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    let archive_root = global_root.join("archive/generalized-workflows-v1");
    let mut archived = Vec::new();
    for entry in entries {
        if !entry.file_type()?.is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("toml")
        {
            continue;
        }
        let source = fs::read_to_string(entry.path())?;
        let legacy = source.lines().map(str::trim).any(|line| {
            line.starts_with("schema_version")
                || line == "[[steps]]"
                || line.starts_with("capabilities")
        });
        if !legacy {
            continue;
        }
        fs::create_dir_all(&archive_root)?;
        let destination = archive_root.join(entry.file_name());
        if destination.exists() {
            return Err(WorkflowSourceError::Io(format!(
                "cannot archive legacy Workflow {}; {} already exists",
                entry.path().display(),
                destination.display()
            )));
        }
        fs::rename(entry.path(), &destination)?;
        crate::durability::sync_directory(
            &workflow_root,
            crate::durability::DurabilityIntent::Standard,
        )?;
        crate::durability::sync_directory(
            &archive_root,
            crate::durability::DurabilityIntent::Standard,
        )?;
        archived.push(destination);
    }
    Ok(archived)
}

pub fn seed_editable_defaults(global_root: &Path) -> Result<bool, WorkflowSourceError> {
    archive_legacy_workflow_sources(global_root)?;
    let marker = global_root.join("state/prompt-workflow-defaults-v1");
    if marker.is_file() {
        return Ok(false);
    }
    let workflows = global_root.join("workflows");
    fs::create_dir_all(&workflows)?;
    let stabilize = workflows.join("stabilize.toml");
    write_new_owner_only(&stabilize, DEFAULT_STABILIZE_SOURCE.as_bytes())?;
    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = marker.with_extension(format!("tmp-{}", std::process::id()));
    match fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    write_new_owner_only(&temporary, b"prompt-workflow-defaults-v1\n")?;
    fs::rename(temporary, &marker)?;
    crate::durability::sync_directory(
        marker.parent().expect("setup marker has parent"),
        crate::durability::DurabilityIntent::Standard,
    )?;
    Ok(true)
}

pub fn copy_example(global_root: &Path, name: &str) -> Result<PathBuf, WorkflowSourceError> {
    let source = match name {
        "multi-model-review" => MULTI_MODEL_REVIEW_EXAMPLE,
        _ => {
            return Err(WorkflowSourceError::Invalid {
                message: format!("unknown workflow example '{name}'"),
                step: None,
            });
        }
    };
    let destination = global_root.join("workflows").join(format!("{name}.toml"));
    fs::create_dir_all(
        destination
            .parent()
            .expect("workflow destination has parent"),
    )?;
    if !write_new_owner_only(&destination, source.as_bytes())? {
        return Err(WorkflowSourceError::Invalid {
            message: format!("workflow {} already exists", destination.display()),
            step: None,
        });
    }
    Ok(destination)
}

fn write_new_owner_only(path: &Path, bytes: &[u8]) -> Result<bool, WorkflowSourceError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    match options.open(path) {
        Ok(mut file) => {
            #[cfg(windows)]
            if let Err(error) = crate::system::windows_security::secure_path(path, false) {
                drop(file);
                let _ = fs::remove_file(path);
                return Err(error.into());
            }
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(name: &str, source: &str) -> Result<CompiledWorkflow, Vec<WorkflowDiagnostic>> {
        compile_workflow(Path::new(name), source, &TriggerCatalog::builtins())
    }

    #[test]
    fn stabilization_is_short_and_compiles_to_linear_graph() {
        let non_comment = DEFAULT_STABILIZE_SOURCE
            .lines()
            .filter(|line| {
                let line = line.trim();
                !line.is_empty() && !line.starts_with('#')
            })
            .count();
        assert!(non_comment < 50, "default has {non_comment} lines");
        let workflow = compile("stabilize.toml", DEFAULT_STABILIZE_SOURCE).unwrap();
        assert_eq!(workflow.name, "stabilize");
        assert_eq!(workflow.steps.len(), 4);
        assert_eq!(workflow.steps[0].dependencies, Vec::<String>::new());
        assert_eq!(workflow.steps[1].dependencies, ["step-1"]);
        assert_eq!(workflow.steps[2].dependencies, ["step-2"]);
        assert_eq!(workflow.steps[3].dependencies, ["step-3"]);
        assert!(workflow.steps[3].prompt.is_none());
        assert!(!workflow.has_explicit_graph());
    }

    #[test]
    fn explicit_roots_join_and_context_compile() {
        let source = r#"
[[step]]
id = "a"
depends_on = []
prompt = "A"
[[step]]
id = "b"
depends_on = []
prompt = "B"
[[step]]
id = "join"
depends_on = ["a", "b"]
context = ["a", "b"]
prompt = "Join"
"#;
        let workflow = compile("review.toml", source).unwrap();
        assert_eq!(workflow.steps[2].dependencies, ["a", "b"]);
        assert_eq!(workflow.topological_order, ["a", "b", "join"]);
        assert!(workflow.has_explicit_graph());
    }

    #[test]
    fn authored_followups_are_ordered_and_pinned_in_the_snapshot() {
        let source = r#"
[[step]]
prompt = "audit"
followups = ["implement gaps", "verify the result"]
"#;
        let workflow = compile("followups.toml", source).unwrap();
        assert_eq!(
            workflow.steps[0].followups,
            ["implement gaps", "verify the result"]
        );
        assert!(workflow.digest.starts_with("sha256:"));
    }

    #[test]
    fn followups_require_a_nonempty_initial_prompt_and_nonempty_turns() {
        let missing = "[[step]]\ntrigger='ready_to_merge'\nfollowups=['continue']\n";
        assert!(
            compile("missing.toml", missing).unwrap_err()[0]
                .message
                .contains("initial prompt")
        );
        let empty = "[[step]]\nprompt='audit'\nfollowups=['  ']\n";
        assert!(
            compile("empty.toml", empty).unwrap_err()[0]
                .message
                .contains("must not contain empty")
        );
    }

    #[test]
    fn file_inputs_bind_initial_and_followup_prompts_into_a_new_snapshot() {
        let source = r#"
[inputs.plan]
type = "file"
glob = "*.md"

[[step]]
prompt = "Have we fully implemented {{plan}}?"
followups = ["Implement the rest of {{plan}}."]
"#;
        let workflow = compile("plan.toml", source).unwrap();
        assert_eq!(workflow.inputs["plan"].file_glob(), Some("*.md"));
        let original_digest = workflow.digest.clone();
        let root =
            std::env::temp_dir().join(format!("prism-workflow-input-bind-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("plan-workflows.md"), "plan").unwrap();
        let bound = bind_workflow_inputs(
            &workflow,
            &root,
            &BTreeMap::from([("plan".into(), "plan-workflows.md".into())]),
        )
        .unwrap();
        assert_eq!(
            bound.steps[0].prompt.as_deref(),
            Some("Have we fully implemented plan-workflows.md?")
        );
        assert_eq!(
            bound.steps[0].followups,
            ["Implement the rest of plan-workflows.md."]
        );
        assert_eq!(bound.input_values["plan"], "plan-workflows.md");
        assert_ne!(bound.digest, original_digest);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn typed_inputs_apply_defaults_and_canonical_validation() {
        let source = r#"
[inputs.title]
type = "string"
description = "Short release title"
min_length = 3
max_length = 20

[inputs.publish]
type = "bool"
default = false

[inputs.count]
type = "number"
min = 1
max = 5
default = 2.5

[inputs.mode]
type = "enum"
options = ["safe", "fast"]
default = "safe"

[[step]]
prompt = "{{title}} publish={{publish}} count={{count}} mode={{mode}}"
"#;
        let workflow = compile("typed.toml", source).unwrap();
        assert!(workflow.inputs["title"].is_required());
        assert_eq!(
            workflow.inputs["publish"].default_value().as_deref(),
            Some("false")
        );
        assert_eq!(
            workflow.inputs["mode"].enum_options().unwrap(),
            ["safe", "fast"]
        );
        let bound = bind_workflow_inputs(
            &workflow,
            Path::new("."),
            &BTreeMap::from([("title".into(), "Release 1".into())]),
        )
        .unwrap();
        assert_eq!(
            bound.steps[0].prompt.as_deref(),
            Some("Release 1 publish=false count=2.5 mode=safe")
        );
        assert_eq!(bound.input_values.len(), 4);

        for (name, value, expected) in [
            ("title", "x", "3 to 20"),
            ("publish", "yes", "true"),
            ("count", "6", "between 1 and 5"),
            ("mode", "other", "safe, fast"),
        ] {
            let mut supplied = BTreeMap::from([("title".into(), "Release 1".into())]);
            supplied.insert(name.into(), value.into());
            assert!(
                bind_workflow_inputs(&workflow, Path::new("."), &supplied)
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
        }
    }

    #[test]
    fn numeric_ranges_compare_large_values_exactly() {
        let min = "9007199254740993".parse::<serde_json::Number>().unwrap();
        let below = "9007199254740992".parse::<serde_json::Number>().unwrap();

        assert!(!number_in_range(&below, Some(&min), None));
    }

    #[test]
    fn typed_input_source_rejects_invalid_constraints_and_defaults() {
        for (source, expected) in [
            (
                "[inputs.mode]\ntype='enum'\noptions=['safe','safe']\n[[step]]\nprompt='{{mode}}'\n",
                "repeated",
            ),
            (
                "[inputs.mode]\ntype='enum'\noptions=['safe']\ndefault='fast'\n[[step]]\nprompt='{{mode}}'\n",
                "valid enum option",
            ),
            (
                "[inputs.count]\ntype='number'\nmin=5\nmax=1\n[[step]]\nprompt='{{count}}'\n",
                "must not exceed",
            ),
            (
                "[inputs.flag]\ntype='bool'\nglob='*.md'\n[[step]]\nprompt='{{flag}}'\n",
                "does not support 'glob'",
            ),
        ] {
            let diagnostic = compile("invalid-input.toml", source).unwrap_err().remove(0);
            assert!(diagnostic.message.contains(expected));
            assert!(diagnostic.byte_start.is_some());
        }
    }

    #[test]
    fn input_placeholders_must_be_declared_well_formed_and_used() {
        let undeclared = "[[step]]\nprompt='review {{plan}}'\n";
        assert!(
            compile("undeclared.toml", undeclared).unwrap_err()[0]
                .message
                .contains("undeclared input")
        );
        let unclosed = "[[step]]\nprompt='review {{plan'\n";
        assert!(
            compile("unclosed.toml", unclosed).unwrap_err()[0]
                .message
                .contains("unclosed")
        );
        let unused = "[inputs.plan]\nglob='*.md'\n[[step]]\nprompt='review'\n";
        assert!(
            compile("unused.toml", unused).unwrap_err()[0]
                .message
                .contains("never referenced")
        );
    }

    #[test]
    fn file_input_candidates_are_recursive_bounded_and_worktree_relative() {
        let root = std::env::temp_dir().join(format!(
            "prism-workflow-file-input-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("plan.md"), "plan").unwrap();
        fs::write(root.join("docs/nested.md"), "nested").unwrap();
        fs::write(root.join("docs/no.txt"), "no").unwrap();
        fs::write(root.join("target/ignored.md"), "ignored").unwrap();
        let input = CompiledWorkflowInput::File {
            description: None,
            default: None,
            glob: "*.md".into(),
        };

        assert_eq!(
            workflow_file_input_candidates(&root, &input).unwrap(),
            ["docs/nested.md", "plan.md"]
        );
        assert_eq!(
            workflow_file_input_candidates(
                &root,
                &CompiledWorkflowInput::File {
                    description: None,
                    default: None,
                    glob: "docs/*.md".into()
                }
            )
            .unwrap(),
            ["docs/nested.md"]
        );
        assert_eq!(
            validate_workflow_file_input(&root, &input, "docs/nested.md").unwrap(),
            "docs/nested.md"
        );
        assert!(
            validate_workflow_file_input(&root, &input, "docs/no.txt")
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unknown_fields_have_source_spans() {
        let source = "[[step]]\nprompt='run'\nclass='action'\n";
        let diagnostic = compile("invalid.toml", source).unwrap_err().remove(0);
        assert!(diagnostic.message.contains("unknown field"));
        assert!(diagnostic.byte_start.is_some());
    }

    #[test]
    fn dependencies_require_explicit_ids() {
        let source = "[[step]]\nprompt='one'\n[[step]]\ndepends_on=['step-1']\nprompt='two'\n";
        assert!(
            compile("invalid.toml", source).unwrap_err()[0]
                .message
                .contains("explicit step id")
        );
    }

    #[test]
    fn explicit_generic_harness_model_override_is_rejected_during_validation() {
        let source = "[[step]]\nharness='custom'\nmodel='provider/model'\nprompt='work'\n";
        assert!(
            compile("invalid.toml", source).unwrap_err()[0]
                .message
                .contains("does not declare")
        );
    }

    #[test]
    fn cycles_are_rejected_with_a_step_source_span() {
        let source = "[[step]]\nid='a'\ndepends_on=['b']\nprompt='a'\n[[step]]\nid='b'\ndepends_on=['a']\nprompt='b'\n";
        let diagnostic = compile("cycle.toml", source).unwrap_err().remove(0);
        assert!(diagnostic.message.contains("cycle"));
        assert!(diagnostic.byte_start.is_some());
        assert!(diagnostic.byte_end.is_some());
    }

    #[test]
    fn selected_default_harness_is_pinned_into_the_immutable_snapshot() {
        let root = std::env::temp_dir().join(format!(
            "prism-workflow-agent-selection-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let repository = crate::repo::Repository { root: root.clone() };
        let config = crate::config::Config::load(&repository);
        let mut workflow = compile("selection.toml", "[[step]]\nprompt='work'\n").unwrap();
        let original_digest = workflow.digest.clone();

        resolve_workflow_agent_selection(&mut workflow, &config).unwrap();

        assert_eq!(
            workflow.steps[0].agent.harness.as_deref(),
            Some(config.default_harness.as_str())
        );
        assert_ne!(workflow.digest, original_digest);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn followups_reject_a_harness_without_headless_continuation() {
        let root = std::env::temp_dir().join(format!(
            "prism-workflow-followup-selection-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let repository = crate::repo::Repository { root: root.clone() };
        let mut config = crate::config::Config::load(&repository);
        config.default_harness = "custom".into();
        config.harnesses.insert(
            "custom".into(),
            crate::harness::HarnessConfig {
                adapter: "generic".into(),
                interactive_command: vec!["agent".into()],
                arguments: Vec::new(),
                interactive_prompt_transport: None,
                headless_command: Some(vec!["agent".into(), "run".into(), "{prompt}".into()]),
                headless_prompt_transport: Some(crate::harness::PromptTransport::Argument),
                output_format: crate::harness::OutputFormat::JsonLines,
                environment: BTreeMap::new(),
            },
        );
        let mut workflow = compile(
            "followups.toml",
            "[[step]]\nprompt='audit'\nfollowups=['continue']\n",
        )
        .unwrap();

        let diagnostic = resolve_workflow_agent_selection(&mut workflow, &config)
            .unwrap_err()
            .remove(0);
        assert!(diagnostic.message.contains("does not support"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn external_trigger_directives_declare_check_only_and_recovery_policy() {
        let root =
            std::env::temp_dir().join(format!("prism-trigger-directives-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("triggers")).unwrap();
        let executable = root.join("triggers/check-clean");
        fs::write(
            &executable,
            "#!/bin/sh\n# prism-trigger: check-only, repeatable-prepare\n",
        )
        .unwrap();
        let catalog = TriggerCatalog::discover(&root, None, false).unwrap();
        let revision = catalog.get("check-clean").unwrap();
        assert!(revision.check_only);
        assert!(revision.repeatable_prepare);
        assert!(!revision.repeatable_finalize);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_trigger_discovery_uses_native_executable_stem() {
        let root =
            std::env::temp_dir().join(format!("prism-trigger-native-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("triggers")).unwrap();
        let executable = root.join("triggers/check-clean.exe");
        fs::write(&executable, b"MZnative-trigger-fixture").unwrap();
        let catalog = TriggerCatalog::discover(&root, None, false).unwrap();
        let revision = catalog.get("check-clean").unwrap();
        assert_eq!(revision.executable.as_deref(), Some(executable.as_path()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repository_workflow_trust_is_invalidated_by_resource_edits() {
        let root =
            std::env::temp_dir().join(format!("prism-workflow-trust-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let global = root.join("global");
        let repository = root.join("repo");
        let resources = repository.join(".prism");
        fs::create_dir_all(resources.join("workflows")).unwrap();
        fs::write(
            resources.join("workflows/review.toml"),
            "[[step]]\nprompt='review'\n",
        )
        .unwrap();
        assert!(!repository_resources_are_trusted(&global, &repository, &resources).unwrap());
        trust_repository_resources(&global, &repository, &resources).unwrap();
        assert!(repository_resources_are_trusted(&global, &repository, &resources).unwrap());
        fs::write(
            resources.join("workflows/review.toml"),
            "[[step]]\nprompt='changed'\n",
        )
        .unwrap();
        assert!(!repository_resources_are_trusted(&global, &repository, &resources).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_sources_are_archived_before_the_editable_default_is_seeded() {
        let root =
            std::env::temp_dir().join(format!("prism-workflow-archive-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("workflows")).unwrap();
        fs::write(
            root.join("workflows/stabilize.toml"),
            "schema_version=2\n[[steps]]\nclass='action'\n",
        )
        .unwrap();
        let input_workflow = "[inputs.plan]\nglob='*.md'\n[[step]]\nprompt='Review {{plan}}'\n";
        fs::write(root.join("workflows/review-plan.toml"), input_workflow).unwrap();
        assert!(seed_editable_defaults(&root).unwrap());
        assert_eq!(
            fs::read_to_string(root.join("workflows/stabilize.toml")).unwrap(),
            DEFAULT_STABILIZE_SOURCE
        );
        assert!(
            root.join("archive/generalized-workflows-v1/stabilize.toml")
                .is_file()
        );
        assert_eq!(
            fs::read_to_string(root.join("workflows/review-plan.toml")).unwrap(),
            input_workflow
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn default_marker_preserves_edit_and_deletion() {
        let root = std::env::temp_dir().join(format!(
            "prism-workflow-defaults-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(seed_editable_defaults(&root).unwrap());
        let source = root.join("workflows/stabilize.toml");
        fs::write(&source, "custom").unwrap();
        assert!(!seed_editable_defaults(&root).unwrap());
        assert_eq!(fs::read_to_string(&source).unwrap(), "custom");
        fs::remove_file(&source).unwrap();
        assert!(!seed_editable_defaults(&root).unwrap());
        assert!(!source.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
