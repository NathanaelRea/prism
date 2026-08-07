use std::collections::HashMap;
use std::future::Future;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;

use crate::persistence::artifacts::{ArtifactBody, ArtifactStore, PublishArtifact};
use crate::persistence::control_plane::{
    AsyncCoordinator, CapacityRequirement, DurableClaim, OutputChunk, OutputStream,
};
use crate::persistence::effects::{EffectBroker, PrepareEffect};
use crate::persistence::pools::WorkflowDatabase;
use crate::persistence::run_ledger::{
    AttemptLease, AttemptResult, Coordinator, RunLedger, StartRun,
};
use crate::persistence::triggers::TriggerStore;
use crate::persistence::wakeups::WakeupStore;
use crate::workflow::trigger::ProviderPollAdapter as _;

static ATTEMPT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub type StepFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
pub type TargetFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
pub type ReconciliationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ReconciliationResult, String>> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectReconciliation {
    pub id: String,
    pub kind: String,
    pub idempotency_key: String,
    pub request_json: String,
    pub previous_result_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationResult {
    pub succeeded: bool,
    pub result_json: String,
}

/// Provider adapter seam used only after an intent has become indeterminate. Implementations
/// inspect authoritative provider state using the persisted idempotency key; they must not
/// blindly dispatch the original effect again.
pub trait EffectReconciler: Send + Sync + 'static {
    fn reconcile<'a>(&'a self, intent: EffectReconciliation) -> ReconciliationFuture<'a>;
}

/// Async workflow implementation seam. Implementations receive cancellation and a bounded
/// output sender; they never receive a database connection or transaction.
pub(crate) trait StepImplementation: Send + Sync + 'static {
    fn execute<'a>(&'a self, context: ExecutionContext) -> StepFuture<'a>;
}

/// Execution target seam for local, remote, container, or future hosted execution. Targets own
/// transport and target-local admission; implementations remain unaware of where they run.
pub(crate) trait ExecutionTarget: Send + Sync + 'static {
    fn execute<'a>(
        &'a self,
        implementation: Arc<dyn StepImplementation>,
        context: ExecutionContext,
    ) -> TargetFuture<'a>;
}

#[derive(Default)]
pub struct LocalExecutionTarget;

impl ExecutionTarget for LocalExecutionTarget {
    fn execute<'a>(
        &'a self,
        implementation: Arc<dyn StepImplementation>,
        context: ExecutionContext,
    ) -> TargetFuture<'a> {
        Box::pin(async move { implementation.execute(context).await })
    }
}

fn default_targets() -> HashMap<String, Arc<dyn ExecutionTarget>> {
    HashMap::from([(
        "local".into(),
        Arc::new(LocalExecutionTarget) as Arc<dyn ExecutionTarget>,
    )])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionClass {
    Agent,
    Command,
    Provider,
}

impl ExecutionClass {
    fn label(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Command => "command",
            Self::Provider => "provider",
        }
    }
}

#[derive(Clone)]
struct RegisteredImplementation {
    class: ExecutionClass,
    implementation: Arc<dyn StepImplementation>,
}

#[derive(Clone)]
struct ExtensionImplementation {
    id: String,
    supervisor: Arc<crate::extension::ExtensionSupervisor>,
}

impl StepImplementation for ExtensionImplementation {
    fn execute<'a>(&'a self, context: ExecutionContext) -> StepFuture<'a> {
        Box::pin(async move {
            let input = serde_json::from_str(context.input_json())
                .map_err(|error| format!("invalid extension input: {error}"))?;
            let artifacts =
                serde_json::from_str::<serde_json::Value>(&context.input_revisions_json)
                    .map_err(|error| format!("invalid extension Artifact bindings: {error}"))?
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(name, value)| {
                        let revision = value
                            .get("revision")
                            .and_then(serde_json::Value::as_u64)
                            .ok_or_else(|| format!("Artifact binding '{name}' has no revision"))?;
                        Ok((
                            name.clone(),
                            prism_extension_protocol::ArtifactReference {
                                id: value
                                    .get("artifact_id")
                                    .and_then(serde_json::Value::as_str)
                                    .ok_or_else(|| format!("Artifact binding '{name}' has no ID"))?
                                    .into(),
                                revision,
                                schema: value
                                    .get("schema")
                                    .and_then(serde_json::Value::as_str)
                                    .ok_or_else(|| {
                                        format!("Artifact binding '{name}' has no schema")
                                    })?
                                    .into(),
                                digest: value
                                    .get("digest")
                                    .and_then(serde_json::Value::as_str)
                                    .ok_or_else(|| {
                                        format!("Artifact binding '{name}' has no digest")
                                    })?
                                    .into(),
                            },
                        ))
                    })
                    .collect::<Result<_, String>>()?;
            let result = self
                .supervisor
                .execute(
                    prism_extension_protocol::AttemptEnvelope {
                        attempt_id: context.run_attempt_id.clone(),
                        generation: u64::try_from(context.lease.fencing_token)
                            .map_err(|_| "invalid Attempt fencing token".to_string())?,
                        implementation_id: self.id.clone(),
                        input,
                        artifacts,
                    },
                    context.cancellation(),
                )
                .await
                .map_err(|error| error.to_string())?;
            match result.outcome {
                prism_extension_protocol::AttemptOutcome::Succeeded { outputs } => {
                    Ok(outputs.to_string())
                }
                prism_extension_protocol::AttemptOutcome::Failed { error } => Err(error),
                prism_extension_protocol::AttemptOutcome::Cancelled => {
                    Err("extension attempt cancelled".into())
                }
            }
        })
    }
}

#[derive(Default)]
pub(crate) struct CommandImplementation;

#[derive(Deserialize, Serialize)]
struct CommandInput {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    #[serde(default)]
    environment: HashMap<String, String>,
    #[serde(default)]
    stdin: Option<String>,
}

#[derive(Default)]
pub(crate) struct HarnessImplementation;

#[derive(Deserialize)]
struct HarnessInput {
    repository: String,
    cwd: String,
    harness_id: String,
    prompt: String,
    title: String,
    variant: Option<String>,
}

impl StepImplementation for HarnessImplementation {
    fn execute<'a>(&'a self, mut context: ExecutionContext) -> StepFuture<'a> {
        Box::pin(async move {
            let input: HarnessInput = serde_json::from_str(context.input_json())
                .map_err(|error| format!("invalid harness step input: {error}"))?;
            let repository = crate::repo::Repository {
                root: input.repository.into(),
            };
            let config = crate::config::Config::load(&repository);
            if !config.config_errors.is_empty() {
                return Err(format!(
                    "invalid repository configuration: {}",
                    config.config_errors.join("; ")
                ));
            }
            let harness_config = config.harness_config(&input.harness_id)?;
            let invocation = crate::harness::Harness::new(&input.harness_id, &harness_config)
                .headless(
                    &input.prompt,
                    std::path::Path::new(&input.cwd),
                    &input.title,
                    None,
                    input.variant.as_deref(),
                    false,
                )?;
            let (program, args) = invocation
                .argv
                .split_first()
                .ok_or_else(|| "harness invocation is empty".to_string())?;
            context.input_json = serde_json::to_string(&CommandInput {
                program: program.clone(),
                args: args.to_vec(),
                cwd: Some(input.cwd),
                environment: invocation.environment.clone().into_iter().collect(),
                stdin: invocation.stdin.clone(),
            })
            .map_err(|error| format!("serialize harness invocation: {error}"))?;
            let result = CommandImplementation.execute(context).await;
            invocation.cleanup();
            result
        })
    }
}

