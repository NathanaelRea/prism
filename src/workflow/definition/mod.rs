#![allow(dead_code)] // Execution consumers arrive at the coordinated production cutover.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::util::prism_config_dir;

pub(crate) const WORKFLOW_SCHEMA_JSON: &str = include_str!("../../../schemas/workflow.schema.json");
pub(crate) const WORKFLOW_EXAMPLE: &str = include_str!("../../../assets/workflows/action.toml");
const BUILTINS: [(&str, &str); 9] = [
    (
        "approval",
        include_str!("../../../assets/workflows/approval.toml"),
    ),
    (
        "action",
        include_str!("../../../assets/workflows/action.toml"),
    ),
    (
        "agent",
        include_str!("../../../assets/workflows/agent.toml"),
    ),
    (
        "plan-phase",
        include_str!("../../../assets/workflows/plan-phase.toml"),
    ),
    ("plan", include_str!("../../../assets/workflows/plan.toml")),
    (
        "coding",
        include_str!("../../../assets/workflows/coding.toml"),
    ),
    (
        "merge",
        include_str!("../../../assets/workflows/merge.toml"),
    ),
    (
        "stabilization",
        include_str!("../../../assets/workflows/stabilization.toml"),
    ),
    (
        "triage",
        include_str!("../../../assets/workflows/triage.toml"),
    ),
];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceNamespace {
    Builtin,
    Global,
    Repository,
}

