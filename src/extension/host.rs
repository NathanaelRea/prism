use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use prism_extension_protocol::{
    AttemptEnvelope, DEFAULT_MAX_FRAME_BYTES, ExecuteResult, ExtensionDescriptor, HOST_FEATURES,
    Hello, HostOperation, Message, PROTOCOL_MAJOR, PROTOCOL_MINOR, ProtocolError, ProtocolVersion,
    StructuredRender,
};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot, watch};

const GLOBAL_EXTENSION_CALL_LIMIT: usize = 64;
static GLOBAL_EXTENSION_CALLS: OnceLock<Arc<Semaphore>> = OnceLock::new();
type RevisionCallLimits = HashMap<String, (usize, Weak<Semaphore>)>;
static REVISION_EXTENSION_CALLS: OnceLock<Mutex<RevisionCallLimits>> = OnceLock::new();

pub type HostFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, ProtocolError>> + Send + 'a>>;

pub trait HostDispatcher: Send + Sync + 'static {
    fn dispatch<'a>(
        &'a self,
        attempt_id: &'a str,
        generation: u64,
        operation: HostOperation,
    ) -> HostFuture<'a>;
}

/// Stable service boundary behind the allowlisted extension-to-host dispatcher.
pub trait HostOperationServices: Send + Sync + 'static {
    fn read_artifact<'a>(
        &'a self,
        attempt_id: &'a str,
        generation: u64,
        artifact: prism_extension_protocol::ArtifactReference,
    ) -> HostFuture<'a>;
    fn trace_process<'a>(
        &'a self,
        attempt_id: &'a str,
        generation: u64,
        pid: u32,
        identity: Option<String>,
    ) -> HostFuture<'a>;
    fn trace_agent<'a>(
        &'a self,
        attempt_id: &'a str,
        generation: u64,
        session_id: String,
        metadata: Value,
    ) -> HostFuture<'a>;
    /// Handles the typed Standard service operations. Keeping this as one protocol-valued
    /// method prevents the public extension seam from exposing internal provider adapters,
    /// credentials, paths, or persistence handles.
    fn standard_operation<'a>(
        &'a self,
        attempt_id: &'a str,
        generation: u64,
        operation: HostOperation,
    ) -> HostFuture<'a>;
}

pub struct AllowlistedHostDispatcher<S> {
    services: S,
}

impl<S> AllowlistedHostDispatcher<S> {
    pub fn new(services: S) -> Self {
        Self { services }
    }
}

impl<S: HostOperationServices> HostDispatcher for AllowlistedHostDispatcher<S> {
    fn dispatch<'a>(
        &'a self,
        attempt_id: &'a str,
        generation: u64,
        operation: HostOperation,
    ) -> HostFuture<'a> {
        match operation {
            HostOperation::ReadArtifact { artifact } => self
                .services
                .read_artifact(attempt_id, generation, artifact),
            HostOperation::TraceProcess { pid, identity } => self
                .services
                .trace_process(attempt_id, generation, pid, identity),
            HostOperation::TraceAgent {
                session_id,
                metadata,
            } => self
                .services
                .trace_agent(attempt_id, generation, session_id, metadata),
            operation @ (HostOperation::RunProcess { .. }
            | HostOperation::RunAgent { .. }
            | HostOperation::ObserveProvider { .. }
            | HostOperation::Commit { .. }
            | HostOperation::Push { .. }
            | HostOperation::CreateChangeRequest { .. }
            | HostOperation::ResolveReviewThreads { .. }
            | HostOperation::SquashMerge { .. }
            | HostOperation::DeleteWorktree { .. }) => self
                .services
                .standard_operation(attempt_id, generation, operation),
        }
    }
}

pub struct NoHostOperations;

impl HostDispatcher for NoHostOperations {
    fn dispatch<'a>(
        &'a self,
        _attempt_id: &'a str,
        _generation: u64,
        operation: HostOperation,
    ) -> HostFuture<'a> {
        Box::pin(async move {
            Err(ProtocolError::new(
                "operation_unavailable",
                format!("host operation is not available: {operation:?}"),
            ))
        })
    }
}

#[derive(Clone, Debug)]
pub struct HostLimits {
    pub max_frame_bytes: usize,
    pub max_diagnostic_bytes: usize,
    pub handshake_timeout: Duration,
    pub request_timeout: Duration,
    pub heartbeat_interval: Duration,
    pub heartbeat_timeout: Duration,
    pub cancellation_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub max_concurrent_calls_per_revision: usize,
}