impl StepImplementation for CommandImplementation {
    fn execute<'a>(&'a self, context: ExecutionContext) -> StepFuture<'a> {
        Box::pin(async move {
            let input: CommandInput = serde_json::from_str(context.input_json())
                .map_err(|error| format!("invalid command step input: {error}"))?;
            if context.is_cancelled() {
                return Err("command cancelled".into());
            }
            if input.program.trim().is_empty() {
                return Err("command program must not be empty".into());
            }
            let mut command = Command::new(&input.program);
            command.as_std_mut().process_group(0);
            command
                .args(&input.args)
                .envs(&input.environment)
                .kill_on_drop(true)
                .stdin(if input.stdin.is_some() {
                    std::process::Stdio::piped()
                } else {
                    std::process::Stdio::null()
                })
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            if let Some(cwd) = input.cwd {
                command.current_dir(cwd);
            }
            let mut child = command
                .spawn()
                .map_err(|error| format!("spawn command '{}': {error}", input.program))?;
            if let Some(stdin) = input.stdin
                && let Some(mut writer) = child.stdin.take()
            {
                writer
                    .write_all(stdin.as_bytes())
                    .await
                    .map_err(|error| format!("write command stdin: {error}"))?;
            }
            let process_id = child
                .id()
                .ok_or_else(|| format!("command '{}' has no process identity", input.program))?;
            let recorded =
                tokio::task::spawn_blocking(move || crate::process::record_process(process_id))
                    .await
                    .map_err(|error| format!("join process identity observation: {error}"))?
                    .map_err(|error| error.to_string())?;
            context
                .record_process(
                    recorded.pid,
                    recorded.identity.map(|identity| identity.stored_value()),
                )
                .await
                .map_err(|error| error.to_string())?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| "capture command stdout".to_string())?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| "capture command stderr".to_string())?;
            let stdout_task = tokio::spawn(pump_command_output(stdout, context.clone(), false));
            let stderr_task = tokio::spawn(pump_command_output(stderr, context.clone(), true));
            let mut cancellation = context.cancellation();
            let status = tokio::select! {
                status = child.wait() => status.map_err(|error| format!("wait for command '{}': {error}", input.program))?,
                changed = cancellation.changed() => {
                    if changed.is_err() || *cancellation.borrow() {
                        let termination = tokio::task::spawn_blocking(move || {
                            crate::process::terminate_recorded_process(recorded, Duration::from_secs(2))
                        })
                        .await
                        .map_err(|error| format!("join command termination: {error}"))?;
                        if let Err(error) = termination {
                            let _ = child.kill().await;
                            return Err(format!("cancel command '{}': {error}", input.program));
                        }
                        let _ = child.wait().await;
                        return Err("command cancelled".into());
                    }
                    child.wait().await.map_err(|error| format!("wait for command '{}': {error}", input.program))?
                }
            };
            stdout_task
                .await
                .map_err(|error| format!("join stdout reader: {error}"))??;
            stderr_task
                .await
                .map_err(|error| format!("join stderr reader: {error}"))??;
            if status.success() {
                Ok(serde_json::json!({"exit_code": status.code()}).to_string())
            } else {
                Err(format!("command exited with status {status}"))
            }
        })
    }
}

async fn pump_command_output(
    mut reader: impl AsyncRead + Unpin,
    context: ExecutionContext,
    stderr: bool,
) -> Result<(), String> {
    let mut buffer = vec![0_u8; 8192];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|error| format!("read command output: {error}"))?;
        if count == 0 {
            return Ok(());
        }
        let result = if stderr {
            context.stderr(buffer[..count].to_vec()).await
        } else {
            context.stdout(buffer[..count].to_vec()).await
        };
        result.map_err(|error| error.to_string())?;
    }
}

pub enum ArtifactContent<'a> {
    Inline(&'a [u8]),
    ContentAddressedFile { path: &'a str, size_bytes: u64 },
}

pub struct ArtifactPublication<'a> {
    pub id: &'a str,
    pub revision: i64,
    pub digest: &'a str,
    pub sensitivity: &'a str,
    pub content: ArtifactContent<'a>,
    pub parents: &'a [String],
}

#[derive(Clone)]
pub struct ExecutionContext {
    pub run_attempt_id: String,
    pub step_id: String,
    input_json: String,
    input_revisions_json: String,
    output: mpsc::Sender<OutputMessage>,
    cancellation: watch::Receiver<bool>,
    control: AsyncCoordinator,
    effects: EffectBroker,
    artifacts: ArtifactStore,
    lease: AttemptLease,
}

impl ExecutionContext {
    pub async fn stdout(&self, body: impl Into<Vec<u8>>) -> Result<(), WorkerError> {
        self.send(OutputStream::Stdout, body.into()).await
    }

    pub async fn stderr(&self, body: impl Into<Vec<u8>>) -> Result<(), WorkerError> {
        self.send(OutputStream::Stderr, body.into()).await
    }

    pub async fn system(&self, body: impl Into<Vec<u8>>) -> Result<(), WorkerError> {
        self.send(OutputStream::System, body.into()).await
    }

    pub fn input_json(&self) -> &str {
        &self.input_json
    }

    pub fn cancellation(&self) -> watch::Receiver<bool> {
        self.cancellation.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        *self.cancellation.borrow()
    }

    async fn record_process(
        &self,
        process_id: u32,
        process_start_time_ticks: Option<u64>,
    ) -> Result<(), WorkerError> {
        self.control
            .record_process(&self.lease, process_id, process_start_time_ticks, unix_ms())
            .await
            .map_err(Into::into)
    }

    pub async fn publish_artifact(
        &self,
        publication: ArtifactPublication<'_>,
    ) -> Result<(), WorkerError> {
        let (body, size_bytes) = match publication.content {
            ArtifactContent::Inline(body) => (ArtifactBody::Inline(body), body.len() as u64),
            ArtifactContent::ContentAddressedFile { path, size_bytes } => {
                (ArtifactBody::ContentAddressedFile(path), size_bytes)
            }
        };
        self.artifacts
            .publish(PublishArtifact {
                id: publication.id,
                lease: &self.lease,
                revision: publication.revision,
                digest: publication.digest,
                size_bytes,
                sensitivity: publication.sensitivity,
                body,
                parents: publication.parents,
                now_unix_ms: unix_ms(),
            })
            .await
            .map_err(Into::into)
    }

    pub async fn prepare_effect(
        &self,
        id: &str,
        kind: &str,
        authority_scope: &str,
        idempotency_key: &str,
        request_json: &str,
    ) -> Result<EffectIntent, WorkerError> {
        let effect_id = self
            .effects
            .prepare(PrepareEffect {
                id,
                lease: &self.lease,
                kind,
                authority_scope,
                idempotency_key,
                request_json,
                now_unix_ms: unix_ms(),
            })
            .await?;
        Ok(EffectIntent {
            id: effect_id,
            broker: self.effects.clone(),
            lease: self.lease.clone(),
        })
    }

    async fn send(&self, stream: OutputStream, body: Vec<u8>) -> Result<(), WorkerError> {
        self.output
            .send(OutputMessage::Chunk {
                attempt_id: self.run_attempt_id.clone(),
                chunk: OutputChunk {
                    stream,
                    body,
                    time_unix_ms: unix_ms(),
                },
            })
            .await
            .map_err(|_| WorkerError::Stopped("output aggregator stopped".into()))
    }
}

/// Intent-first handle. Dispatch must happen only after `mark_dispatching` commits.
pub struct EffectIntent {
    id: String,
    broker: EffectBroker,
    lease: AttemptLease,
}

impl EffectIntent {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub async fn mark_dispatching(&self) -> Result<(), WorkerError> {
        self.broker
            .mark_dispatching(&self.id, &self.lease, unix_ms())
            .await
            .map_err(Into::into)
    }

    /// Returns true when the result was authoritative, false when lease loss made the effect
    /// indeterminate and reconciliation is required.
    pub async fn record_result(
        &self,
        succeeded: bool,
        result_json: &str,
    ) -> Result<bool, WorkerError> {
        self.broker
            .record_result(&self.id, &self.lease, succeeded, result_json, unix_ms())
            .await
            .map_err(Into::into)
    }
}

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub dispatch_capacity: usize,
    pub output_capacity: usize,
    pub scheduler_batch: usize,
    pub global_capacity: usize,
    pub agent_capacity: usize,
    pub command_capacity: usize,
    pub provider_capacity: usize,
    pub target_capacity: usize,
    pub repository_capacity: usize,
    pub lease_duration: Duration,
    pub lease_renew_interval: Duration,
    pub scheduler_interval: Duration,
    pub output_flush_interval: Duration,
    pub output_batch_chunks: usize,
    pub output_budget_bytes: usize,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            dispatch_capacity: 32,
            output_capacity: 256,
            scheduler_batch: 32,
            global_capacity: 4,
            agent_capacity: 4,
            command_capacity: 4,
            provider_capacity: 8,
            target_capacity: 4,
            repository_capacity: 4,
            lease_duration: Duration::from_secs(15),
            lease_renew_interval: Duration::from_secs(5),
            scheduler_interval: Duration::from_millis(250),
            output_flush_interval: Duration::from_millis(100),
            output_batch_chunks: 32,
            output_budget_bytes: 4 * 1024 * 1024,
        }
    }
}

/// One supervised async control plane over the global workflow database.
pub struct WorkflowWorker {
    database: WorkflowDatabase,
    worker_id: String,
    config: WorkerConfig,
    implementations: HashMap<String, RegisteredImplementation>,
    targets: HashMap<String, Arc<dyn ExecutionTarget>>,
    reconcilers: HashMap<String, Arc<dyn EffectReconciler>>,
    extension_clients: Vec<Arc<crate::extension::ExtensionSupervisor>>,
    execution: ExecutionControl,
}

impl WorkflowWorker {
    pub async fn open(
        path: &Path,
        worker_id: impl Into<String>,
        config: WorkerConfig,
    ) -> Result<Self, WorkerError> {
        validate_config(&config)?;
        Ok(Self {
            database: WorkflowDatabase::open(path).await?,
            worker_id: worker_id.into(),
            config,
            implementations: HashMap::new(),
            targets: default_targets(),
            reconcilers: HashMap::new(),
            extension_clients: Vec::new(),
            execution: ExecutionControl::new(),
        })
    }