impl SourceNamespace {
    fn label(&self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Global => "global",
            Self::Repository => "repository",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SourceIdentity {
    pub namespace: SourceNamespace,
    pub name: String,
    pub revision: String,
    pub digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

impl SourceIdentity {
    pub(crate) fn qualified_name(&self) -> String {
        format!("{}:{}", self.namespace.label(), self.name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Diagnostic {
    pub code: String,
    pub message: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PrimitiveClass {
    Action,
    Gate,
    Approval,
    Wait,
    Notification,
    WorkflowCall,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Capability {
    RepositoryRead,
    WorkspaceRead,
    WorkspaceWrite,
    ProcessExecute,
    NetworkRead,
    ProviderRead,
    ProviderWrite,
    GitCommit,
    GitRefMutation,
    GitPush,
    Merge,
    WorktrunkLifecycle,
    SecretUse,
    ChildWorkflowCreate,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Port {
    pub artifact_type: String,
    #[serde(default = "default_true")]
    pub required: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InputBinding {
    pub from: String,
    pub artifact_type: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowBudgets {
    pub max_attempts: u32,
    pub max_fan_out: u32,
    pub max_child_depth: u32,
    pub max_mutations: u32,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepSettings {
    pub prompt: Option<String>,
    pub harness: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub command: Vec<String>,
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub retry: RetrySettings,
    #[serde(default)]
    pub continuation: ContinuationSettings,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetrySettings {
    pub max_attempts: u32,
    pub backoff_ms: u64,
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self {
            max_attempts: 1,
            backoff_ms: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContinuationSettings {
    #[default]
    Reject,
    Supported,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PinnedWorkflow {
    pub qualified_name: String,
    pub revision: String,
    pub digest: String,
    #[serde(default)]
    pub capabilities: BTreeSet<Capability>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdmissionPolicySnapshot {
    pub id: String,
    pub revision: String,
    #[serde(default)]
    pub capabilities: BTreeSet<Capability>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawDefinition {
    schema_version: u32,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    ordered: bool,
    #[serde(default)]
    capabilities: BTreeSet<Capability>,
    #[serde(default)]
    inputs: BTreeMap<String, Port>,
    #[serde(default)]
    outputs: BTreeMap<String, Port>,
    budgets: WorkflowBudgets,
    admission_policy: Option<AdmissionPolicySnapshot>,
    #[serde(default)]
    triggers: Vec<RawTrigger>,
    #[serde(default)]
    implementations: Vec<RawImplementation>,
    steps: Vec<RawStep>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawTrigger {
    id: String,
    #[serde(default)]
    enabled: bool,
    definition_selector: String,
    admission_purpose: String,
    kind: String,
    expression: Option<String>,
    timezone: Option<String>,
    missed: Option<crate::trigger::MissedOccurrencePolicy>,
    repository: Option<String>,
    event: Option<String>,
    overlap: crate::trigger::OverlapPolicy,
    max_fan_out: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawImplementation {
    id: String,
    adapter: String,
    class: PrimitiveClass,
    #[serde(default)]
    capabilities: BTreeSet<Capability>,
    #[serde(default)]
    inputs: BTreeMap<String, Port>,
    #[serde(default)]
    outputs: BTreeMap<String, Port>,
    effect: EffectClass,
    target: TargetRequirement,
    #[serde(default)]
    settings: StepSettings,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    id: String,
    class: PrimitiveClass,
    implementation: String,
    #[serde(default)]
    depends_on: Vec<String>,
    condition: Option<String>,
    #[serde(default)]
    capabilities: BTreeSet<Capability>,
    #[serde(default)]
    inputs: BTreeMap<String, InputBinding>,
    #[serde(default)]
    outputs: BTreeMap<String, Port>,
    #[serde(default)]
    settings: StepSettings,
    child_workflow: Option<PinnedWorkflow>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct ImplementationDescriptor {
    pub id: String,
    pub revision: u32,
    pub class: PrimitiveClass,
    pub capabilities: BTreeSet<Capability>,
    pub inputs: BTreeMap<String, String>,
    pub outputs: BTreeMap<String, String>,
    pub effect: EffectClass,
    pub target: TargetRequirement,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EffectClass {
    ReadOnly,
    WorkspaceMutation,
    Brokered,
    HumanDecision,
    DurableWait,
    BestEffort,
    ChildRun,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TargetRequirement {
    Any,
    Local,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct CompiledStep {
    pub id: String,
    pub class: PrimitiveClass,
    pub implementation: String,
    pub implementation_revision: u32,
    pub dependencies: Vec<String>,
    pub condition: Option<ConditionExpr>,
    pub capabilities: BTreeSet<Capability>,
    pub inputs: BTreeMap<String, InputBinding>,
    pub outputs: BTreeMap<String, Port>,
    pub settings: StepSettings,
    pub child_workflow: Option<PinnedWorkflow>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct SnapshotContent {
    pub schema_version: u32,
    pub qualified_name: String,
    pub source_revision: String,
    pub source_digest: String,
    pub description: String,
    pub capabilities: BTreeSet<Capability>,
    pub inputs: BTreeMap<String, Port>,
    pub outputs: BTreeMap<String, Port>,
    pub budgets: WorkflowBudgets,
    pub steps: Vec<CompiledStep>,
    pub implementations: Vec<ImplementationDescriptor>,
    pub admission_policy: Option<AdmissionPolicySnapshot>,
    #[serde(default)]
    pub triggers: Vec<crate::trigger::TriggerDefinition>,
    pub pinned_workflows: Vec<PinnedWorkflow>,
    /// Canonical reachable child snapshots, keyed by digest, so launched runs
    /// never need to reread mutable Workflow sources before creating children.
    #[serde(default)]
    pub pinned_snapshots: BTreeMap<String, SnapshotContent>,
    pub transitive_capabilities: BTreeSet<Capability>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct DefinitionSnapshot {
    pub digest: String,
    pub content: SnapshotContent,
    #[serde(skip)]
    pub canonical_bytes: Vec<u8>,
    #[serde(skip)]
    pub source_trust_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct DefinitionSummary {
    pub source: SourceIdentity,
    pub valid: bool,
    pub trust_required: bool,
    pub requested_capabilities: BTreeSet<Capability>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct DefinitionPreview {
    pub schema_version: u32,
    pub source: SourceIdentity,
    pub trust_required: bool,
    pub snapshot: DefinitionSnapshot,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) enum ConditionExpr {
    Literal(bool),
    Reference(String),
    Not(Box<ConditionExpr>),
    All(Vec<ConditionExpr>),
    Any(Vec<ConditionExpr>),
    Equal { left: String, right: String },
    NotEqual { left: String, right: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObservedBool {
    Value(bool),
    Missing,
    Stale,
    Unknown,
    Unavailable,
}

impl ConditionExpr {
    pub(crate) fn evaluate(&self, values: &BTreeMap<String, ObservedBool>) -> ObservedBool {
        match self {
            Self::Literal(value) => ObservedBool::Value(*value),
            Self::Reference(name) => values.get(name).copied().unwrap_or(ObservedBool::Missing),
            Self::Not(value) => match value.evaluate(values) {
                ObservedBool::Value(value) => ObservedBool::Value(!value),
                other => other,
            },
            Self::All(expressions) => combine_conditions(expressions, values, false),
            Self::Any(expressions) => combine_conditions(expressions, values, true),
            Self::Equal { left, right } => compare_bool(left, right, values, false),
            Self::NotEqual { left, right } => compare_bool(left, right, values, true),
        }
    }
}

fn combine_conditions(
    expressions: &[ConditionExpr],
    values: &BTreeMap<String, ObservedBool>,
    any: bool,
) -> ObservedBool {
    let mut unresolved = None;
    for expression in expressions {
        match expression.evaluate(values) {
            ObservedBool::Value(value) if value == any => return ObservedBool::Value(any),
            ObservedBool::Value(_) => {}
            other => unresolved = Some(worse_quality(unresolved, other)),
        }
    }
    unresolved.unwrap_or(ObservedBool::Value(!any))
}

fn worse_quality(current: Option<ObservedBool>, next: ObservedBool) -> ObservedBool {
    fn rank(value: ObservedBool) -> u8 {
        match value {
            ObservedBool::Unavailable => 4,
            ObservedBool::Unknown => 3,
            ObservedBool::Stale => 2,
            ObservedBool::Missing => 1,
            ObservedBool::Value(_) => 0,
        }
    }
    current
        .filter(|value| rank(*value) >= rank(next))
        .unwrap_or(next)
}

fn compare_bool(
    left: &str,
    right: &str,
    values: &BTreeMap<String, ObservedBool>,
    invert: bool,
) -> ObservedBool {
    let resolve = |value: &str| match value {
        "true" => ObservedBool::Value(true),
        "false" => ObservedBool::Value(false),
        name => values.get(name).copied().unwrap_or(ObservedBool::Missing),
    };
    match (resolve(left), resolve(right)) {
        (ObservedBool::Value(left), ObservedBool::Value(right)) => {
            ObservedBool::Value((left == right) != invert)
        }
        (left, right) => worse_quality(Some(left), right),
    }
}

struct SourceDocument {
    identity: SourceIdentity,
    text: String,
}

pub(crate) struct DefinitionCatalog {
    global_dir: PathBuf,
    repository_dir: Option<PathBuf>,
    implementations: BTreeMap<String, ImplementationDescriptor>,
}

impl DefinitionCatalog {
    pub(crate) fn discover(repository: Option<&Path>) -> Self {
        Self {
            global_dir: prism_config_dir().join("workflows"),
            repository_dir: repository.map(|path| path.join(".prism/workflows")),
            implementations: builtin_implementations(),
        }
    }

    #[cfg(test)]
    fn at(global_dir: PathBuf, repository_dir: Option<PathBuf>) -> Self {
        Self {
            global_dir,
            repository_dir,
            implementations: builtin_implementations(),
        }
    }

    pub(crate) fn list(&self) -> Result<Vec<DefinitionSummary>, String> {
        let documents = self.documents()?;
        Ok(documents
            .into_iter()
            .map(|document| {
                let compile = self.compile(&document);
                let capabilities = compile
                    .as_ref()
                    .map(|snapshot| snapshot.content.capabilities.clone())
                    .unwrap_or_default();
                let diagnostics = compile.err().into_iter().flatten().collect::<Vec<_>>();
                DefinitionSummary {
                    trust_required: document.identity.namespace == SourceNamespace::Repository,
                    source: document.identity,
                    valid: diagnostics.is_empty(),
                    requested_capabilities: capabilities,
                    diagnostics,
                }
            })
            .collect())
    }

    pub(crate) fn resolve(&self, selector: &str) -> Result<DefinitionSnapshot, Vec<Diagnostic>> {
        let document = self.select(selector).map_err(|message| {
            vec![Diagnostic {
                code: "definition_not_found".to_string(),
                message,
                source: selector.to_string(),
                path: None,
                line: None,
                column: None,
                step: None,
            }]
        })?;
        self.compile(&document)
    }

    pub(crate) fn preview(&self, selector: &str) -> Result<DefinitionPreview, Vec<Diagnostic>> {
        let document = self.select(selector).map_err(|message| {
            vec![Diagnostic {
                code: "definition_not_found".to_string(),
                message,
                source: selector.to_string(),
                path: None,
                line: None,
                column: None,
                step: None,
            }]
        })?;
        let snapshot = self.compile(&document)?;
        Ok(DefinitionPreview {
            schema_version: 1,
            trust_required: document.identity.namespace == SourceNamespace::Repository,
            source: document.identity,
            snapshot,
        })
    }

    fn select(&self, selector: &str) -> Result<SourceDocument, String> {
        let documents = self.documents()?;
        let (namespace, name) = selector
            .split_once(':')
            .map(|(namespace, name)| (Some(namespace), name))
            .unwrap_or((None, selector));
        let matches = documents
            .into_iter()
            .filter(|document| {
                document.identity.name == name
                    && namespace
                        .is_none_or(|namespace| namespace == document.identity.namespace.label())
            })
            .collect::<Vec<_>>();
        match matches.len() {
            0 => Err(format!("workflow definition '{selector}' was not found")),
            1 => Ok(matches.into_iter().next().expect("one definition")),
            _ => Err(format!(
                "workflow definition '{selector}' is ambiguous; use builtin:, global:, or repository:"
            )),
        }
    }

    fn documents(&self) -> Result<Vec<SourceDocument>, String> {
        let mut documents = BUILTINS
            .iter()
            .map(|(name, text)| source_document(SourceNamespace::Builtin, name, None, text))
            .collect::<Vec<_>>();
        documents.extend(read_source_dir(SourceNamespace::Global, &self.global_dir)?);
        if let Some(directory) = &self.repository_dir {
            documents.extend(read_source_dir(SourceNamespace::Repository, directory)?);
        }
        documents.sort_by(|left, right| {
            left.identity
                .qualified_name()
                .cmp(&right.identity.qualified_name())
        });
        Ok(documents)
    }

    fn compile(&self, document: &SourceDocument) -> Result<DefinitionSnapshot, Vec<Diagnostic>> {
        self.compile_with_stack(document, &[])
    }

    fn compile_with_stack(
        &self,
        document: &SourceDocument,
        ancestors: &[String],
    ) -> Result<DefinitionSnapshot, Vec<Diagnostic>> {
        let raw = toml::from_str::<RawDefinition>(&document.text).map_err(|error| {
            let (line, column) = error.span().map_or((None, None), |span| {
                let prefix = &document.text[..span.start.min(document.text.len())];
                let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
                let column = prefix.rsplit('\n').next().map(str::len).unwrap_or(0) + 1;
                (Some(line), Some(column))
            });
            vec![Diagnostic {
                code: "invalid_toml".to_string(),
                message: error.to_string(),
                source: document.identity.qualified_name(),
                path: document.identity.path.clone(),
                line,
                column,
                step: None,
            }]
        })?;
        let mut diagnostics = Vec::new();
        if raw.schema_version != 1 {
            diagnostics.push(diagnostic(
                &document.identity,
                "unsupported_schema",
                format!(
                    "workflow schema version {} is not supported",
                    raw.schema_version
                ),
                None,
            ));
        }
        if raw.name != document.identity.name {
            diagnostics.push(diagnostic(
                &document.identity,
                "name_mismatch",
                format!("definition name '{}' must match its source name", raw.name),
                None,
            ));
        }
        validate_identifier(
            &document.identity,
            &raw.name,
            "definition",
            None,
            &mut diagnostics,
        );
        if raw.steps.is_empty() {
            diagnostics.push(diagnostic(
                &document.identity,
                "empty_graph",
                "a workflow must contain at least one Step".to_string(),
                None,
            ));
        }
        if raw.budgets.max_attempts == 0 || raw.budgets.max_fan_out == 0 {
            diagnostics.push(diagnostic(
                &document.identity,
                "invalid_budget",
                "max_attempts and max_fan_out must be greater than zero".to_string(),
                None,
            ));
        }

        let mut implementations = self.implementations.clone();
        let mut implementation_settings = BTreeMap::new();
        for implementation in &raw.implementations {
            let source_prefix = format!("{}:", document.identity.namespace.label());
            if !implementation.id.starts_with(&source_prefix) {
                diagnostics.push(diagnostic(
                    &document.identity,
                    "implementation_namespace_mismatch",
                    format!(
                        "Step Implementation '{}' must use the source namespace '{source_prefix}'",
                        implementation.id
                    ),
                    None,
                ));
                continue;
            }
            if implementation.adapter != "command"
                || implementation.class != PrimitiveClass::Action
                || !matches!(
                    implementation.effect,
                    EffectClass::ReadOnly | EffectClass::WorkspaceMutation
                )
            {
                diagnostics.push(diagnostic(
                    &document.identity,
                    "unsupported_implementation_adapter",
                    format!("custom Step Implementation '{}' must currently use the command Action adapter", implementation.id),
                    None,
                ));
                continue;
            }
            if implementation.settings.command.is_empty() {
                diagnostics.push(diagnostic(
                    &document.identity,
                    "missing_implementation_command",
                    format!(
                        "command Step Implementation '{}' has no structured argv",
                        implementation.id
                    ),
                    None,
                ));
            }
            let descriptor = ImplementationDescriptor {
                id: implementation.id.clone(),
                revision: 1,
                class: implementation.class,
                capabilities: implementation.capabilities.clone(),
                inputs: implementation
                    .inputs
                    .iter()
                    .map(|(name, port)| (name.clone(), port.artifact_type.clone()))
                    .collect(),
                outputs: implementation
                    .outputs
                    .iter()
                    .map(|(name, port)| (name.clone(), port.artifact_type.clone()))
                    .collect(),
                effect: implementation.effect,
                target: implementation.target,
            };
            if implementations
                .insert(implementation.id.clone(), descriptor)
                .is_some()
            {
                diagnostics.push(diagnostic(
                    &document.identity,
                    "duplicate_implementation",
                    format!(
                        "Step Implementation '{}' is already defined",
                        implementation.id
                    ),
                    None,
                ));
            }
            implementation_settings
                .insert(implementation.id.clone(), implementation.settings.clone());
        }

        let mut ids = BTreeSet::new();
        let mut steps = Vec::new();
        for (index, step) in raw.steps.iter().enumerate() {
            validate_identifier(
                &document.identity,
                &step.id,
                "Step",
                Some(&step.id),
                &mut diagnostics,
            );
            if !ids.insert(step.id.clone()) {
                diagnostics.push(diagnostic(
                    &document.identity,
                    "duplicate_step",
                    format!("duplicate Step ID '{}'", step.id),
                    Some(&step.id),
                ));
            }
            let implementation = implementations.get(&step.implementation);
            match implementation {
                None => diagnostics.push(diagnostic(
                    &document.identity,
                    "unknown_implementation",
                    format!("unknown Step Implementation '{}'", step.implementation),
                    Some(&step.id),
                )),
                Some(descriptor) if descriptor.class != step.class => diagnostics.push(diagnostic(
                    &document.identity,
                    "implementation_class_mismatch",
                    format!(
                        "implementation '{}' has class {:?}, not {:?}",
                        step.implementation, descriptor.class, step.class
                    ),
                    Some(&step.id),
                )),
                _ => {}
            }
            if let Some(descriptor) = implementation {
                for capability in &descriptor.capabilities {
                    if !step.capabilities.contains(capability) {
                        diagnostics.push(diagnostic(
                            &document.identity,
                            "implementation_capability_missing",
                            format!(
                                "Step '{}' must declare implementation capability {capability:?}",
                                step.id
                            ),
                            Some(&step.id),
                        ));
                    }
                }
                validate_implementation_ports(
                    &document.identity,
                    step,
                    descriptor,
                    &mut diagnostics,
                );
            }
            for capability in &step.capabilities {
                if !raw.capabilities.contains(capability) {
                    diagnostics.push(diagnostic(
                        &document.identity,
                        "undeclared_capability",
                        format!(
                            "Step '{}' requests undeclared capability {capability:?}",
                            step.id
                        ),
                        Some(&step.id),
                    ));
                }
            }
            let mut dependencies = step.depends_on.clone();
            if raw.ordered && dependencies.is_empty() && index > 0 {
                dependencies.push(raw.steps[index - 1].id.clone());
            }
            let condition = match step.condition.as_deref() {
                Some(text) => match parse_condition(text) {
                    Ok(condition) => Some(condition),
                    Err(message) => {
                        diagnostics.push(diagnostic(
                            &document.identity,
                            "invalid_condition",
                            message,
                            Some(&step.id),
                        ));
                        None
                    }
                },
                None => None,
            };
            steps.push(CompiledStep {
                id: step.id.clone(),
                class: step.class,
                implementation: step.implementation.clone(),
                implementation_revision: implementation.map_or(0, |value| value.revision),
                dependencies,
                condition,
                capabilities: step.capabilities.clone(),
                inputs: step.inputs.clone(),
                outputs: step.outputs.clone(),
                settings: if step.settings == StepSettings::default() {
                    implementation_settings
                        .get(&step.implementation)
                        .cloned()
                        .unwrap_or_default()
                } else {
                    step.settings.clone()
                },
                child_workflow: step.child_workflow.clone(),
            });
            if step.class == PrimitiveClass::WorkflowCall && step.child_workflow.is_none() {
                diagnostics.push(diagnostic(
                    &document.identity,
                    "unpinned_child_workflow",
                    format!(
                        "Workflow Call Step '{}' must pin an exact child Workflow",
                        step.id
                    ),
                    Some(&step.id),
                ));
            }
        }
        let mut lineage = ancestors.to_vec();
        lineage.push(document.identity.qualified_name());
        let mut pinned_snapshots = BTreeMap::new();
        for step in &steps {
            let Some(pinned) = &step.child_workflow else {
                continue;
            };
            if lineage.contains(&pinned.qualified_name) {
                diagnostics.push(diagnostic(
                    &document.identity,
                    "recursive_child_workflow",
                    format!(
                        "Workflow Call Step '{}' recursively reaches '{}'",
                        step.id, pinned.qualified_name
                    ),
                    Some(&step.id),
                ));
                continue;
            }
            let child_document = match self.select(&pinned.qualified_name) {
                Ok(document) => document,
                Err(message) => {
                    diagnostics.push(diagnostic(
                        &document.identity,
                        "child_workflow_not_found",
                        message,
                        Some(&step.id),
                    ));
                    continue;
                }
            };
            match self.compile_with_stack(&child_document, &lineage) {
                Ok(child)
                    if child.digest == pinned.digest
                        && child.content.source_revision == pinned.revision
                        && child.content.qualified_name == pinned.qualified_name
                        && child.content.transitive_capabilities == pinned.capabilities =>
                {
                    pinned_snapshots.insert(child.digest, child.content);
                }
                Ok(child) => diagnostics.push(diagnostic(
                    &document.identity,
                    "child_workflow_pin_mismatch",
                    format!(
                        "Workflow Call Step '{}' pins {}@{} ({}) but resolved {}@{} ({})",
                        step.id,
                        pinned.qualified_name,
                        pinned.revision,
                        pinned.digest,
                        child.content.qualified_name,
                        child.content.source_revision,
                        child.digest,
                    ),
                    Some(&step.id),
                )),
                Err(child_diagnostics) => diagnostics.extend(child_diagnostics),
            }
        }
        validate_graph(&document.identity, &steps, &mut diagnostics);
        validate_bindings(&document.identity, &raw.inputs, &steps, &mut diagnostics);
        validate_conditions(&document.identity, &raw.inputs, &steps, &mut diagnostics);
        if !diagnostics.is_empty() {
            return Err(diagnostics);
        }

        let triggers = raw
            .triggers
            .iter()
            .map(|trigger| compile_trigger(&document.identity, trigger))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|message| {
                vec![diagnostic(
                    &document.identity,
                    "invalid_trigger",
                    message,
                    None,
                )]
            })?;
        let used_implementations = steps
            .iter()
            .filter_map(|step| implementations.get(&step.implementation).cloned())
            .map(|descriptor| (descriptor.id.clone(), descriptor))
            .collect::<BTreeMap<_, _>>();
        let semantic_source_digest =
            sha256(&serde_json::to_vec(&raw).expect("parsed definition serializes canonically"));
        let pinned_workflows = steps
            .iter()
            .filter_map(|step| step.child_workflow.clone())
            .collect::<Vec<_>>();
        let mut transitive_capabilities = raw.capabilities.clone();
        for child in &pinned_workflows {
            transitive_capabilities.extend(child.capabilities.iter().cloned());
        }
        if let Some(policy) = &raw.admission_policy {
            transitive_capabilities.extend(policy.capabilities.iter().cloned());
        }
        let content = SnapshotContent {
            schema_version: 1,
            qualified_name: document.identity.qualified_name(),
            source_revision: if document.identity.namespace == SourceNamespace::Builtin {
                document.identity.revision.clone()
            } else {
                semantic_source_digest.clone()
            },
            source_digest: semantic_source_digest,
            description: raw.description,
            capabilities: raw.capabilities,
            inputs: raw.inputs,
            outputs: raw.outputs,
            budgets: raw.budgets,
            steps,
            implementations: used_implementations.into_values().collect(),
            admission_policy: raw.admission_policy,
            triggers,
            pinned_workflows,
            pinned_snapshots,
            transitive_capabilities,
        };
        let canonical_bytes = serde_json::to_vec(&content).expect("snapshot content serializes");
        let digest = sha256(&canonical_bytes);
        Ok(DefinitionSnapshot {
            digest,
            content,
            canonical_bytes,
            source_trust_digest: document.identity.digest.clone(),
        })
    }
}

fn compile_trigger(
    source: &SourceIdentity,
    raw: &RawTrigger,
) -> Result<crate::trigger::TriggerDefinition, String> {
    if raw.id.is_empty() || raw.max_fan_out == 0 {
        return Err(
            "Trigger id must be non-empty and max_fan_out must be greater than zero".to_string(),
        );
    }
    let kind = match raw.kind.as_str() {
        "manual" => crate::trigger::TriggerKind::Manual,
        "schedule" => crate::trigger::TriggerKind::Schedule {
            expression: raw
                .expression
                .clone()
                .ok_or_else(|| "scheduled Trigger requires expression".to_string())?,
            timezone: raw
                .timezone
                .clone()
                .ok_or_else(|| "scheduled Trigger requires timezone".to_string())?,
            missed: raw
                .missed
                .ok_or_else(|| "scheduled Trigger requires missed policy".to_string())?,
        },
        "provider_event" => crate::trigger::TriggerKind::ProviderEvent {
            repository: raw
                .repository
                .clone()
                .ok_or_else(|| "provider Trigger requires repository".to_string())?,
            event: raw
                .event
                .clone()
                .ok_or_else(|| "provider Trigger requires event".to_string())?,
        },
        value => return Err(format!("unknown Trigger kind '{value}'")),
    };
    let definition = crate::trigger::TriggerDefinition {
        id: format!("{}/{}", source.qualified_name(), raw.id),
        enabled: raw.enabled,
        definition_selector: raw.definition_selector.clone(),
        admission_purpose: raw.admission_purpose.clone(),
        kind,
        overlap: raw.overlap,
        max_fan_out: raw.max_fan_out,
    };
    crate::trigger::validate_definition(&definition)?;
    Ok(definition)
}

fn source_document(
    namespace: SourceNamespace,
    name: &str,
    path: Option<PathBuf>,
    text: &str,
) -> SourceDocument {
    let digest = sha256(text.as_bytes());
    let revision = if namespace == SourceNamespace::Builtin {
        "1".to_string()
    } else {
        digest.clone()
    };
    SourceDocument {
        identity: SourceIdentity {
            namespace,
            name: name.to_string(),
            revision,
            digest,
            path,
        },
        text: text.to_string(),
    }
}

fn read_source_dir(
    namespace: SourceNamespace,
    directory: &Path,
) -> Result<Vec<SourceDocument>, String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "read workflow directory {}: {error}",
                directory.display()
            ));
        }
    };
    let mut paths = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read workflow directory {}: {error}", directory.display()))?;
    paths.retain(|path| {
        path.extension()
            .is_some_and(|extension| extension == "toml")
    });
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let name = path
                .file_stem()
                .and_then(|name| name.to_str())
                .ok_or_else(|| format!("workflow source {} has no UTF-8 name", path.display()))?
                .to_string();
            let text = fs::read_to_string(&path)
                .map_err(|error| format!("read workflow source {}: {error}", path.display()))?;
            Ok(source_document(namespace.clone(), &name, Some(path), &text))
        })
        .collect()
}

fn name_collisions(documents: &[SourceDocument]) -> BTreeSet<String> {
    let mut counts = BTreeMap::<String, usize>::new();
    for document in documents {
        *counts.entry(document.identity.name.clone()).or_default() += 1;
    }
    counts
        .into_iter()
        .filter_map(|(name, count)| (count > 1).then_some(name))
        .collect()
}

fn builtin_implementations() -> BTreeMap<String, ImplementationDescriptor> {
    let values = [
        descriptor(
            "builtin:command@1",
            PrimitiveClass::Action,
            EffectClass::ReadOnly,
            TargetRequirement::Local,
        ),
        descriptor(
            "builtin:agent@1",
            PrimitiveClass::Action,
            EffectClass::WorkspaceMutation,
            TargetRequirement::Local,
        ),
        descriptor(
            "builtin:gate@1",
            PrimitiveClass::Gate,
            EffectClass::ReadOnly,
            TargetRequirement::Any,
        ),
        descriptor(
            "builtin:approval@1",
            PrimitiveClass::Approval,
            EffectClass::HumanDecision,
            TargetRequirement::Any,
        ),
        descriptor(
            "builtin:wait@1",
            PrimitiveClass::Wait,
            EffectClass::DurableWait,
            TargetRequirement::Any,
        ),
        descriptor(
            "builtin:notification@1",
            PrimitiveClass::Notification,
            EffectClass::BestEffort,
            TargetRequirement::Any,
        ),
        descriptor(
            "builtin:workflow-call@1",
            PrimitiveClass::WorkflowCall,
            EffectClass::ChildRun,
            TargetRequirement::Any,
        ),
        flexible_descriptor(
            "builtin:create-plan@1",
            PrimitiveClass::Action,
            EffectClass::ReadOnly,
            TargetRequirement::Any,
            &[Capability::RepositoryRead, Capability::ProcessExecute],
        ),
        flexible_descriptor(
            "builtin:review-plan@1",
            PrimitiveClass::Action,
            EffectClass::ReadOnly,
            TargetRequirement::Any,
            &[Capability::RepositoryRead, Capability::ProcessExecute],
        ),
        flexible_descriptor(
            "builtin:approve-plan@1",
            PrimitiveClass::Approval,
            EffectClass::HumanDecision,
            TargetRequirement::Any,
            &[],
        ),
        flexible_descriptor(
            "builtin:implement-plan-phase@1",
            PrimitiveClass::Action,
            EffectClass::WorkspaceMutation,
            TargetRequirement::Local,
            &[
                Capability::RepositoryRead,
                Capability::WorkspaceWrite,
                Capability::ProcessExecute,
            ],
        ),
        flexible_descriptor(
            "builtin:promote-workspace@1",
            PrimitiveClass::Action,
            EffectClass::Brokered,
            TargetRequirement::Local,
            &[Capability::GitCommit],
        ),
        flexible_descriptor(
            "builtin:implement@1",
            PrimitiveClass::Action,
            EffectClass::WorkspaceMutation,
            TargetRequirement::Local,
            &[
                Capability::RepositoryRead,
                Capability::WorkspaceWrite,
                Capability::ProcessExecute,
            ],
        ),
        flexible_descriptor(
            "builtin:self-review@1",
            PrimitiveClass::Action,
            EffectClass::ReadOnly,
            TargetRequirement::Local,
            &[Capability::RepositoryRead, Capability::ProcessExecute],
        ),
        flexible_descriptor(
            "builtin:distinct-model-review@1",
            PrimitiveClass::Action,
            EffectClass::ReadOnly,
            TargetRequirement::Local,
            &[Capability::RepositoryRead, Capability::ProcessExecute],
        ),
        flexible_descriptor(
            "builtin:local-verification@1",
            PrimitiveClass::Gate,
            EffectClass::ReadOnly,
            TargetRequirement::Local,
            &[Capability::RepositoryRead, Capability::ProcessExecute],
        ),
        flexible_descriptor(
            "builtin:review-policy@1",
            PrimitiveClass::Gate,
            EffectClass::ReadOnly,
            TargetRequirement::Any,
            &[],
        ),
        flexible_descriptor(
            "builtin:commit@1",
            PrimitiveClass::Action,
            EffectClass::Brokered,
            TargetRequirement::Local,
            &[Capability::GitCommit],
        ),
        flexible_descriptor(
            "builtin:create-change-request@1",
            PrimitiveClass::Action,
            EffectClass::Brokered,
            TargetRequirement::Local,
            &[Capability::ProviderWrite, Capability::GitPush],
        ),
        flexible_descriptor(
            "builtin:ci@1",
            PrimitiveClass::Gate,
            EffectClass::ReadOnly,
            TargetRequirement::Any,
            &[Capability::ProviderRead],
        ),
        flexible_descriptor(
            "builtin:provider-review@1",
            PrimitiveClass::Gate,
            EffectClass::ReadOnly,
            TargetRequirement::Any,
            &[Capability::ProviderRead],
        ),
        flexible_descriptor(
            "builtin:policy@1",
            PrimitiveClass::Gate,
            EffectClass::ReadOnly,
            TargetRequirement::Any,
            &[Capability::ProviderRead],
        ),
        flexible_descriptor(
            "builtin:mergeability@1",
            PrimitiveClass::Gate,
            EffectClass::ReadOnly,
            TargetRequirement::Any,
            &[Capability::ProviderRead],
        ),
        flexible_descriptor(
            "builtin:repair@1",
            PrimitiveClass::Action,
            EffectClass::WorkspaceMutation,
            TargetRequirement::Local,
            &[
                Capability::RepositoryRead,
                Capability::WorkspaceWrite,
                Capability::ProcessExecute,
                Capability::GitPush,
            ],
        ),
        flexible_descriptor(
            "builtin:human-test@1",
            PrimitiveClass::Approval,
            EffectClass::HumanDecision,
            TargetRequirement::Any,
            &[],
        ),
        flexible_descriptor(
            "builtin:exact-mutation-approval@1",
            PrimitiveClass::Approval,
            EffectClass::HumanDecision,
            TargetRequirement::Any,
            &[],
        ),
        flexible_descriptor(
            "builtin:merge@1",
            PrimitiveClass::Action,
            EffectClass::Brokered,
            TargetRequirement::Local,
            &[Capability::Merge],
        ),
        flexible_descriptor(
            "builtin:cleanup@1",
            PrimitiveClass::Action,
            EffectClass::Brokered,
            TargetRequirement::Local,
            &[Capability::WorktrunkLifecycle],
        ),
        flexible_descriptor(
            "builtin:classify-provider-item@1",
            PrimitiveClass::Action,
            EffectClass::ReadOnly,
            TargetRequirement::Any,
            &[Capability::ProviderRead, Capability::ProcessExecute],
        ),
        flexible_descriptor(
            "builtin:admit-provider-item@1",
            PrimitiveClass::Approval,
            EffectClass::HumanDecision,
            TargetRequirement::Any,
            &[],
        ),
    ];
    values
        .into_iter()
        .map(|value| (value.id.clone(), value))
        .collect()
}

fn flexible_descriptor(
    id: &str,
    class: PrimitiveClass,
    effect: EffectClass,
    target: TargetRequirement,
    capabilities: &[Capability],
) -> ImplementationDescriptor {
    ImplementationDescriptor {
        id: id.to_string(),
        revision: 1,
        class,
        capabilities: capabilities.iter().cloned().collect(),
        inputs: BTreeMap::new(),
        outputs: BTreeMap::new(),
        effect,
        target,
    }
}

fn descriptor(
    id: &str,
    class: PrimitiveClass,
    effect: EffectClass,
    target: TargetRequirement,
) -> ImplementationDescriptor {
    let (capabilities, inputs, outputs) = match id {
        "builtin:command@1" => (
            BTreeSet::from([Capability::ProcessExecute]),
            BTreeMap::from([("task".to_string(), "builtin:task@1".to_string())]),
            BTreeMap::from([("result".to_string(), "builtin:task@1".to_string())]),
        ),
        "builtin:agent@1" => (
            BTreeSet::from([Capability::WorkspaceWrite, Capability::ProcessExecute]),
            BTreeMap::from([("task".to_string(), "builtin:task@1".to_string())]),
            BTreeMap::from([("result".to_string(), "builtin:task@1".to_string())]),
        ),
        "builtin:approval@1" => (
            BTreeSet::new(),
            BTreeMap::from([("task".to_string(), "builtin:task@1".to_string())]),
            BTreeMap::from([(
                "decision".to_string(),
                "builtin:human-attestation@1".to_string(),
            )]),
        ),
        _ => (BTreeSet::new(), BTreeMap::new(), BTreeMap::new()),
    };
    ImplementationDescriptor {
        id: id.to_string(),
        revision: 1,
        class,
        capabilities,
        inputs,
        outputs,
        effect,
        target,
    }
}

fn validate_implementation_ports(
    source: &SourceIdentity,
    step: &RawStep,
    descriptor: &ImplementationDescriptor,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (name, artifact_type) in &descriptor.inputs {
        if step.inputs.get(name).map(|port| &port.artifact_type) != Some(artifact_type) {
            diagnostics.push(diagnostic(
                source,
                "implementation_input_mismatch",
                format!(
                    "Step '{}' must bind input '{name}' as '{artifact_type}'",
                    step.id
                ),
                Some(&step.id),
            ));
        }
    }
    for (name, artifact_type) in &descriptor.outputs {
        if step.outputs.get(name).map(|port| &port.artifact_type) != Some(artifact_type) {
            diagnostics.push(diagnostic(
                source,
                "implementation_output_mismatch",
                format!(
                    "Step '{}' must declare output '{name}' as '{artifact_type}'",
                    step.id
                ),
                Some(&step.id),
            ));
        }
    }
}

fn validate_identifier(
    source: &SourceIdentity,
    value: &str,
    subject: &str,
    step: Option<&str>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let valid = !value.is_empty()
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        });
    if !valid {
        diagnostics.push(diagnostic(
            source,
            "invalid_identifier",
            format!("{subject} ID '{value}' must use lowercase letters, digits, '_' or '-'"),
            step,
        ));
    }
}

fn validate_graph(
    source: &SourceIdentity,
    steps: &[CompiledStep],
    diagnostics: &mut Vec<Diagnostic>,
) {
    let ids = steps
        .iter()
        .map(|step| step.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut indegree = steps
        .iter()
        .map(|step| (step.id.as_str(), 0usize))
        .collect::<BTreeMap<_, _>>();
    let mut outgoing = BTreeMap::<&str, Vec<&str>>::new();
    for step in steps {
        for dependency in &step.dependencies {
            if dependency == &step.id {
                diagnostics.push(diagnostic(
                    source,
                    "graph_cycle",
                    format!("Step '{}' depends on itself", step.id),
                    Some(&step.id),
                ));
            } else if !ids.contains(dependency.as_str()) {
                diagnostics.push(diagnostic(
                    source,
                    "missing_dependency",
                    format!("Step '{}' depends on missing Step '{dependency}'", step.id),
                    Some(&step.id),
                ));
            } else {
                *indegree.get_mut(step.id.as_str()).expect("known Step") += 1;
                outgoing.entry(dependency).or_default().push(&step.id);
            }
        }
    }
    if diagnostics
        .iter()
        .any(|value| value.code == "missing_dependency" || value.code == "graph_cycle")
    {
        return;
    }
    let mut queue = indegree
        .iter()
        .filter_map(|(id, degree)| (*degree == 0).then_some(*id))
        .collect::<VecDeque<_>>();
    let mut visited = 0;
    while let Some(id) = queue.pop_front() {
        visited += 1;
        for child in outgoing.get(id).into_iter().flatten() {
            let degree = indegree.get_mut(child).expect("known child");
            *degree -= 1;
            if *degree == 0 {
                queue.push_back(child);
            }
        }
    }
    if visited != steps.len() {
        diagnostics.push(diagnostic(
            source,
            "graph_cycle",
            "workflow graph contains a dependency cycle".to_string(),
            None,
        ));
    }
}

fn validate_bindings(
    source: &SourceIdentity,
    run_inputs: &BTreeMap<String, Port>,
    steps: &[CompiledStep],
    diagnostics: &mut Vec<Diagnostic>,
) {
    let outputs = steps
        .iter()
        .map(|step| (step.id.as_str(), &step.outputs))
        .collect::<BTreeMap<_, _>>();
    for step in steps {
        for binding in step.inputs.values() {
            let Some((owner, port)) = binding.from.split_once('.') else {
                diagnostics.push(diagnostic(
                    source,
                    "invalid_binding",
                    format!(
                        "binding '{}' must be run.<port> or <step>.<port>",
                        binding.from
                    ),
                    Some(&step.id),
                ));
                continue;
            };
            let produced = if owner == "run" {
                run_inputs.get(port)
            } else {
                outputs.get(owner).and_then(|ports| ports.get(port))
            };
            match produced {
                None => diagnostics.push(diagnostic(
                    source,
                    "missing_binding",
                    format!("binding '{}' does not exist", binding.from),
                    Some(&step.id),
                )),
                Some(produced) if produced.artifact_type != binding.artifact_type => diagnostics
                    .push(diagnostic(
                        source,
                        "type_mismatch",
                        format!(
                            "binding '{}' has type '{}', not '{}'",
                            binding.from, produced.artifact_type, binding.artifact_type
                        ),
                        Some(&step.id),
                    )),
                _ => {}
            }
        }
    }
}

fn validate_conditions(
    source: &SourceIdentity,
    run_inputs: &BTreeMap<String, Port>,
    steps: &[CompiledStep],
    diagnostics: &mut Vec<Diagnostic>,
) {
    let step_ids = steps
        .iter()
        .map(|step| step.id.as_str())
        .collect::<BTreeSet<_>>();
    for step in steps {
        let Some(condition) = &step.condition else {
            continue;
        };
        let mut references = Vec::new();
        condition_references(condition, &mut references);
        for reference in references {
            let valid = reference
                .strip_prefix("run.")
                .and_then(|value| value.split('.').next())
                .is_some_and(|port| run_inputs.contains_key(port))
                || reference.strip_prefix("step.").is_some_and(|value| {
                    let mut parts = value.split('.');
                    let Some(id) = parts.next() else {
                        return false;
                    };
                    let Some(port) = parts.next() else {
                        return false;
                    };
                    if port == "outcome" && parts.next().is_none() {
                        return step_ids.contains(id);
                    }
                    steps
                        .iter()
                        .find(|step| step.id == id)
                        .is_some_and(|step| step.outputs.contains_key(port))
                });
            if !valid {
                diagnostics.push(diagnostic(
                    source,
                    "unknown_condition_reference",
                    format!(
                        "Step '{}' condition references unknown typed value '{reference}'",
                        step.id
                    ),
                    Some(&step.id),
                ));
            }
        }
    }
}

fn condition_references<'a>(condition: &'a ConditionExpr, output: &mut Vec<&'a str>) {
    match condition {
        ConditionExpr::Literal(_) => {}
        ConditionExpr::Reference(value) => output.push(value),
        ConditionExpr::Not(value) => condition_references(value, output),
        ConditionExpr::All(values) | ConditionExpr::Any(values) => {
            for value in values {
                condition_references(value, output);
            }
        }
        ConditionExpr::Equal { left, right } | ConditionExpr::NotEqual { left, right } => {
            for value in [left.as_str(), right.as_str()] {
                if !matches!(value, "true" | "false") {
                    output.push(value);
                }
            }
        }
    }
}

fn diagnostic(
    source: &SourceIdentity,
    code: &str,
    message: String,
    step: Option<&str>,
) -> Diagnostic {
    Diagnostic {
        code: code.to_string(),
        message,
        source: source.qualified_name(),
        path: source.path.clone(),
        line: None,
        column: None,
        step: step.map(str::to_string),
    }
}

fn parse_condition(text: &str) -> Result<ConditionExpr, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("condition cannot be empty".to_string());
    }
    if let Some(inner) = strip_outer_parentheses(text) {
        return parse_condition(inner);
    }
    if let Some(parts) = split_operator(text, "||") {
        return parts
            .into_iter()
            .map(parse_condition)
            .collect::<Result<Vec<_>, _>>()
            .map(ConditionExpr::Any);
    }
    if let Some(parts) = split_operator(text, "&&") {
        return parts
            .into_iter()
            .map(parse_condition)
            .collect::<Result<Vec<_>, _>>()
            .map(ConditionExpr::All);
    }
    if let Some(value) = text.strip_prefix('!') {
        return Ok(ConditionExpr::Not(Box::new(parse_condition(value)?)));
    }
    for (operator, invert) in [("==", false), ("!=", true)] {
        if let Some(parts) = split_operator(text, operator) {
            if parts.len() != 2 {
                return Err(format!(
                    "condition operator '{operator}' must have two operands"
                ));
            }
            let left = condition_operand(parts[0])?;
            let right = condition_operand(parts[1])?;
            return Ok(if invert {
                ConditionExpr::NotEqual { left, right }
            } else {
                ConditionExpr::Equal { left, right }
            });
        }
    }
    match text {
        "true" => Ok(ConditionExpr::Literal(true)),
        "false" => Ok(ConditionExpr::Literal(false)),
        value => Ok(ConditionExpr::Reference(condition_operand(value)?)),
    }
}

fn split_operator<'a>(text: &'a str, operator: &str) -> Option<Vec<&'a str>> {
    let bytes = text.as_bytes();
    let operator = operator.as_bytes();
    let mut depth = 0i32;
    let mut start = 0;
    let mut parts = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
        if depth == 0 && bytes[index..].starts_with(operator) {
            parts.push(&text[start..index]);
            index += operator.len();
            start = index;
            continue;
        }
        index += 1;
    }
    if depth != 0 {
        return None;
    }
    if !parts.is_empty() {
        parts.push(&text[start..]);
    }
    (parts.len() > 1).then_some(parts)
}

fn strip_outer_parentheses(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'(') || bytes.last() != Some(&b')') {
        return None;
    }
    let mut depth = 0i32;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
        if depth == 0 && index + 1 != bytes.len() {
            return None;
        }
    }
    (depth == 0).then_some(&text[1..text.len() - 1])
}

fn condition_operand(value: &str) -> Result<String, String> {
    let value = value.trim();
    if matches!(value, "true" | "false") || value.starts_with("run.") || value.starts_with("step.")
    {
        Ok(value.to_string())
    } else {
        Err(format!("invalid condition operand '{value}'"))
    }
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("write to String");
    }
    output
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn builtins_compile_through_catalog() {
        let catalog = DefinitionCatalog::at(PathBuf::from("/missing"), None);
        let list = catalog.list().unwrap();
        assert_eq!(list.len(), 9);
        assert!(list.iter().all(|definition| definition.valid));
        assert!(catalog.resolve("builtin:approval").is_ok());
    }

    #[test]
    fn workflow_source_resolves_a_named_command_implementation() {
        let implementation = "[[implementations]]\nid = \"global:echo@1\"\nadapter = \"command\"\nclass = \"action\"\neffect = \"read_only\"\ntarget = \"local\"\ncapabilities = [\"process_execute\"]\n[implementations.inputs.task]\nartifact_type = \"builtin:task@1\"\n[implementations.outputs.result]\nartifact_type = \"builtin:task@1\"\n[implementations.settings]\ncommand = [\"printf\", \"named\"]\n\n";
        let source = WORKFLOW_EXAMPLE
            .replace("[[steps]]", &format!("{implementation}[[steps]]"))
            .replace("builtin:command@1", "global:echo@1")
            .replace(
                "[steps.settings]\ncommand = [\"printf\", \"%s\\n\", \"hello\"]\n",
                "",
            );
        let snapshot = DefinitionCatalog::at(PathBuf::from("/missing"), None)
            .compile(&source_document(
                SourceNamespace::Global,
                "action",
                None,
                &source,
            ))
            .unwrap();

        assert_eq!(snapshot.content.steps[0].implementation, "global:echo@1");
        assert_eq!(
            snapshot.content.steps[0].settings.command,
            ["printf", "named"]
        );
    }

    #[test]
    fn workflow_source_snapshots_durable_trigger_definitions() {
        let source = format!(
            "{}\n[[triggers]]\nid = \"nightly\"\ndefinition_selector = \"global:action\"\nadmission_purpose = \"scheduled\"\nkind = \"schedule\"\nexpression = \"0 0 9 * * * *\"\ntimezone = \"UTC\"\nmissed = \"latest\"\noverlap = \"coalesce\"\nmax_fan_out = 1\n",
            WORKFLOW_EXAMPLE
        );
        let document = source_document(SourceNamespace::Global, "action", None, &source);
        let snapshot = DefinitionCatalog::at(PathBuf::from("/missing"), None)
            .compile(&document)
            .unwrap();

        assert_eq!(snapshot.content.triggers.len(), 1);
        assert_eq!(snapshot.content.triggers[0].id, "global:action/nightly");
        assert!(matches!(
            snapshot.content.triggers[0].kind,
            crate::trigger::TriggerKind::Schedule { .. }
        ));
    }

    #[test]
    fn ordered_steps_expand_to_explicit_edges() {
        let source = WORKFLOW_EXAMPLE.replace("description =", "ordered = true\ndescription =").replace("[[steps]]\nid = \"act\"", "[[steps]]\nid = \"first\"\nclass = \"gate\"\nimplementation = \"builtin:gate@1\"\n\n[[steps]]\nid = \"act\"");
        let document = source_document(SourceNamespace::Global, "action", None, &source);
        let catalog = DefinitionCatalog::at(PathBuf::from("/missing"), None);
        let snapshot = catalog.compile(&document).unwrap();
        assert_eq!(snapshot.content.steps[1].dependencies, ["first"]);
    }

    #[test]
    fn cycles_and_type_mismatches_are_rejected() {
        let source = WORKFLOW_EXAMPLE
            .replace(
                "implementation = \"builtin:command@1\"",
                "implementation = \"builtin:command@1\"\ndepends_on = [\"act\"]",
            )
            .replace(
                "artifact_type = \"builtin:task@1\"\n\n[steps.outputs.result]",
                "artifact_type = \"other:type@1\"\n\n[steps.outputs.result]",
            );
        let document = source_document(SourceNamespace::Global, "action", None, &source);
        let errors = DefinitionCatalog::at(PathBuf::from("/missing"), None)
            .compile(&document)
            .unwrap_err();
        assert!(errors.iter().any(|error| error.code == "graph_cycle"));
        assert!(errors.iter().any(|error| error.code == "type_mismatch"));
    }

    #[test]
    fn canonical_digest_is_stable_across_map_ordering() {
        let left = source_document(SourceNamespace::Global, "action", None, WORKFLOW_EXAMPLE);
        let reordered = WORKFLOW_EXAMPLE.replace(
            "max_attempts = 3\nmax_fan_out = 1",
            "max_fan_out = 1\nmax_attempts = 3",
        );
        let right = source_document(SourceNamespace::Global, "action", None, &reordered);
        let catalog = DefinitionCatalog::at(PathBuf::from("/missing"), None);
        let left = catalog.compile(&left).unwrap();
        let right = catalog.compile(&right).unwrap();
        assert_eq!(left.digest, right.digest);
        assert_eq!(left.canonical_bytes, right.canonical_bytes);
    }

    #[test]
    fn source_edit_changes_new_snapshot_without_mutating_old_snapshot() {
        let catalog = DefinitionCatalog::at(PathBuf::from("/missing"), None);
        let original = catalog
            .compile(&source_document(
                SourceNamespace::Global,
                "action",
                None,
                WORKFLOW_EXAMPLE,
            ))
            .unwrap();
        let edited_text = WORKFLOW_EXAMPLE.replace("Run one typed", "Run exactly one typed");
        let edited = catalog
            .compile(&source_document(
                SourceNamespace::Global,
                "action",
                None,
                &edited_text,
            ))
            .unwrap();
        assert_ne!(original.digest, edited.digest);
        assert!(
            String::from_utf8(original.canonical_bytes)
                .unwrap()
                .contains("Run one typed")
        );
    }

    #[test]
    fn condition_quality_is_not_collapsed_to_false() {
        let condition = parse_condition("run.ready && step.observe.outcome").unwrap();
        let values = BTreeMap::from([
            ("run.ready".to_string(), ObservedBool::Value(true)),
            ("step.observe.outcome".to_string(), ObservedBool::Stale),
        ]);
        assert_eq!(condition.evaluate(&values), ObservedBool::Stale);
    }

    #[test]
    fn compiled_condition_round_trips_without_reparsing_source_text() {
        let condition = parse_condition("!(run.ready != true) || step.observe.outcome").unwrap();
        let persisted = serde_json::to_string(&condition).unwrap();
        let restored: ConditionExpr = serde_json::from_str(&persisted).unwrap();
        assert_eq!(restored, condition);
    }

    #[test]
    fn repository_and_global_collision_requires_qualification() {
        let root = std::env::temp_dir().join(format!(
            "prism-definition-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let global = root.join("global");
        let repository = root.join("repository");
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(&repository).unwrap();
        fs::write(global.join("action.toml"), WORKFLOW_EXAMPLE).unwrap();
        fs::write(repository.join("action.toml"), WORKFLOW_EXAMPLE).unwrap();
        let catalog = DefinitionCatalog::at(global, Some(repository));
        assert!(
            catalog.resolve("action").unwrap_err()[0]
                .message
                .contains("ambiguous")
        );
        assert!(catalog.resolve("global:action").is_ok());
        fs::remove_dir_all(root).unwrap();
    }
}