impl Default for HostLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_diagnostic_bytes: 256 * 1024,
            handshake_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_timeout: Duration::from_secs(2),
            cancellation_timeout: Duration::from_secs(2),
            shutdown_timeout: Duration::from_secs(2),
            max_concurrent_calls_per_revision: 8,
        }
    }
}

type ResponseSender = oneshot::Sender<Result<Message, ExtensionHostError>>;

pub struct ExtensionClient {
    executable: PathBuf,
    extension_id: String,
    extension_revision: String,
    sdk_version: String,
    package_id: String,
    platform: String,
    executable_digest: String,
    descriptor: ExtensionDescriptor,
    writer: Arc<tokio::sync::Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<String, ResponseSender>>>,
    sequence: AtomicU64,
    child: Arc<tokio::sync::Mutex<Option<Child>>>,
    process: crate::process::RecordedProcess,
    diagnostics: Arc<Mutex<BoundedDiagnostics>>,
    failed: Arc<AtomicBool>,
    limits: HostLimits,
    dispatcher: Arc<dyn HostDispatcher>,
    revision_calls: Arc<Semaphore>,
}

/// Keeps one extension revision alive, health-checking and restarting the process without
/// changing the executable revision used by callers.
pub struct ExtensionSupervisor {
    current: Arc<tokio::sync::RwLock<Arc<ExtensionClient>>>,
    shutdown: watch::Sender<bool>,
}

impl ExtensionSupervisor {
    pub async fn launch(
        executable: impl AsRef<Path>,
        dispatcher: Arc<dyn HostDispatcher>,
        limits: HostLimits,
    ) -> Result<Arc<Self>, ExtensionHostError> {
        let client = ExtensionClient::launch(executable, dispatcher, limits.clone()).await?;
        let (shutdown, mut stopping) = watch::channel(false);
        let supervisor = Arc::new(Self {
            current: Arc::new(tokio::sync::RwLock::new(client)),
            shutdown,
        });
        let current = supervisor.current.clone();
        tokio::spawn(async move {
            let mut heartbeat = tokio::time::interval(limits.heartbeat_interval);
            heartbeat.tick().await;
            loop {
                tokio::select! {
                    _ = heartbeat.tick() => {
                        let client = current.read().await.clone();
                        if client.heartbeat().await.is_err()
                            && let Ok(replacement) = client.restart().await
                        {
                            *current.write().await = replacement;
                        }
                    }
                    changed = stopping.changed() => {
                        if changed.is_err() || *stopping.borrow() { break; }
                    }
                }
            }
        });
        Ok(supervisor)
    }

    pub async fn client(&self) -> Arc<ExtensionClient> {
        self.current.read().await.clone()
    }

    pub async fn execute(
        &self,
        attempt: AttemptEnvelope,
        cancellation: watch::Receiver<bool>,
    ) -> Result<ExecuteResult, ExtensionHostError> {
        self.current
            .read()
            .await
            .clone()
            .execute(attempt, cancellation)
            .await
    }

    pub async fn shutdown(&self) -> Result<(), ExtensionHostError> {
        let _ = self.shutdown.send(true);
        self.current.read().await.clone().shutdown().await
    }
}