    pub async fn open_default(
        worker_id: impl Into<String>,
        config: WorkerConfig,
    ) -> Result<Self, WorkerError> {
        validate_config(&config)?;
        Ok(Self {
            database: WorkflowDatabase::open_default().await?,
            worker_id: worker_id.into(),
            config,
            implementations: HashMap::new(),
            targets: default_targets(),
            reconcilers: HashMap::new(),
            extension_clients: Vec::new(),
            execution: ExecutionControl::new(),
        })
    }

    pub(crate) fn operations(&self) -> crate::workflow::operations::WorkflowOperations {
        crate::workflow::operations::WorkflowOperations::from_database_with_execution(
            self.database.clone(),
            Some(self.execution.clone()),
        )
    }

    pub fn register_builtins(&mut self) -> Result<(), WorkerError> {
        self.register_as("command", ExecutionClass::Command, CommandImplementation)?;
        self.register_as("harness", ExecutionClass::Agent, HarnessImplementation)
    }

    /// Builds the production intent-first dispatcher for the Standard Extension. The supplied
    /// backend resolves opaque references and performs the provider/Git/Worktrunk operation;
    /// Prism owns durable intent, fencing, and result recording around it.
    pub(crate) fn standard_production_dispatcher(
        &self,
    ) -> Arc<dyn crate::extension::HostDispatcher> {
        let observational = crate::workflow::standard_host::dispatcher(self.database.clone());
        self.standard_host_dispatcher(
            Arc::new(
                crate::workflow::standard_host::StandardProtectedEffects::new(
                    self.database.clone(),
                ),
            ),
            observational,
        )
    }

    pub fn standard_host_dispatcher<B: crate::extension::ProtectedEffectBackend>(
        &self,
        backend: Arc<B>,
        observational: Arc<dyn crate::extension::HostDispatcher>,
    ) -> Arc<dyn crate::extension::HostDispatcher> {
        Arc::new(crate::extension::BrokeredHostDispatcher::new(
            Arc::new(crate::persistence::effects::WorkflowEffectLedger::new(
                self.database.clone(),
            )),
            backend,
            observational,
        ))
    }

    #[cfg(test)]
    pub(crate) fn register(
        &mut self,
        name: impl Into<String>,
        implementation: impl StepImplementation,
    ) -> Result<(), WorkerError> {
        self.register_as(name, ExecutionClass::Agent, implementation)
    }

    pub(crate) fn register_as(
        &mut self,
        name: impl Into<String>,
        class: ExecutionClass,
        implementation: impl StepImplementation,
    ) -> Result<(), WorkerError> {
        let name = name.into();
        if self
            .implementations
            .insert(
                name.clone(),
                RegisteredImplementation {
                    class,
                    implementation: Arc::new(implementation),
                },
            )
            .is_some()
        {
            return Err(WorkerError::Configuration(format!(
                "step implementation '{name}' is already registered"
            )));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn register_target(
        &mut self,
        id: impl Into<String>,
        target: impl ExecutionTarget,
    ) -> Result<(), WorkerError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(WorkerError::Configuration(
                "execution target id must not be empty".into(),
            ));
        }
        if self.targets.insert(id.clone(), Arc::new(target)).is_some() {
            return Err(WorkerError::Configuration(format!(
                "execution target '{id}' is already registered"
            )));
        }
        Ok(())
    }

    pub fn register_reconciler(
        &mut self,
        effect_kind: impl Into<String>,
        reconciler: impl EffectReconciler,
    ) -> Result<(), WorkerError> {
        let effect_kind = effect_kind.into();
        if self
            .reconcilers
            .insert(effect_kind.clone(), Arc::new(reconciler))
            .is_some()
        {
            return Err(WorkerError::Configuration(format!(
                "effect reconciler '{effect_kind}' is already registered"
            )));
        }
        Ok(())
    }

    /// Register every implementation advertised by an executable extension. This process
    /// protocol is the public implementation seam; registrations are applied atomically.
    pub async fn register_extension(
        &mut self,
        executable: impl AsRef<Path>,
        dispatcher: Arc<dyn crate::extension::HostDispatcher>,
        limits: crate::extension::HostLimits,
    ) -> Result<Arc<crate::extension::ExtensionSupervisor>, WorkerError> {
        let supervisor =
            crate::extension::ExtensionSupervisor::launch(executable, dispatcher, limits)
                .await
                .map_err(|error| WorkerError::Configuration(error.to_string()))?;
        let client = supervisor.client().await;
        for descriptor in &client.descriptor().implementations {
            if self.implementations.contains_key(&descriptor.id) {
                let _ = supervisor.shutdown().await;
                return Err(WorkerError::Configuration(format!(
                    "step implementation '{}' is already registered",
                    descriptor.id
                )));
            }
        }
        for descriptor in &client.descriptor().implementations {
            let class = match descriptor.class {
                prism_extension_protocol::StepClass::Action => ExecutionClass::Command,
                prism_extension_protocol::StepClass::WorkflowCall => ExecutionClass::Agent,
                prism_extension_protocol::StepClass::Gate
                | prism_extension_protocol::StepClass::Approval
                | prism_extension_protocol::StepClass::Wait
                | prism_extension_protocol::StepClass::Notification => ExecutionClass::Provider,
            };
            self.implementations.insert(
                descriptor.id.clone(),
                RegisteredImplementation {
                    class,
                    implementation: Arc::new(ExtensionImplementation {
                        id: descriptor.id.clone(),
                        supervisor: supervisor.clone(),
                    }),
                },
            );
        }
        self.extension_clients.push(supervisor.clone());
        Ok(supervisor)
    }

    /// Runs until shutdown is requested or any critical task fails. All task failures are
    /// observed; a critical failure cancels active attempts and drains the worker.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<(), WorkerError> {
        let coordinator = AsyncCoordinator::new(self.database.clone());
        let ledger_coordinator = Coordinator::new(self.database.clone());
        let run_ledger = RunLedger::new(self.database.clone());
        let effect_broker = EffectBroker::new(self.database.clone());
        let artifact_store = ArtifactStore::new(self.database.clone());
        let wakeups = WakeupStore::new(self.database.clone());
        let triggers = TriggerStore::new(self.database.clone());
        triggers.record_startup(&self.worker_id, unix_ms()).await?;
        let active = self.execution.active.clone();
        let registry = ExecutionRegistry {
            implementations: Arc::new(self.implementations),
            targets: Arc::new(self.targets),
        };
        let reconcilers = Arc::new(self.reconcilers);
        let extension_clients = self.extension_clients.clone();
        let (dispatch_tx, dispatch_rx) = mpsc::channel(self.config.dispatch_capacity);
        let (output_tx, output_rx) = mpsc::channel(self.config.output_capacity);
        let (stop_tx, stop_rx) = watch::channel(false);
        let mut tasks = JoinSet::new();

        tasks.spawn(scheduler_task(
            coordinator.clone(),
            self.worker_id.clone(),
            self.config.clone(),
            registry.clone(),
            dispatch_tx,
            stop_rx.clone(),
        ));
        tasks.spawn(execution_task(
            ledger_coordinator,
            registry,
            active.clone(),
            dispatch_rx,
            output_tx,
            ExecutionStores {
                control: coordinator.clone(),
                effects: effect_broker.clone(),
                artifacts: artifact_store,
            },
            stop_rx.clone(),
        ));
        tasks.spawn(lease_task(
            coordinator.clone(),
            active.clone(),
            self.config.clone(),
            stop_rx.clone(),
        ));
        tasks.spawn(wakeup_task(
            WakeupStores {
                wakeups,
                triggers,
                ledger: run_ledger,
            },
            effect_broker,
            reconcilers,
            coordinator.clone(),
            self.config.clone(),
            stop_rx.clone(),
        ));
        tasks.spawn(output_task(
            coordinator.clone(),
            active.clone(),
            output_rx,
            self.config.clone(),
            stop_rx,
        ));

        let result = tokio::select! {
            changed = shutdown.changed() => {
                match changed {
                    Ok(()) if *shutdown.borrow() => Ok(()),
                    Ok(()) => Ok(()),
                    Err(_) => Ok(()),
                }
            }
            task = tasks.join_next() => match task {
                Some(Ok(Ok(()))) => Err(WorkerError::Stopped("critical worker task exited".into())),
                Some(Ok(Err(error))) => Err(error),
                Some(Err(error)) => Err(WorkerError::Task(error.to_string())),
                None => Err(WorkerError::Stopped("worker had no supervised tasks".into())),
            }
        };
        let _ = stop_tx.send(true);
        cancel_all(&active);
        while let Some(task) = tasks.join_next().await {
            if result.is_ok()
                && let Ok(Err(error)) = task
            {
                self.database.close().await;
                return Err(error);
            }
        }
        for extension in extension_clients {
            let _ = extension.shutdown().await;
        }
        self.database.close().await;
        result
    }
}

#[derive(Clone)]
struct Dispatch {
    lease: AttemptLease,
    run_id: String,
    implementation: String,
    input_json: String,
    input_revisions_json: String,
}

