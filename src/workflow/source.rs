//! Prompt-first Workflow source parsing, graph compilation, discovery, and immutable snapshots.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub const DEFAULT_MAX_AGENT_RUNS: u32 = 10;
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
                crate::harness::Harness::new(harness_id, &harness_config)
                    .headless_with_model(
                        step.prompt.as_deref().unwrap_or("check-only"),
                        std::path::Path::new("."),
                        "Workflow validation",
                        None,
                        crate::harness::AgentSelection {
                            model: step.agent.model.as_deref(),
                            variant: step.agent.variant.as_deref(),
                        },
                        false,
                    )
                    .map(|_| ())
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
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSource {
    #[serde(default)]
    pub defaults: WorkflowDefaults,
    #[serde(rename = "step")]
    pub steps: Vec<WorkflowStepSource>,
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
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompiledWorkflow {
    pub name: String,
    pub source_path: PathBuf,
    pub source_revision: String,
    pub source: String,
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
        });
    }

    let source_revision = sha256(source.as_bytes());
    let mut compiled = CompiledWorkflow {
        name,
        source_path: path.to_path_buf(),
        source_revision,
        source: source.to_string(),
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

fn validate_name(kind: &str, value: &str, step: usize) -> Result<(), WorkflowSourceError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
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
        WorkflowSourceError::Invalid { step: None, .. } | WorkflowSourceError::Io(_) => {
            (None, None)
        }
    };
    WorkflowDiagnostic {
        path: path.to_path_buf(),
        message: error.to_string(),
        byte_start,
        byte_end,
    }
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
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| WorkflowSourceError::Io("trigger filename is not UTF-8".into()))?
            .to_string();
        let bytes = fs::read(&path)?;
        if !bytes.starts_with(b"#!") {
            return Err(WorkflowSourceError::Io(format!(
                "external trigger {} must begin with a shebang",
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
                || line.starts_with("[inputs.")
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
    options.write(true).create_new(true).mode(0o600);
    match options.open(path) {
        Ok(mut file) => {
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
        assert!(seed_editable_defaults(&root).unwrap());
        assert_eq!(
            fs::read_to_string(root.join("workflows/stabilize.toml")).unwrap(),
            DEFAULT_STABILIZE_SOURCE
        );
        assert!(
            root.join("archive/generalized-workflows-v1/stabilize.toml")
                .is_file()
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