impl Drop for ExtensionSupervisor {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

impl ExtensionClient {
    pub async fn launch(
        executable: impl AsRef<Path>,
        dispatcher: Arc<dyn HostDispatcher>,
        limits: HostLimits,
    ) -> Result<Arc<Self>, ExtensionHostError> {
        let started = Instant::now();
        validate_limits(&limits)?;
        let executable = executable.as_ref().to_path_buf();
        let expected_digest = crate::resource::ContentRevision::digest(
            &std::fs::read(&executable).map_err(|error| {
                ExtensionHostError::Handshake(format!(
                    "read extension executable for verification: {error}"
                ))
            })?,
        )
        .to_string();
        let mut command = Command::new(&executable);
        command.as_std_mut().process_group(0);
        command
            .env("PRISM_EXTENSION_EXECUTABLE_DIGEST", &expected_digest)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| ExtensionHostError::Spawn(executable.clone(), error.to_string()))?;
        let pid = child.id().ok_or_else(|| {
            ExtensionHostError::Spawn(executable.clone(), "process has no id".into())
        })?;
        let process = tokio::task::spawn_blocking(move || crate::process::record_process(pid))
            .await
            .map_err(|error| ExtensionHostError::Process(error.to_string()))?
            .map_err(|error| ExtensionHostError::Process(error.to_string()))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ExtensionHostError::Spawn(executable.clone(), "missing stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ExtensionHostError::Spawn(executable.clone(), "missing stdout".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            ExtensionHostError::Spawn(executable.clone(), "missing stderr".into())
        })?;
        let mut stdout = BufReader::new(stdout);
        write_message(
            &mut stdin,
            &Message::Hello {
                hello: Hello {
                    protocol: ProtocolVersion::CURRENT,
                    features: HOST_FEATURES
                        .iter()
                        .map(|feature| (*feature).into())
                        .collect(),
                    host: format!("prism/{}", env!("CARGO_PKG_VERSION")),
                },
            },
            limits.max_frame_bytes,
        )
        .await?;
        let acknowledgement = timeout_frame(&mut stdout, &limits, "hello acknowledgement").await?;
        let Message::HelloAck { hello } = acknowledgement else {
            terminate_process(process).await;
            return Err(ExtensionHostError::Handshake("expected hello_ack".into()));
        };
        if let Err(error) = validate_negotiated_version(hello.protocol) {
            terminate_process(process).await;
            return Err(error);
        }
        validate_features(&hello.features)?;
        if hello.sdk_version.trim().is_empty()
            || hello.package_id.trim().is_empty()
            || hello.platform.trim().is_empty()
            || hello.executable_digest != expected_digest
        {
            terminate_process(process).await;
            return Err(ExtensionHostError::Handshake(
                "extension handshake metadata or executable digest is invalid".into(),
            ));
        }
        write_message(
            &mut stdin,
            &Message::Describe {
                id: "describe-0".into(),
            },
            limits.max_frame_bytes,
        )
        .await?;
        let description = timeout_frame(&mut stdout, &limits, "extension description").await?;
        let Message::Description { id, descriptor } = description else {
            terminate_process(process).await;
            return Err(ExtensionHostError::Handshake("expected description".into()));
        };
        if id != "describe-0" {
            terminate_process(process).await;
            return Err(ExtensionHostError::Correlation {
                expected: "describe-0".into(),
                actual: id,
            });
        }
        let mut registry = crate::extension::registry::DescriptorRegistry::default();
        registry
            .register(&descriptor)
            .map_err(|error| ExtensionHostError::Descriptor(error.to_string()))?;
        let diagnostics = Arc::new(Mutex::new(BoundedDiagnostics::new(
            limits.max_diagnostic_bytes,
        )));
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let writer = Arc::new(tokio::sync::Mutex::new(stdin));
        let child = Arc::new(tokio::sync::Mutex::new(Some(child)));
        let failed = Arc::new(AtomicBool::new(false));
        spawn_stderr_reader(stderr, diagnostics.clone());
        spawn_protocol_reader(
            stdout,
            pending.clone(),
            writer.clone(),
            dispatcher.clone(),
            failed.clone(),
            limits.max_frame_bytes,
        );
        let client = Arc::new(Self {
            executable,
            extension_id: hello.extension_id,
            extension_revision: hello.extension_revision,
            sdk_version: hello.sdk_version,
            package_id: hello.package_id,
            platform: hello.platform,
            executable_digest: hello.executable_digest,
            descriptor,
            writer,
            pending,
            sequence: AtomicU64::new(1),
            child,
            process,
            diagnostics,
            failed,
            revision_calls: revision_call_limit(
                &expected_digest,
                limits.max_concurrent_calls_per_revision,
            )?,
            limits,
            dispatcher,
        });
        crate::observability::emit(crate::observability::EventInput {
            level: crate::observability::LogLevel::Info,
            target: "workflow.extension",
            action: "startup",
            operation_id: None,
            parent_operation_id: None,
            branch: None,
            session: None,
            message: format!("started extension {}", client.extension_id),
            data_json: Some(
                serde_json::json!({
                    "extension_id": client.extension_id,
                    "revision": client.extension_revision,
                    "elapsed_ms": i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
                })
                .to_string(),
            ),
        });
        Ok(client)
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }
    pub fn id(&self) -> &str {
        &self.extension_id
    }
    pub fn revision(&self) -> &str {
        &self.extension_revision
    }
    pub fn sdk_version(&self) -> &str {
        &self.sdk_version
    }
    pub fn package_id(&self) -> &str {
        &self.package_id
    }
    pub fn platform(&self) -> &str {
        &self.platform
    }
    pub fn executable_digest(&self) -> &str {
        &self.executable_digest
    }
    pub fn descriptor(&self) -> &ExtensionDescriptor {
        &self.descriptor
    }
    pub fn diagnostics(&self) -> Vec<String> {
        self.diagnostics
            .lock()
            .map(|value| value.lines.iter().cloned().collect())
            .unwrap_or_default()
    }
    pub fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub async fn execute(
        &self,
        attempt: AttemptEnvelope,
        mut cancellation: watch::Receiver<bool>,
    ) -> Result<ExecuteResult, ExtensionHostError> {
        let started = Instant::now();
        let _permits = self.acquire_call_permits().await?;
        let attempt_id = attempt.attempt_id.clone();
        let generation = attempt.generation;
        let (id, mut response) = self
            .begin_call(|id| Message::Execute { id, attempt })
            .await?;
        let result = tokio::select! {
            response = &mut response => decode_execute(flatten_response(response), &id, &attempt_id, generation),
            changed = cancellation.changed() => {
                if changed.is_ok() && !*cancellation.borrow() {
                    decode_execute(flatten_response(response.await), &id, &attempt_id, generation)
                } else {
                    match self.cancel(&attempt_id, generation).await {
                        Err(error) => Err(error),
                        Ok(()) => match tokio::time::timeout(self.limits.cancellation_timeout, response).await {
                            Ok(result) => decode_execute(flatten_response(result), &id, &attempt_id, generation),
                            Err(_) => {
                                self.terminate().await;
                                Err(ExtensionHostError::CancellationTimeout(attempt_id.clone()))
                            }
                        }
                    }
                }
            }
        };
        crate::observability::emit(crate::observability::EventInput {
            level: if result.is_ok() {
                crate::observability::LogLevel::Info
            } else {
                crate::observability::LogLevel::Warn
            },
            target: "workflow.extension",
            action: "call",
            operation_id: None,
            parent_operation_id: None,
            branch: None,
            session: None,
            message: format!(
                "extension call {}",
                if result.is_ok() {
                    "completed"
                } else {
                    "failed"
                }
            ),
            data_json: Some(
                serde_json::json!({
                    "extension_id": self.extension_id,
                    "revision": self.extension_revision,
                    "attempt_id": attempt_id,
                    "generation": generation,
                    "elapsed_ms": i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
                    "succeeded": result.is_ok(),
                })
                .to_string(),
            ),
        });
        result
    }

    pub async fn heartbeat(&self) -> Result<(), ExtensionHostError> {
        let response = self
            .call_with_timeout(self.limits.heartbeat_timeout, |id| Message::Ping { id })
            .await?;
        if matches!(response, Message::Pong { .. }) {
            Ok(())
        } else {
            Err(ExtensionHostError::Unexpected(response.kind().into()))
        }
    }

    pub async fn invoke_trigger(
        &self,
        adapter_id: &str,
        input: Value,
    ) -> Result<Value, ExtensionHostError> {
        match self
            .call(|id| Message::InvokeTrigger {
                id,
                adapter_id: adapter_id.into(),
                input,
            })
            .await?
        {
            Message::TriggerResult { result, .. } => result.map_err(ExtensionHostError::Remote),
            other => Err(ExtensionHostError::Unexpected(other.kind().into())),
        }
    }

    pub async fn send_notification(
        &self,
        channel_id: &str,
        notification: Value,
    ) -> Result<Value, ExtensionHostError> {
        match self
            .call(|id| Message::SendNotification {
                id,
                channel_id: channel_id.into(),
                notification,
            })
            .await?
        {
            Message::NotificationResult { result, .. } => {
                result.map_err(ExtensionHostError::Remote)
            }
            other => Err(ExtensionHostError::Unexpected(other.kind().into())),
        }
    }

    pub async fn suggest_input(
        &self,
        schema_id: &str,
        context: Value,
    ) -> Result<Vec<Value>, ExtensionHostError> {
        match self
            .call(|id| Message::SuggestInput {
                id,
                schema_id: schema_id.into(),
                context,
            })
            .await?
        {
            Message::InputSuggestions { result, .. } => result.map_err(ExtensionHostError::Remote),
            other => Err(ExtensionHostError::Unexpected(other.kind().into())),
        }
    }

    pub async fn validate_input(
        &self,
        schema_id: &str,
        value: Value,
    ) -> Result<(), ExtensionHostError> {
        match self
            .call(|id| Message::ValidateInput {
                id,
                schema_id: schema_id.into(),
                value,
            })
            .await?
        {
            Message::InputValidation { result, .. } => result.map_err(ExtensionHostError::Remote),
            other => Err(ExtensionHostError::Unexpected(other.kind().into())),
        }
    }

    pub async fn render_artifact(
        &self,
        schema_id: &str,
        value: Value,
        width: u16,
    ) -> Result<StructuredRender, ExtensionHostError> {
        match self
            .call(|id| Message::RenderArtifact {
                id,
                schema_id: schema_id.into(),
                value,
                width,
            })
            .await?
        {
            Message::ArtifactRender { result, .. } => result.map_err(ExtensionHostError::Remote),
            other => Err(ExtensionHostError::Unexpected(other.kind().into())),
        }
    }

    pub async fn shutdown(&self) -> Result<(), ExtensionHostError> {
        if !self.failed.load(Ordering::Acquire) {
            let result = tokio::time::timeout(
                self.limits.shutdown_timeout,
                self.call(|id| Message::Shutdown { id }),
            )
            .await;
            if let Ok(Ok(Message::ShutdownAck { .. })) = result {
                if let Some(mut child) = self.child.lock().await.take() {
                    let _ = child.wait().await;
                }
                return Ok(());
            }
        }
        self.terminate().await;
        Ok(())
    }

    pub async fn restart(&self) -> Result<Arc<Self>, ExtensionHostError> {
        self.terminate().await;
        Self::launch(
            &self.executable,
            self.dispatcher.clone(),
            self.limits.clone(),
        )
        .await
    }

    async fn cancel(&self, attempt_id: &str, generation: u64) -> Result<(), ExtensionHostError> {
        let expected_attempt = attempt_id.to_owned();
        let response = self
            .call_without_permit(self.limits.request_timeout, |id| Message::Cancel {
                id,
                attempt_id: attempt_id.into(),
                generation,
            })
            .await?;
        match response {
            Message::Cancelled {
                attempt_id,
                generation: actual_generation,
                ..
            } if attempt_id == expected_attempt && actual_generation == generation => Ok(()),
            other => Err(ExtensionHostError::Unexpected(other.kind().into())),
        }
    }

    async fn call(
        &self,
        message: impl FnOnce(String) -> Message,
    ) -> Result<Message, ExtensionHostError> {
        self.call_with_timeout(self.limits.request_timeout, message)
            .await
    }

    async fn call_with_timeout(
        &self,
        timeout: Duration,
        message: impl FnOnce(String) -> Message,
    ) -> Result<Message, ExtensionHostError> {
        let _permits = self.acquire_call_permits().await?;
        self.call_without_permit(timeout, message).await
    }

    async fn call_without_permit(
        &self,
        timeout: Duration,
        message: impl FnOnce(String) -> Message,
    ) -> Result<Message, ExtensionHostError> {
        let (id, response) = self.begin_call(message).await?;
        let response = flatten_response(
            tokio::time::timeout(timeout, response)
                .await
                .map_err(|_| ExtensionHostError::Timeout(id.clone()))?,
        )?;
        validate_response_id(&id, &response)?;
        Ok(response)
    }

    async fn begin_call(
        &self,
        message: impl FnOnce(String) -> Message,
    ) -> Result<
        (
            String,
            oneshot::Receiver<Result<Message, ExtensionHostError>>,
        ),
        ExtensionHostError,
    > {
        if self.failed.load(Ordering::Acquire) {
            return Err(ExtensionHostError::Crashed);
        }
        let id = format!("host-{}", self.sequence.fetch_add(1, Ordering::Relaxed));
        let (send, receive) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| ExtensionHostError::Poisoned)?
            .insert(id.clone(), send);
        let write = write_message(
            &mut *self.writer.lock().await,
            &message(id.clone()),
            self.limits.max_frame_bytes,
        )
        .await;
        if let Err(error) = write {
            self.pending
                .lock()
                .ok()
                .and_then(|mut pending| pending.remove(&id));
            return Err(error);
        }
        Ok((id, receive))
    }

    async fn acquire_call_permits(
        &self,
    ) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit), ExtensionHostError> {
        let global = GLOBAL_EXTENSION_CALLS
            .get_or_init(|| Arc::new(Semaphore::new(GLOBAL_EXTENSION_CALL_LIMIT)))
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ExtensionHostError::Crashed)?;
        let revision = self
            .revision_calls
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ExtensionHostError::Crashed)?;
        Ok((global, revision))
    }

    async fn terminate(&self) {
        self.failed.store(true, Ordering::Release);
        terminate_process(self.process).await;
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.wait().await;
        }
        fail_pending(&self.pending, ExtensionHostError::Crashed);
    }
}