struct ActiveAttempt {
    lease: AttemptLease,
    run_id: String,
    cancel: watch::Sender<bool>,
}

#[derive(Clone)]
pub(crate) struct ExecutionControl {
    active: Arc<Mutex<HashMap<String, ActiveAttempt>>>,
}

impl ExecutionControl {
    fn new() -> Self {
        Self {
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn cancel_run(&self, run_id: &str) {
        if let Ok(active) = self.active.lock() {
            for attempt in active.values().filter(|attempt| attempt.run_id == run_id) {
                let _ = attempt.cancel.send(true);
            }
        }
    }
}

#[derive(Clone)]
struct ExecutionRegistry {
    implementations: Arc<HashMap<String, RegisteredImplementation>>,
    targets: Arc<HashMap<String, Arc<dyn ExecutionTarget>>>,
}

#[derive(Clone)]
struct ExecutionStores {
    control: AsyncCoordinator,
    effects: EffectBroker,
    artifacts: ArtifactStore,
}

enum OutputMessage {
    Chunk {
        attempt_id: String,
        chunk: OutputChunk,
    },
    Flush {
        acknowledgement: oneshot::Sender<Result<(), WorkerError>>,
    },
}

async fn scheduler_task(
    coordinator: AsyncCoordinator,
    worker_id: String,
    config: WorkerConfig,
    registry: ExecutionRegistry,
    dispatch: mpsc::Sender<Dispatch>,
    mut stop: watch::Receiver<bool>,
) -> Result<(), WorkerError> {
    let mut interval = tokio::time::interval(config.scheduler_interval);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let now = unix_ms();
                coordinator.recover_expired(now).await?;
                let available = dispatch.capacity().min(config.scheduler_batch);
                if available == 0 { continue; }
                let runnable = coordinator.runnable(now, available).await?;
                coordinator.metric("scheduler_candidates", i64::try_from(runnable.len()).unwrap_or(i64::MAX), "{}", now).await?;
                let mut unsupported = 0_i64;
                for step in runnable {
                    let Some(registration) = registry.implementations.get(&step.implementation) else {
                        unsupported = unsupported.saturating_add(1);
                        let reason = format!("pinned implementation '{}' is not registered; install or reload the package revision and retry the Run", step.implementation);
                        coordinator.fail_unsupported(&step, &reason, now).await?;
                        continue;
                    };
                    if !registry.targets.contains_key(&step.target_id) {
                        unsupported = unsupported.saturating_add(1);
                        let reason = format!("execution target '{}' required by pinned implementation '{}' is not registered", step.target_id, step.implementation);
                        coordinator.fail_unsupported(&step, &reason, now).await?;
                        continue;
                    }
                    let class_capacity = match registration.class {
                        ExecutionClass::Agent => config.agent_capacity,
                        ExecutionClass::Command => config.command_capacity,
                        ExecutionClass::Provider => config.provider_capacity,
                    };
                    let attempt_id = next_attempt_id(&worker_id);
                    let resources = coordinator.required_resources(&step.id).await?;
                    let mut capacities = vec![
                        CapacityRequirement { scope: "global".into(), key: "attempts".into(), maximum: config.global_capacity },
                        CapacityRequirement { scope: "class".into(), key: registration.class.label().into(), maximum: class_capacity },
                        CapacityRequirement { scope: "implementation".into(), key: step.implementation.clone(), maximum: class_capacity },
                        CapacityRequirement { scope: "target".into(), key: step.target_id.clone(), maximum: config.target_capacity },
                    ];
                    if let Some(repository) = &step.repository {
                        capacities.push(CapacityRequirement {
                            scope: "repository".into(),
                            key: repository.clone(),
                            maximum: config.repository_capacity,
                        });
                    }
                    let Some(lease) = coordinator.claim(DurableClaim {
                        attempt_id: &attempt_id,
                        step_id: &step.id,
                        worker_id: &worker_id,
                        now_unix_ms: now,
                        lease_expires_unix_ms: now.saturating_add(duration_ms(config.lease_duration)),
                        resources: &resources,
                        capacities: &capacities,
                    }).await? else { continue };
                    if let Err(error) = dispatch.send(Dispatch {
                        lease,
                        run_id: step.run_id,
                        implementation: step.implementation,
                        input_json: step.input_json,
                        input_revisions_json: step.input_revisions_json,
                    }).await {
                        // A failed ephemeral handoff must not consume durable capacity until lease
                        // expiry. Release it explicitly; a concurrent expiry is already safe.
                        match coordinator.release_handoff(&error.0.lease, unix_ms()).await {
                            Ok(()) | Err(crate::persistence::error::DatabaseError::StaleClaim) => {}
                            Err(error) => return Err(error.into()),
                        }
                        return Err(WorkerError::Stopped("execution dispatcher stopped".into()));
                    }
                }
                coordinator.metric("unsupported_runnable_steps", unsupported, "{}", now).await?;
            }
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() { return Ok(()); }
            }
        }
    }
}

async fn execution_task(
    coordinator: Coordinator,
    registry: ExecutionRegistry,
    active: Arc<Mutex<HashMap<String, ActiveAttempt>>>,
    mut dispatch: mpsc::Receiver<Dispatch>,
    output: mpsc::Sender<OutputMessage>,
    stores: ExecutionStores,
    mut stop: watch::Receiver<bool>,
) -> Result<(), WorkerError> {
    let mut executions = JoinSet::new();
    loop {
        tokio::select! {
            item = dispatch.recv() => match item {
                Some(item) => {
                    let (cancel, cancellation) = watch::channel(false);
                    active.lock().map_err(|_| WorkerError::Stopped("active attempt registry poisoned".into()))?
                        .insert(item.lease.attempt_id.clone(), ActiveAttempt {
                            lease: item.lease.clone(),
                            run_id: item.run_id.clone(),
                            cancel,
                        });
                    if coordinator.run_is_cancelled(&item.run_id).await?
                        && let Ok(active) = active.lock()
                        && let Some(attempt) = active.get(&item.lease.attempt_id)
                    {
                        let _ = attempt.cancel.send(true);
                    }
                    let implementation = registry.implementations
                        .get(&item.implementation)
                        .map(|registration| registration.implementation.clone());
                    let target = registry.targets.get(&item.lease.target_id).cloned();
                    let coordinator = coordinator.clone();
                    let active = active.clone();
                    let output = output.clone();
                    let stores = stores.clone();
                    executions.spawn(async move {
                        let completion_cancellation = cancellation.clone();
                        let result = if *completion_cancellation.borrow() {
                            Err("attempt cancelled".into())
                        } else {
                            match (implementation, target) {
                                (Some(implementation), Some(target)) => target.execute(implementation, ExecutionContext {
                                    run_attempt_id: item.lease.attempt_id.clone(),
                                    step_id: item.lease.step_id.clone(),
                                    input_json: item.input_json,
                                    input_revisions_json: item.input_revisions_json,
                                    output: output.clone(),
                                    cancellation,
                                    control: stores.control,
                                    effects: stores.effects,
                                    artifacts: stores.artifacts,
                                    lease: item.lease.clone(),
                                }).await,
                                (None, _) => Err(format!("unregistered step implementation '{}'", item.implementation)),
                                (_, None) => Err(format!("unregistered execution target '{}'", item.lease.target_id)),
                            }
                        };
                        let (status, result_json) = if *completion_cancellation.borrow() {
                            ("cancelled", serde_json::json!({"error": "attempt cancelled"}).to_string())
                        } else {
                            match result {
                                Ok(value) => ("succeeded", value),
                                Err(error) => ("failed", serde_json::json!({"error": error}).to_string()),
                            }
                        };
                        let (acknowledgement, flushed) = oneshot::channel();
                        output.send(OutputMessage::Flush { acknowledgement }).await
                            .map_err(|_| WorkerError::Stopped("output aggregator stopped before completion".into()))?;
                        flushed.await.map_err(|_| WorkerError::Stopped("output flush acknowledgement dropped".into()))??;
                        let finish = coordinator.finish(&item.lease, AttemptResult {
                            status,
                            result_json: &result_json,
                            finished_unix_ms: unix_ms(),
                        }).await;
                        if let Ok(mut active) = active.lock() { active.remove(&item.lease.attempt_id); }
                        finish.map_err(WorkerError::from)
                    });
                }
                None => return Ok(()),
            },
            completed = executions.join_next(), if !executions.is_empty() => {
                match completed {
                    Some(Ok(Ok(()))) => {},
                    Some(Ok(Err(WorkerError::StaleLease))) => {},
                    Some(Ok(Err(error))) => return Err(error),
                    Some(Err(error)) => return Err(WorkerError::Task(error.to_string())),
                    None => {},
                }
            }
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    cancel_all(&active);
                    while let Some(result) = executions.join_next().await {
                        if let Err(error) = result { return Err(WorkerError::Task(error.to_string())); }
                    }
                    return Ok(());
                }
            }
        }
    }
}