fn spawn_protocol_reader<R: AsyncBufRead + Unpin + Send + 'static>(
    mut reader: R,
    pending: Arc<Mutex<HashMap<String, ResponseSender>>>,
    writer: Arc<tokio::sync::Mutex<ChildStdin>>,
    dispatcher: Arc<dyn HostDispatcher>,
    failed: Arc<AtomicBool>,
    max_frame_bytes: usize,
) {
    tokio::spawn(async move {
        loop {
            let message = match read_message(&mut reader, max_frame_bytes).await {
                Ok(Some(message)) => message,
                Ok(None) => break,
                Err(error) => {
                    fail_pending(&pending, error);
                    break;
                }
            };
            if let Message::HostRequest {
                id,
                attempt_id,
                generation,
                operation,
            } = message
            {
                // Host operations may be as long-lived as the enclosing Workflow Step. Dispatch
                // them independently so the protocol reader can continue servicing heartbeats,
                // cancellation, and unrelated correlated responses.
                let dispatcher = dispatcher.clone();
                let writer = writer.clone();
                let pending = pending.clone();
                tokio::spawn(async move {
                    let result = dispatcher
                        .dispatch(&attempt_id, generation, operation)
                        .await;
                    if let Err(error) = write_message(
                        &mut *writer.lock().await,
                        &Message::HostResponse { id, result },
                        max_frame_bytes,
                    )
                    .await
                    {
                        fail_pending(&pending, error);
                    }
                });
                continue;
            }
            let Some(id) = message.correlation_id().map(str::to_owned) else {
                fail_pending(
                    &pending,
                    ExtensionHostError::Unexpected(message.kind().into()),
                );
                break;
            };
            if let Some(sender) = pending
                .lock()
                .ok()
                .and_then(|mut pending| pending.remove(&id))
            {
                let _ = sender.send(Ok(message));
            } else {
                fail_pending(
                    &pending,
                    ExtensionHostError::Unexpected(format!(
                        "response for unknown correlation id '{id}'"
                    )),
                );
                break;
            }
        }
        failed.store(true, Ordering::Release);
        fail_pending(&pending, ExtensionHostError::Crashed);
    });
}

fn spawn_stderr_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    reader: R,
    diagnostics: Arc<Mutex<BoundedDiagnostics>>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(mut diagnostics) = diagnostics.lock() {
                diagnostics.push(line);
            }
        }
    });
}

async fn timeout_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limits: &HostLimits,
    description: &str,
) -> Result<Message, ExtensionHostError> {
    tokio::time::timeout(
        limits.handshake_timeout,
        read_message(reader, limits.max_frame_bytes),
    )
    .await
    .map_err(|_| ExtensionHostError::Timeout(description.into()))??
    .ok_or(ExtensionHostError::Crashed)
}

async fn read_message<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    maximum: usize,
) -> Result<Option<Message>, ExtensionHostError> {
    let mut frame = Vec::new();
    loop {
        let buffer = reader
            .fill_buf()
            .await
            .map_err(|error| ExtensionHostError::Io(error.to_string()))?;
        if buffer.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            return Err(ExtensionHostError::Malformed(
                "unterminated JSON frame".into(),
            ));
        }
        let count = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |index| index + 1);
        if frame.len().saturating_add(count) > maximum {
            return Err(ExtensionHostError::OversizedFrame(maximum));
        }
        frame.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if frame.last() == Some(&b'\n') {
            break;
        }
    }
    frame.pop();
    if frame.last() == Some(&b'\r') {
        frame.pop();
    }
    let message = serde_json::from_slice(&frame)
        .map_err(|error| ExtensionHostError::Malformed(error.to_string()))?;
    Ok(Some(message))
}