async fn lease_task(
    coordinator: AsyncCoordinator,
    active: Arc<Mutex<HashMap<String, ActiveAttempt>>>,
    config: WorkerConfig,
    mut stop: watch::Receiver<bool>,
) -> Result<(), WorkerError> {
    let mut interval = tokio::time::interval(config.lease_renew_interval);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let leases = active.lock().map_err(|_| WorkerError::Stopped("active attempt registry poisoned".into()))?
                    .values().map(|attempt| attempt.lease.clone()).collect::<Vec<_>>();
                if leases.is_empty() { continue; }
                let now = unix_ms();
                let lost = coordinator.renew_batch(&leases, now, now.saturating_add(duration_ms(config.lease_duration))).await?;
                if !lost.is_empty() {
                    let active = active.lock().map_err(|_| WorkerError::Stopped("active attempt registry poisoned".into()))?;
                    for id in lost { if let Some(attempt) = active.get(&id) { let _ = attempt.cancel.send(true); } }
                }
            }
            changed = stop.changed() => if changed.is_err() || *stop.borrow() { return Ok(()); }
        }
    }
}

#[derive(Default, Deserialize)]
struct PersistedTriggerConfig {
    repository: Option<String>,
    #[serde(default = "empty_inputs")]
    inputs: serde_json::Value,
}

fn empty_inputs() -> serde_json::Value {
    serde_json::json!({})
}

async fn launch_due_trigger(
    wakeups: &WakeupStore,
    ledger: &RunLedger,
    trigger: crate::persistence::wakeups::DueTrigger,
    now_unix_ms: i64,
) -> Result<(), WorkerError> {
    let config: PersistedTriggerConfig =
        serde_json::from_str(&trigger.config_json).map_err(|error| {
            WorkerError::Configuration(format!(
                "trigger '{}' configuration is invalid: {error}",
                trigger.trigger_id
            ))
        })?;
    let run_id = format!("trigger:{}", trigger.id);
    let idempotency_key = format!(
        "trigger:{}:{}",
        trigger.trigger_id, trigger.deduplication_key
    );
    let input_json = trigger
        .input_json
        .clone()
        .unwrap_or_else(|| config.inputs.to_string());
    let repository = (trigger.trigger_kind != "provider_poll")
        .then_some(config.repository.as_deref())
        .flatten();
    let launched = ledger
        .start(StartRun {
            run_id: &run_id,
            definition_snapshot_id: &trigger.definition_snapshot_id,
            repository,
            idempotency_key: &idempotency_key,
            input_json: &input_json,
            now_unix_ms,
            paused: false,
        })
        .await?;
    wakeups
        .complete_trigger(
            &trigger.id,
            &launched,
            &serde_json::json!({"last_due_unix_ms": trigger.due_unix_ms}).to_string(),
            now_unix_ms,
        )
        .await?;
    Ok(())
}

async fn poll_due_provider_trigger(
    triggers: &TriggerStore,
    trigger: &crate::persistence::wakeups::DueTrigger,
    now_unix_ms: i64,
) -> Result<(), WorkerError> {
    let config: PersistedTriggerConfig =
        serde_json::from_str(&trigger.config_json).map_err(|error| {
            WorkerError::Configuration(format!(
                "provider Trigger '{}' configuration is invalid: {error}",
                trigger.trigger_id
            ))
        })?;
    let repository = config.repository.ok_or_else(|| {
        WorkerError::Configuration(format!(
            "provider Trigger '{}' has no repository path",
            trigger.trigger_id
        ))
    })?;
    let schedule: crate::workflow::trigger::TriggerSchedule =
        serde_json::from_str(&trigger.schedule_json).map_err(|error| {
            WorkerError::Configuration(format!(
                "provider Trigger '{}' schedule is invalid: {error}",
                trigger.trigger_id
            ))
        })?;
    let crate::workflow::trigger::TriggerSchedule::ProviderPoll { item_kind, .. } = schedule else {
        return Err(WorkerError::Configuration(format!(
            "Trigger '{}' is not a provider poll",
            trigger.trigger_id
        )));
    };
    let repository = crate::repo::Repository {
        root: PathBuf::from(repository),
    };
    let adapter = crate::remote::dispatcher::RepositoryProviderPollAdapter::new(
        repository.root.clone(),
        crate::config::Config::load(&repository),
        item_kind,
    );
    let checkpoint = trigger
        .checkpoint_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| {
            WorkerError::Configuration(format!("provider checkpoint is invalid: {error}"))
        })?;
    match adapter
        .poll(crate::workflow::trigger::ProviderPollRequest {
            checkpoint,
            max_items: 1_000,
        })
        .await
    {
        Ok(batch) => {
            triggers
                .record_provider_page(&crate::workflow::trigger::ProviderPollPage {
                    trigger_id: trigger.trigger_id.clone(),
                    occurrence_id: trigger.id.clone(),
                    items: batch.items,
                    checkpoint: batch.checkpoint,
                    observed_unix_ms: now_unix_ms,
                })
                .await?;
            triggers
                .complete_poll_occurrence(&trigger.id, now_unix_ms)
                .await?;
        }
        Err(error) => {
            let provider_retry_after = match &error {
                crate::workflow::trigger::ProviderPollError::Retryable {
                    retry_after_unix_ms,
                    ..
                } => *retry_after_unix_ms,
                _ => None,
            };
            let retry_after = triggers
                .record_poll_failure(
                    &trigger.trigger_id,
                    &error.to_string(),
                    now_unix_ms,
                    provider_retry_after,
                )
                .await?;
            triggers
                .defer_poll_occurrence(&trigger.id, retry_after, &error.to_string())
                .await?;
        }
    }
    Ok(())
}

async fn wakeup_task(
    stores: WakeupStores,
    effects: EffectBroker,
    reconcilers: Arc<HashMap<String, Arc<dyn EffectReconciler>>>,
    coordinator: AsyncCoordinator,
    config: WorkerConfig,
    mut stop: watch::Receiver<bool>,
) -> Result<(), WorkerError> {
    let WakeupStores {
        wakeups,
        triggers,
        ledger,
    } = stores;
    let mut interval = tokio::time::interval(config.scheduler_interval);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let now = unix_ms();
                ledger.advance_children(now).await?;
                triggers.materialize_due(now, config.scheduler_batch).await?;
                let released = wakeups.release_due_gates(now, config.scheduler_batch).await?;
                let due = wakeups.due_triggers(now, config.scheduler_batch).await?;
                let due_count = due.len();
                for trigger in due {
                    if trigger.trigger_kind == "provider_poll" && trigger.provider_item_id.is_none() {
                        poll_due_provider_trigger(&triggers, &trigger, now).await?;
                    } else {
                        launch_due_trigger(&wakeups, &ledger, trigger, now).await?;
                    }
                }
                let reconciliation = effects.reconciliation_required(config.scheduler_batch).await?;
                let reconciliation_count = reconciliation.len();
                let mut reconciliation_failures = 0_i64;
                for intent in reconciliation {
                    let Some(reconciler) = reconcilers.get(&intent.effect_kind) else { continue };
                    let id = intent.id.clone();
                    let request = EffectReconciliation {
                        id: intent.id,
                        kind: intent.effect_kind,
                        idempotency_key: intent.idempotency_key,
                        request_json: intent.request_json,
                        previous_result_json: intent.result_json,
                    };
                    match reconciler.reconcile(request).await {
                        Ok(result) => effects.record_reconciliation(&id, result.succeeded, &result.result_json, unix_ms()).await?,
                        Err(_) => reconciliation_failures = reconciliation_failures.saturating_add(1),
                    }
                }
                coordinator.metric("due_gates", i64::try_from(released).unwrap_or(i64::MAX), "{}", now).await?;
                coordinator.metric("due_triggers", i64::try_from(due_count).unwrap_or(i64::MAX), "{}", now).await?;
                coordinator.metric("effects_requiring_reconciliation", i64::try_from(reconciliation_count).unwrap_or(i64::MAX), "{}", now).await?;
                coordinator.metric("effect_reconciliation_failures", reconciliation_failures, "{}", now).await?;
            }
            changed = stop.changed() => if changed.is_err() || *stop.borrow() { return Ok(()); }
        }
    }
}

struct WakeupStores {
    wakeups: WakeupStore,
    triggers: TriggerStore,
    ledger: RunLedger,
}