async fn write_message(
    writer: &mut ChildStdin,
    message: &Message,
    maximum: usize,
) -> Result<(), ExtensionHostError> {
    let frame = prism_extension_protocol::encode_frame(message, maximum).map_err(|error| {
        if error.code == "oversized_frame" {
            ExtensionHostError::OversizedFrame(maximum)
        } else {
            ExtensionHostError::Malformed(error.message)
        }
    })?;
    writer
        .write_all(&frame)
        .await
        .map_err(|error| ExtensionHostError::Io(error.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|error| ExtensionHostError::Io(error.to_string()))
}

fn decode_execute(
    response: Result<Message, ExtensionHostError>,
    id: &str,
    attempt_id: &str,
    generation: u64,
) -> Result<ExecuteResult, ExtensionHostError> {
    let response = response?;
    validate_response_id(id, &response)?;
    match response {
        Message::ExecuteResult { result, .. }
            if result.attempt_id == attempt_id && result.generation == generation =>
        {
            Ok(result)
        }
        Message::Error { error, .. } => Err(ExtensionHostError::Remote(error)),
        other => Err(ExtensionHostError::Unexpected(other.kind().into())),
    }
}

fn flatten_response(
    response: Result<Result<Message, ExtensionHostError>, oneshot::error::RecvError>,
) -> Result<Message, ExtensionHostError> {
    response.map_err(|_| ExtensionHostError::Crashed)?
}

fn validate_response_id(expected: &str, response: &Message) -> Result<(), ExtensionHostError> {
    let actual = response.correlation_id().unwrap_or_default();
    if actual == expected {
        Ok(())
    } else {
        Err(ExtensionHostError::Correlation {
            expected: expected.into(),
            actual: actual.into(),
        })
    }
}

fn validate_features(features: &[String]) -> Result<(), ExtensionHostError> {
    if let Some(feature) = features
        .iter()
        .find(|feature| !HOST_FEATURES.contains(&feature.as_str()))
    {
        return Err(ExtensionHostError::Handshake(format!(
            "extension selected unknown feature '{feature}'"
        )));
    }
    Ok(())
}

fn validate_negotiated_version(version: ProtocolVersion) -> Result<(), ExtensionHostError> {
    if version.major != PROTOCOL_MAJOR || version.minor > PROTOCOL_MINOR {
        Err(ExtensionHostError::IncompatibleVersion(version))
    } else {
        Ok(())
    }
}

fn validate_limits(limits: &HostLimits) -> Result<(), ExtensionHostError> {
    if limits.max_frame_bytes == 0
        || limits.max_diagnostic_bytes == 0
        || limits.heartbeat_interval.is_zero()
        || limits.heartbeat_timeout.is_zero()
        || limits.max_concurrent_calls_per_revision == 0
    {
        return Err(ExtensionHostError::Configuration(
            "host byte limits must be positive".into(),
        ));
    }
    Ok(())
}

fn revision_call_limit(
    revision: &str,
    maximum: usize,
) -> Result<Arc<Semaphore>, ExtensionHostError> {
    let mut revisions = REVISION_EXTENSION_CALLS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| ExtensionHostError::Poisoned)?;
    if let Some((configured, existing)) = revisions.get(revision)
        && let Some(existing) = existing.upgrade()
    {
        if *configured != maximum {
            return Err(ExtensionHostError::Handshake(format!(
                "extension revision {revision} was launched with incompatible concurrency limits"
            )));
        }
        return Ok(existing);
    }
    let semaphore = Arc::new(Semaphore::new(maximum));
    revisions.insert(revision.into(), (maximum, Arc::downgrade(&semaphore)));
    Ok(semaphore)
}

fn fail_pending(pending: &Mutex<HashMap<String, ResponseSender>>, error: ExtensionHostError) {
    if let Ok(mut pending) = pending.lock() {
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(error.clone()));
        }
    }
}

async fn terminate_process(process: crate::process::RecordedProcess) {
    let _ = tokio::task::spawn_blocking(move || {
        crate::process::terminate_recorded_process(process, Duration::from_secs(2))
    })
    .await;
}

struct BoundedDiagnostics {
    lines: VecDeque<String>,
    bytes: usize,
    maximum: usize,
}
impl BoundedDiagnostics {
    fn new(maximum: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
            maximum,
        }
    }
    fn push(&mut self, mut line: String) {
        if line.len() > self.maximum {
            line.truncate(self.maximum);
        }
        self.bytes = self.bytes.saturating_add(line.len());
        self.lines.push_back(line);
        while self.bytes > self.maximum {
            if let Some(removed) = self.lines.pop_front() {
                self.bytes = self.bytes.saturating_sub(removed.len());
            } else {
                break;
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum ExtensionHostError {
    Configuration(String),
    Spawn(PathBuf, String),
    Process(String),
    Io(String),
    Handshake(String),
    IncompatibleVersion(ProtocolVersion),
    Descriptor(String),
    Malformed(String),
    OversizedFrame(usize),
    Correlation { expected: String, actual: String },
    Unexpected(String),
    Timeout(String),
    CancellationTimeout(String),
    Remote(ProtocolError),
    Crashed,
    Poisoned,
}

impl fmt::Display for ExtensionHostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(value) => {
                write!(f, "invalid extension host configuration: {value}")
            }
            Self::Spawn(path, value) => write!(f, "start extension {}: {value}", path.display()),
            Self::Process(value) => write!(f, "observe extension process: {value}"),
            Self::Io(value) => write!(f, "extension transport: {value}"),
            Self::Handshake(value) => write!(f, "extension handshake: {value}"),
            Self::IncompatibleVersion(value) => write!(
                f,
                "incompatible extension protocol {}.{}",
                value.major, value.minor
            ),
            Self::Descriptor(value) => write!(f, "invalid extension description: {value}"),
            Self::Malformed(value) => write!(f, "malformed extension frame: {value}"),
            Self::OversizedFrame(maximum) => write!(f, "extension frame exceeds {maximum} bytes"),
            Self::Correlation { expected, actual } => write!(
                f,
                "extension correlation mismatch: expected {expected}, got {actual}"
            ),
            Self::Unexpected(value) => write!(f, "unexpected extension message: {value}"),
            Self::Timeout(value) => write!(f, "extension timed out waiting for {value}"),
            Self::CancellationTimeout(value) => {
                write!(f, "extension did not cancel attempt {value}")
            }
            Self::Remote(value) => write!(f, "extension error {}: {}", value.code, value.message),
            Self::Crashed => write!(f, "extension process exited"),
            Self::Poisoned => write!(f, "extension host state is poisoned"),
        }
    }
}
impl std::error::Error for ExtensionHostError {}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn rejects_oversized_frame_before_decoding() {
        let bytes = vec![b'x'; 20];
        let mut reader = BufReader::new(bytes.as_slice());
        assert!(matches!(
            read_message(&mut reader, 10).await,
            Err(ExtensionHostError::OversizedFrame(10))
        ));
    }

    #[tokio::test]
    async fn rejects_malformed_protocol_stdout() {
        let mut reader = BufReader::new(b"not-json\n".as_slice());
        assert!(matches!(
            read_message(&mut reader, 100).await,
            Err(ExtensionHostError::Malformed(_))
        ));
    }

    #[test]
    fn negotiates_minor_versions_and_rejects_unknown_features() {
        assert!(validate_negotiated_version(ProtocolVersion::CURRENT).is_ok());
        assert!(validate_negotiated_version(ProtocolVersion { major: 2, minor: 0 }).is_err());
        assert!(validate_negotiated_version(ProtocolVersion { major: 1, minor: 2 }).is_err());
        assert!(validate_features(&["future.required".into()]).is_err());
    }

    #[test]
    fn diagnostics_are_bounded() {
        let mut diagnostics = BoundedDiagnostics::new(5);
        diagnostics.push("abc".into());
        diagnostics.push("def".into());
        assert_eq!(
            diagnostics.lines.into_iter().collect::<Vec<_>>(),
            vec!["def"]
        );
    }

    #[test]
    fn all_six_classes_are_wire_values() {
        use prism_extension_protocol::StepClass;
        let classes = [
            StepClass::Action,
            StepClass::Gate,
            StepClass::Approval,
            StepClass::Wait,
            StepClass::Notification,
            StepClass::WorkflowCall,
        ];
        assert_eq!(classes.len(), 6);
    }

    #[test]
    fn outcome_has_no_internal_runtime_types() {
        let outcome = prism_extension_protocol::AttemptOutcome::Cancelled;
        assert_eq!(
            serde_json::to_string(&outcome).unwrap(),
            r#"{"status":"cancelled"}"#
        );
    }
}