async fn output_task(
    coordinator: AsyncCoordinator,
    active: Arc<Mutex<HashMap<String, ActiveAttempt>>>,
    mut output: mpsc::Receiver<OutputMessage>,
    config: WorkerConfig,
    mut stop: watch::Receiver<bool>,
) -> Result<(), WorkerError> {
    let mut pending = HashMap::<String, Vec<OutputChunk>>::new();
    let mut interval = tokio::time::interval(config.output_flush_interval);
    loop {
        tokio::select! {
            message = output.recv() => match message {
                Some(OutputMessage::Chunk { attempt_id, chunk }) => {
                    pending.entry(attempt_id).or_default().push(chunk);
                    if pending.values().map(Vec::len).sum::<usize>() >= config.output_batch_chunks {
                        flush_output(&coordinator, &active, &mut pending, config.output_budget_bytes).await?;
                    }
                }
                Some(OutputMessage::Flush { acknowledgement }) => {
                    let flushed = flush_output(&coordinator, &active, &mut pending, config.output_budget_bytes).await;
                    let failed = flushed.as_ref().err().map(ToString::to_string);
                    let _ = acknowledgement.send(flushed);
                    if let Some(error) = failed { return Err(WorkerError::Stopped(error)); }
                }
                None => {
                    flush_output(&coordinator, &active, &mut pending, config.output_budget_bytes).await?;
                    return Ok(());
                }
            },
            _ = interval.tick() => flush_output(&coordinator, &active, &mut pending, config.output_budget_bytes).await?,
            changed = stop.changed() => if changed.is_err() || *stop.borrow() {
                flush_output(&coordinator, &active, &mut pending, config.output_budget_bytes).await?;
                return Ok(());
            }
        }
    }
}

async fn flush_output(
    coordinator: &AsyncCoordinator,
    active: &Arc<Mutex<HashMap<String, ActiveAttempt>>>,
    pending: &mut HashMap<String, Vec<OutputChunk>>,
    budget: usize,
) -> Result<(), WorkerError> {
    let batches = std::mem::take(pending);
    for (attempt_id, chunks) in batches {
        let lease = active
            .lock()
            .map_err(|_| WorkerError::Stopped("active attempt registry poisoned".into()))?
            .get(&attempt_id)
            .map(|attempt| attempt.lease.clone());
        let Some(lease) = lease else { continue };
        match coordinator
            .append_output(&lease, &chunks, budget, unix_ms())
            .await
        {
            Ok(()) | Err(crate::persistence::error::DatabaseError::StaleClaim) => {}
            Err(crate::persistence::error::DatabaseError::OutputBudgetExceeded {
                attempted_bytes,
                maximum_bytes,
            }) => {
                if let Ok(active) = active.lock()
                    && let Some(attempt) = active.get(&attempt_id)
                {
                    let _ = attempt.cancel.send(true);
                }
                coordinator
                    .metric(
                        "output_truncations",
                        1,
                        &serde_json::json!({
                            "attempt_id": attempt_id,
                            "attempted_bytes": attempted_bytes,
                            "maximum_bytes": maximum_bytes,
                        })
                        .to_string(),
                        unix_ms(),
                    )
                    .await?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn cancel_all(active: &Mutex<HashMap<String, ActiveAttempt>>) {
    if let Ok(active) = active.lock() {
        for attempt in active.values() {
            let _ = attempt.cancel.send(true);
        }
    }
}

fn validate_config(config: &WorkerConfig) -> Result<(), WorkerError> {
    if config.dispatch_capacity == 0
        || config.output_capacity == 0
        || config.scheduler_batch == 0
        || config.global_capacity == 0
        || config.agent_capacity == 0
        || config.command_capacity == 0
        || config.provider_capacity == 0
        || config.target_capacity == 0
        || config.repository_capacity == 0
        || config.output_batch_chunks == 0
        || config.output_budget_bytes == 0
    {
        return Err(WorkerError::Configuration(
            "worker capacities and budgets must be positive".into(),
        ));
    }
    if config.lease_renew_interval >= config.lease_duration {
        return Err(WorkerError::Configuration(
            "lease renewal interval must be shorter than lease duration".into(),
        ));
    }
    Ok(())
}

fn next_attempt_id(worker_id: &str) -> String {
    format!(
        "attempt-{worker_id}-{}-{}",
        unix_ms(),
        ATTEMPT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn duration_ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[derive(Debug)]
pub enum WorkerError {
    Database(crate::WorkflowOperationError),
    StaleLease,
    Configuration(String),
    Stopped(String),
    Task(String),
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(error) => error.fmt(formatter),
            Self::StaleLease => formatter.write_str("execution lease is stale"),
            Self::Configuration(error) | Self::Stopped(error) | Self::Task(error) => {
                formatter.write_str(error)
            }
        }
    }
}

impl std::error::Error for WorkerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::StaleLease | Self::Configuration(_) | Self::Stopped(_) | Self::Task(_) => None,
        }
    }
}

impl From<crate::persistence::error::DatabaseError> for WorkerError {
    fn from(error: crate::persistence::error::DatabaseError) -> Self {
        if matches!(error, crate::persistence::error::DatabaseError::StaleClaim) {
            Self::StaleLease
        } else {
            Self::Database(error.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    use super::*;
    use crate::persistence::run_ledger::{
        MaterializedStep, RegisterDefinition, RunLedger, StartRun,
    };

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);

    struct FakeImplementation(Arc<AtomicBool>);

    struct TrackingTarget(Arc<AtomicBool>);

    impl ExecutionTarget for TrackingTarget {
        fn execute<'a>(
            &'a self,
            implementation: Arc<dyn StepImplementation>,
            context: ExecutionContext,
        ) -> TargetFuture<'a> {
            self.0.store(true, Ordering::Release);
            Box::pin(async move { implementation.execute(context).await })
        }
    }

    impl StepImplementation for FakeImplementation {
        fn execute<'a>(&'a self, context: ExecutionContext) -> StepFuture<'a> {
            Box::pin(async move {
                context
                    .stdout("bounded output")
                    .await
                    .map_err(|error| error.to_string())?;
                context
                    .publish_artifact(ArtifactPublication {
                        id: "artifact",
                        revision: 1,
                        digest: "sha256:test",
                        sensitivity: "internal",
                        content: ArtifactContent::Inline(b"artifact body"),
                        parents: &[],
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                self.0.store(true, Ordering::Release);
                Ok("{}".into())
            })
        }
    }

    struct BlockingImplementation {
        started: Arc<AtomicUsize>,
        release: watch::Receiver<bool>,
    }

    struct CancellationImplementation {
        started: Arc<AtomicBool>,
        cancelled: Arc<AtomicBool>,
    }

    impl StepImplementation for CancellationImplementation {
        fn execute<'a>(&'a self, context: ExecutionContext) -> StepFuture<'a> {
            let started = self.started.clone();
            let cancelled = self.cancelled.clone();
            Box::pin(async move {
                started.store(true, Ordering::Release);
                let mut cancellation = context.cancellation();
                while !*cancellation.borrow() {
                    cancellation
                        .changed()
                        .await
                        .map_err(|_| "cancellation dropped".to_string())?;
                }
                cancelled.store(true, Ordering::Release);
                Err("cancelled".into())
            })
        }
    }

    impl StepImplementation for BlockingImplementation {
        fn execute<'a>(&'a self, _context: ExecutionContext) -> StepFuture<'a> {
            let started = self.started.clone();
            let mut release = self.release.clone();
            Box::pin(async move {
                started.fetch_add(1, Ordering::AcqRel);
                while !*release.borrow() {
                    release
                        .changed()
                        .await
                        .map_err(|_| "release dropped".to_string())?;
                }
                Ok("{}".into())
            })
        }
    }

    #[test]
    fn supervised_worker_executes_from_durable_state_and_flushes_before_finish() {
        let path = std::env::temp_dir().join(format!(
            "prism-async-worker-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let database = WorkflowDatabase::open(&path).await.unwrap();
                let ledger = RunLedger::new(database.clone());
                ledger
                    .register_definition(RegisterDefinition {
                        id: "definition",
                        name: "fake",
                        revision: "1",
                        source: "test",
                        trusted: true,
                        body_json: "{}",
                        digest: "digest",
                        now_unix_ms: 1,
                    })
                    .await
                    .unwrap();
                ledger
                    .start_materialized(
                        StartRun {
                            run_id: "run",
                            definition_snapshot_id: "definition",
                            repository: None,
                            idempotency_key: "run",
                            input_json: "{}",
                            now_unix_ms: 2,
                            paused: false,
                        },
                        vec![MaterializedStep {
                            id: "step".into(),
                            key: "step".into(),
                            implementation: "fake".into(),
                            target_id: "test-target".into(),
                            input_json: "{}".into(),
                            dependencies: vec![],
                            resources: vec!["repo:test".into()],
                        }],
                    )
                    .await
                    .unwrap();
                database.close().await;

                let complete = Arc::new(AtomicBool::new(false));
                let target_used = Arc::new(AtomicBool::new(false));
                let config = WorkerConfig {
                    scheduler_interval: Duration::from_millis(5),
                    output_flush_interval: Duration::from_millis(5),
                    lease_duration: Duration::from_secs(2),
                    lease_renew_interval: Duration::from_millis(100),
                    ..WorkerConfig::default()
                };
                let mut worker = WorkflowWorker::open(&path, "worker", config).await.unwrap();
                worker
                    .register("fake", FakeImplementation(complete.clone()))
                    .unwrap();
                worker
                    .register_target("test-target", TrackingTarget(target_used.clone()))
                    .unwrap();
                let (shutdown, receiver) = watch::channel(false);
                let task = tokio::spawn(worker.run(receiver));
                for _ in 0..200 {
                    if complete.load(Ordering::Acquire) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert!(complete.load(Ordering::Acquire));
                assert!(target_used.load(Ordering::Acquire));
                tokio::time::sleep(Duration::from_millis(25)).await;
                shutdown.send(true).unwrap();
                task.await.unwrap().unwrap();

                let database = WorkflowDatabase::open(&path).await.unwrap();
                let status: String =
                    sqlx::query_scalar("select status from workflow_step where id = 'step'")
                        .fetch_one(database.readers())
                        .await
                        .unwrap();
                let output: Vec<u8> = sqlx::query_scalar(
                    "select body from attempt_output where attempt_id like 'attempt-%'",
                )
                .fetch_one(database.readers())
                .await
                .unwrap();
                let artifact: Vec<u8> =
                    sqlx::query_scalar("select inline_body from artifact where id = 'artifact'")
                        .fetch_one(database.readers())
                        .await
                        .unwrap();
                assert_eq!(status, "succeeded");
                assert_eq!(output, b"bounded output");
                assert_eq!(artifact, b"artifact body");
                let projection = crate::WorkflowOperations::from_database(database.clone())
                    .inspect("run")
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(projection.steps[0].input_json, "{}");
                assert_eq!(projection.attempts[0].output.len(), 1);
                assert_eq!(projection.attempts[0].output[0].body, b"bounded output");
                assert_eq!(projection.artifacts.len(), 1);
                assert_eq!(
                    projection.artifacts[0].inline_body.as_deref(),
                    Some(b"artifact body".as_slice())
                );
                database.close().await;
            });
        let _ = std::fs::remove_file(path);
    }

    struct NoisyImplementation;

    impl StepImplementation for NoisyImplementation {
        fn execute<'a>(&'a self, context: ExecutionContext) -> StepFuture<'a> {
            Box::pin(async move {
                context
                    .stdout("output beyond budget")
                    .await
                    .map_err(|error| error.to_string())?;
                let mut cancellation = context.cancellation();
                while !*cancellation.borrow() {
                    cancellation
                        .changed()
                        .await
                        .map_err(|_| "cancellation dropped".to_string())?;
                }
                Err("output truncated".into())
            })
        }
    }

    #[test]
    fn execution_classes_have_independent_durable_capacity() {
        let path = std::env::temp_dir().join(format!(
            "prism-worker-capacity-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let operations = crate::WorkflowOperations::open(&path).await.unwrap();
                operations
                    .register_definition(crate::DefinitionSnapshot {
                        id: "definition",
                        name: "capacity",
                        revision: "1",
                        source: "test",
                        trusted: true,
                        body_json: "{}",
                        digest: "digest",
                        now_unix_ms: 1,
                    })
                    .await
                    .unwrap();
                operations
                    .launch_definition(
                        crate::LaunchWorkflow {
                            run_id: "run",
                            definition_snapshot_id: "definition",
                            repository: Some("repo"),
                            idempotency_key: "run",
                            input_json: "{}",
                            now_unix_ms: 2,
                        },
                        (1..=2)
                            .map(|index| crate::WorkflowStep {
                                id: format!("step-{index}"),
                                key: format!("step-{index}"),
                                implementation: "blocking-command".into(),
                                target_id: "local".into(),
                                input_json: "{}".into(),
                                dependencies: vec![],
                                resources: vec![],
                            })
                            .collect(),
                    )
                    .await
                    .unwrap();

                let config = WorkerConfig {
                    global_capacity: 4,
                    command_capacity: 1,
                    scheduler_interval: Duration::from_millis(5),
                    lease_duration: Duration::from_secs(2),
                    lease_renew_interval: Duration::from_millis(100),
                    ..WorkerConfig::default()
                };
                let started = Arc::new(AtomicUsize::new(0));
                let (release, receiver) = watch::channel(false);
                let mut worker = WorkflowWorker::open(&path, "worker", config).await.unwrap();
                worker
                    .register_as(
                        "blocking-command",
                        ExecutionClass::Command,
                        BlockingImplementation {
                            started: started.clone(),
                            release: receiver,
                        },
                    )
                    .unwrap();
                let (shutdown, shutdown_receiver) = watch::channel(false);
                let worker_task = tokio::spawn(worker.run(shutdown_receiver));
                for _ in 0..100 {
                    if started.load(Ordering::Acquire) == 1 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
                assert_eq!(started.load(Ordering::Acquire), 1);
                let projection = operations.inspect("run").await.unwrap().unwrap();
                assert_eq!(
                    projection
                        .attempts
                        .iter()
                        .filter(|attempt| attempt.status == "claimed")
                        .count(),
                    1
                );
                release.send(true).unwrap();
                for _ in 0..100 {
                    if operations.inspect("run").await.unwrap().unwrap().status == "succeeded" {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                shutdown.send(true).unwrap();
                worker_task.await.unwrap().unwrap();
            });
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cancelling_a_run_signals_its_active_attempt() {
        let path = std::env::temp_dir().join(format!(
            "prism-worker-run-cancellation-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let started = Arc::new(AtomicBool::new(false));
                let cancelled = Arc::new(AtomicBool::new(false));
                let mut worker = WorkflowWorker::open(
                    &path,
                    "worker",
                    WorkerConfig {
                        scheduler_interval: Duration::from_millis(5),
                        output_flush_interval: Duration::from_millis(5),
                        ..WorkerConfig::default()
                    },
                )
                .await
                .unwrap();
                worker
                    .register(
                        "cancellable",
                        CancellationImplementation {
                            started: started.clone(),
                            cancelled: cancelled.clone(),
                        },
                    )
                    .unwrap();
                let operations = worker.operations();
                operations
                    .register_definition(crate::DefinitionSnapshot {
                        id: "definition",
                        name: "cancellation",
                        revision: "1",
                        source: "test",
                        trusted: true,
                        body_json: "{}",
                        digest: "digest",
                        now_unix_ms: 1,
                    })
                    .await
                    .unwrap();
                operations
                    .launch_definition(
                        crate::LaunchWorkflow {
                            run_id: "run",
                            definition_snapshot_id: "definition",
                            repository: None,
                            idempotency_key: "run",
                            input_json: "{}",
                            now_unix_ms: 2,
                        },
                        vec![crate::WorkflowStep {
                            id: "step".into(),
                            key: "step".into(),
                            implementation: "cancellable".into(),
                            target_id: "local".into(),
                            input_json: "{}".into(),
                            dependencies: vec![],
                            resources: vec![],
                        }],
                    )
                    .await
                    .unwrap();
                let (shutdown, shutdown_receiver) = watch::channel(false);
                let task = tokio::spawn(worker.run(shutdown_receiver));
                for _ in 0..100 {
                    if started.load(Ordering::Acquire) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert!(started.load(Ordering::Acquire));

                operations
                    .command("run", crate::WorkflowCommand::Cancel, 3)
                    .await
                    .unwrap();
                for _ in 0..100 {
                    if cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert!(cancelled.load(Ordering::Acquire));
                for _ in 0..100 {
                    if operations.inspect("run").await.unwrap().unwrap().attempts[0].status
                        == "cancelled"
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                let projection = operations.inspect("run").await.unwrap().unwrap();
                assert_eq!(projection.status, "cancelled");
                assert_eq!(projection.attempts[0].status, "cancelled");
                shutdown.send(true).unwrap();
                task.await.unwrap().unwrap();
            });
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn expired_attempt_with_recorded_process_requires_explicit_recovery() {
        let path = std::env::temp_dir().join(format!(
            "prism-worker-crash-recovery-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let operations = crate::WorkflowOperations::open(&path).await.unwrap();
                operations
                    .register_definition(crate::DefinitionSnapshot {
                        id: "definition",
                        name: "crash-recovery",
                        revision: "1",
                        source: "test",
                        trusted: true,
                        body_json: "{}",
                        digest: "digest",
                        now_unix_ms: 1,
                    })
                    .await
                    .unwrap();
                operations
                    .launch_definition(
                        crate::LaunchWorkflow {
                            run_id: "run",
                            definition_snapshot_id: "definition",
                            repository: None,
                            idempotency_key: "run",
                            input_json: "{}",
                            now_unix_ms: 2,
                        },
                        vec![crate::WorkflowStep {
                            id: "step".into(),
                            key: "step".into(),
                            implementation: "command".into(),
                            target_id: "local".into(),
                            input_json: serde_json::json!({
                                "program": "/bin/sh",
                                "args": ["-c", "sleep 30"]
                            })
                            .to_string(),
                            dependencies: vec![],
                            resources: vec![],
                        }],
                    )
                    .await
                    .unwrap();
                let config = WorkerConfig {
                    scheduler_interval: Duration::from_millis(5),
                    lease_duration: Duration::from_millis(80),
                    lease_renew_interval: Duration::from_millis(20),
                    ..WorkerConfig::default()
                };
                let mut worker = WorkflowWorker::open(&path, "first", config.clone())
                    .await
                    .unwrap();
                worker.register_builtins().unwrap();
                let first_database = worker.database.clone();
                let (_shutdown, receiver) = watch::channel(false);
                let first = tokio::spawn(worker.run(receiver));
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if operations
                            .inspect("run")
                            .await
                            .unwrap()
                            .unwrap()
                            .attempts
                            .iter()
                            .any(|attempt| attempt.process_id.is_some())
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("command process should be recorded before simulating a crash");
                first.abort();
                let _ = first.await;
                first_database.close().await;
                tokio::time::sleep(Duration::from_millis(120)).await;

                let mut replacement = WorkflowWorker::open(&path, "replacement", config)
                    .await
                    .unwrap();
                replacement.register_builtins().unwrap();
                let (shutdown, receiver) = watch::channel(false);
                let replacement = tokio::spawn(replacement.run(receiver));
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if operations.inspect("run").await.unwrap().unwrap().status
                            == "recovery_required"
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("replacement worker should require explicit recovery");
                assert_eq!(
                    operations.inspect("run").await.unwrap().unwrap().status,
                    "recovery_required"
                );
                shutdown.send(true).unwrap();
                replacement.await.unwrap().unwrap();
            });
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn scale_contract_keeps_waiting_runs_dormant_and_active_attempts_bounded() {
        let path = std::env::temp_dir().join(format!(
            "prism-worker-scale-contract-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let operations = crate::WorkflowOperations::open(&path).await.unwrap();
                operations
                    .register_definition(crate::DefinitionSnapshot {
                        id: "definition",
                        name: "scale-contract",
                        revision: "1",
                        source: "test",
                        trusted: true,
                        body_json: "{}",
                        digest: "digest",
                        now_unix_ms: 1,
                    })
                    .await
                    .unwrap();

                for index in 0..200 {
                    let run_id = format!("waiting-run-{index}");
                    let step_id = format!("waiting-step-{index}");
                    operations
                        .launch_definition(
                            crate::LaunchWorkflow {
                                run_id: &run_id,
                                definition_snapshot_id: "definition",
                                repository: None,
                                idempotency_key: &run_id,
                                input_json: "{}",
                                now_unix_ms: 2,
                            },
                            vec![crate::WorkflowStep {
                                id: step_id.clone(),
                                key: "wait".into(),
                                implementation: "blocking".into(),
                                target_id: "local".into(),
                                input_json: "{}".into(),
                                dependencies: vec![],
                                resources: vec![],
                            }],
                        )
                        .await
                        .unwrap();
                    operations
                        .wait_on_gate(&step_id, "scale", i64::MAX, "{}", 3)
                        .await
                        .unwrap();
                }

                let active_steps = (0..100)
                    .map(|index| crate::WorkflowStep {
                        id: format!("active-step-{index}"),
                        key: format!("active-{index}"),
                        implementation: "blocking".into(),
                        target_id: "local".into(),
                        input_json: "{}".into(),
                        dependencies: vec![],
                        resources: vec![],
                    })
                    .collect();
                operations
                    .launch_definition(
                        crate::LaunchWorkflow {
                            run_id: "active-run",
                            definition_snapshot_id: "definition",
                            repository: Some("repo"),
                            idempotency_key: "active-run",
                            input_json: "{}",
                            now_unix_ms: 3,
                        },
                        active_steps,
                    )
                    .await
                    .unwrap();

                let started = Arc::new(AtomicUsize::new(0));
                let (release, receiver) = watch::channel(false);
                let mut worker = WorkflowWorker::open(
                    &path,
                    "scale-worker",
                    WorkerConfig {
                        dispatch_capacity: 8,
                        global_capacity: 8,
                        agent_capacity: 8,
                        target_capacity: 8,
                        repository_capacity: 8,
                        scheduler_batch: 32,
                        scheduler_interval: Duration::from_millis(5),
                        lease_duration: Duration::from_secs(2),
                        lease_renew_interval: Duration::from_millis(100),
                        ..WorkerConfig::default()
                    },
                )
                .await
                .unwrap();
                worker
                    .register(
                        "blocking",
                        BlockingImplementation {
                            started: started.clone(),
                            release: receiver,
                        },
                    )
                    .unwrap();
                let (shutdown, shutdown_receiver) = watch::channel(false);
                let worker_task = tokio::spawn(worker.run(shutdown_receiver));
                for _ in 0..200 {
                    if started.load(Ordering::Acquire) == 8 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert_eq!(started.load(Ordering::Acquire), 8);

                let projection = operations.inspect("active-run").await.unwrap().unwrap();
                assert_eq!(
                    projection
                        .attempts
                        .iter()
                        .filter(|attempt| attempt.status == "claimed")
                        .count(),
                    8
                );
                for index in [0, 37, 199] {
                    let projection = operations
                        .inspect(&format!("waiting-run-{index}"))
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(projection.status, "waiting");
                    assert!(projection.attempts.is_empty());
                }

                let readers = (0..16)
                    .map(|_| {
                        let operations = operations.clone();
                        tokio::spawn(async move {
                            operations
                                .inspect("active-run")
                                .await
                                .unwrap()
                                .unwrap()
                                .steps
                                .len()
                        })
                    })
                    .collect::<Vec<_>>();
                for reader in readers {
                    assert_eq!(reader.await.unwrap(), 100);
                }

                release.send(true).unwrap();
                for _ in 0..500 {
                    if operations
                        .inspect("active-run")
                        .await
                        .unwrap()
                        .unwrap()
                        .status
                        == "succeeded"
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert_eq!(
                    operations
                        .inspect("active-run")
                        .await
                        .unwrap()
                        .unwrap()
                        .status,
                    "succeeded"
                );
                shutdown.send(true).unwrap();
                worker_task.await.unwrap().unwrap();
                drop(operations);

                let reopened = crate::WorkflowOperations::open(&path).await.unwrap();
                let waiting = reopened.inspect("waiting-run-199").await.unwrap().unwrap();
                assert_eq!(waiting.status, "waiting");
                assert!(waiting.attempts.is_empty());

                let database = WorkflowDatabase::open(&path).await.unwrap();
                let wakeups = WakeupStore::new(database.clone());
                assert_eq!(wakeups.release_due_gates(i64::MAX, 256).await.unwrap(), 200);
                let released = reopened.inspect("waiting-run-199").await.unwrap().unwrap();
                assert_eq!(released.status, "runnable");
                assert!(
                    released
                        .events
                        .iter()
                        .any(|event| event.kind == "gate_waiting")
                );
                assert!(
                    released
                        .events
                        .iter()
                        .any(|event| event.kind == "gate_released")
                );
                database.close().await;
            });
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn output_budget_cancels_only_the_attempt_and_keeps_worker_healthy() {
        let path = std::env::temp_dir().join(format!(
            "prism-worker-output-budget-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let operations = crate::WorkflowOperations::open(&path).await.unwrap();
                operations
                    .register_definition(crate::DefinitionSnapshot {
                        id: "definition",
                        name: "output-budget",
                        revision: "1",
                        source: "test",
                        trusted: true,
                        body_json: "{}",
                        digest: "digest",
                        now_unix_ms: 1,
                    })
                    .await
                    .unwrap();
                operations
                    .launch_definition(
                        crate::LaunchWorkflow {
                            run_id: "run",
                            definition_snapshot_id: "definition",
                            repository: None,
                            idempotency_key: "run",
                            input_json: "{}",
                            now_unix_ms: 2,
                        },
                        vec![crate::WorkflowStep {
                            id: "step".into(),
                            key: "step".into(),
                            implementation: "noisy".into(),
                            target_id: "local".into(),
                            input_json: "{}".into(),
                            dependencies: vec![],
                            resources: vec![],
                        }],
                    )
                    .await
                    .unwrap();
                let mut worker = WorkflowWorker::open(
                    &path,
                    "worker",
                    WorkerConfig {
                        output_budget_bytes: 4,
                        output_flush_interval: Duration::from_millis(5),
                        scheduler_interval: Duration::from_millis(5),
                        lease_duration: Duration::from_secs(2),
                        lease_renew_interval: Duration::from_millis(100),
                        ..WorkerConfig::default()
                    },
                )
                .await
                .unwrap();
                worker.register("noisy", NoisyImplementation).unwrap();
                let (shutdown, shutdown_receiver) = watch::channel(false);
                let task = tokio::spawn(worker.run(shutdown_receiver));
                for _ in 0..100 {
                    if operations.inspect("run").await.unwrap().unwrap().status == "cancelled" {
                        break;
                    }
                    assert!(
                        !task.is_finished(),
                        "worker stopped after one attempt exceeded output budget"
                    );
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert_eq!(
                    operations.inspect("run").await.unwrap().unwrap().status,
                    "cancelled"
                );
                shutdown.send(true).unwrap();
                task.await.unwrap().unwrap();
            });
        let _ = std::fs::remove_file(path);
    }
}
