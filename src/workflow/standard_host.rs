//! Production host operations used by the Standard Extension.
//!
//! Extensions receive opaque references only. This module resolves them against the claimed
//! Attempt and keeps paths, credentials, adapters, and database handles on the Prism side.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prism_extension_protocol::{
    AgentRequest, ArtifactReference, BrokeredEffectRequest, HostOperation, OpaqueReference,
    ProcessRequest, ProtocolError,
};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::extension::{BrokerFuture, HostFuture, HostOperationServices, ProtectedEffectBackend};
use crate::persistence::pools::WorkflowDatabase;
use crate::workflow::effect::ProtectedEffectKind;

const MAX_HOST_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_HOST_TIMEOUT_MS: u64 = 3_600_000;

#[derive(Clone)]
pub(crate) struct StandardHostServices {
    database: WorkflowDatabase,
}

#[derive(Clone)]
struct ClaimedAttempt {
    repository: PathBuf,
    worker_id: String,
    target_id: String,
    fencing_token: i64,
}

struct ChildPolicy {
    stdin: Option<String>,
    timeout_ms: u64,
    maximum: u64,
}

impl StandardHostServices {
    pub(crate) fn new(database: WorkflowDatabase) -> Self {
        Self { database }
    }

    async fn claimed_attempt(
        &self,
        attempt_id: &str,
        generation: u64,
    ) -> Result<ClaimedAttempt, ProtocolError> {
        let generation = i64::try_from(generation)
            .map_err(|_| ProtocolError::new("invalid_generation", "generation is too large"))?;
        let row = self
            .database
            .claimed_host_attempt(attempt_id)
            .await
            .map_err(|error| ProtocolError::new("workflow_store", error.to_string()))?;
        let Some((repository, worker_id, target_id, fencing_token, lease_expires)) = row else {
            return Err(ProtocolError::new(
                "stale_attempt",
                "Attempt is not actively claimed",
            ));
        };
        if fencing_token != generation || lease_expires <= unix_ms() {
            return Err(ProtocolError::new(
                "stale_generation",
                "Attempt lease or generation is no longer current",
            ));
        }
        let repository = repository.ok_or_else(|| {
            ProtocolError::new(
                "repository_unavailable",
                "workflow Attempt is not associated with a Repository",
            )
        })?;
        Ok(ClaimedAttempt {
            repository: repository.into(),
            worker_id,
            target_id,
            fencing_token,
        })
    }

    async fn attempt_repository(&self, attempt_id: &str) -> Result<String, ProtocolError> {
        self.database
            .attempt_repository(attempt_id)
            .await
            .map_err(|error| ProtocolError::new("workflow_store", error.to_string()))?
            .ok_or_else(|| {
                ProtocolError::new(
                    "repository_unavailable",
                    "workflow Attempt is not associated with a Repository",
                )
            })
    }

    async fn record_process(
        &self,
        attempt_id: &str,
        attempt: &ClaimedAttempt,
        process: crate::process::RecordedProcess,
    ) -> Result<(), ProtocolError> {
        let start = process
            .identity
            .map(|identity| i64::try_from(identity.stored_value()))
            .transpose()
            .map_err(|_| ProtocolError::new("process_identity", "process identity is too large"))?;
        let changed = self
            .database
            .record_host_process(
                attempt_id,
                &attempt.worker_id,
                &attempt.target_id,
                attempt.fencing_token,
                (process.pid, start),
                unix_ms(),
            )
            .await
            .map_err(|error| ProtocolError::new("workflow_store", error.to_string()))?;
        if changed {
            Ok(())
        } else {
            Err(ProtocolError::new(
                "stale_attempt",
                "Attempt lost its lease before process ownership was recorded",
            ))
        }
    }

    async fn run_process(
        &self,
        attempt_id: &str,
        generation: u64,
        request: ProcessRequest,
    ) -> Result<Value, ProtocolError> {
        validate_launch_limits(request.timeout_ms, request.max_output_bytes)?;
        let attempt = self.claimed_attempt(attempt_id, generation).await?;
        let worktree = resolve_worktree(&attempt.repository, &request.working_scope)?;
        let mut command = Command::new(&request.executable);
        command.as_std_mut().process_group(0);
        command
            .args(&request.arguments)
            .envs(&request.environment)
            .current_dir(&worktree)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        self.run_child(
            attempt_id,
            generation,
            &attempt,
            command,
            request.timeout_ms,
            request.max_output_bytes,
        )
        .await
    }

    async fn run_agent(
        &self,
        attempt_id: &str,
        generation: u64,
        request: AgentRequest,
    ) -> Result<Value, ProtocolError> {
        validate_launch_limits(request.timeout_ms, request.max_output_bytes)?;
        if request.prompt.trim().is_empty() {
            return Err(ProtocolError::new(
                "invalid_agent_request",
                "Agent prompt is empty",
            ));
        }
        if request.continuation.is_some() {
            return Err(ProtocolError::new(
                "unsupported_continuation",
                "production workflow Agents do not resume interactive sessions",
            ));
        }
        let attempt = self.claimed_attempt(attempt_id, generation).await?;
        let worktree = resolve_worktree(&attempt.repository, &request.working_scope)?;
        let repository = crate::repo::Repository {
            root: attempt.repository.clone(),
        };
        let config = crate::config::Config::load(&repository);
        if !config.config_errors.is_empty() {
            return Err(ProtocolError::new(
                "invalid_configuration",
                config.config_errors.join("; "),
            ));
        }
        let harness_id = if request.harness == "default" {
            config.default_harness.as_str()
        } else {
            request.harness.as_str()
        };
        let harness_config = config
            .harness_config(harness_id)
            .map_err(|error| ProtocolError::new("harness_configuration", error))?;
        let invocation = crate::harness::Harness::new(harness_id, &harness_config)
            .headless(
                &request.prompt,
                &worktree,
                "Prism workflow Agent",
                None,
                request.model.as_deref(),
                false,
            )
            .map_err(|error| ProtocolError::new("harness_invocation", error))?;
        let (program, arguments) = invocation.argv.split_first().ok_or_else(|| {
            ProtocolError::new("harness_invocation", "harness invocation is empty")
        })?;
        let mut command = Command::new(program);
        command.as_std_mut().process_group(0);
        command
            .args(arguments)
            .envs(&invocation.environment)
            .current_dir(&worktree)
            .stdin(if invocation.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let stdin = invocation.stdin.clone();
        let result = self
            .run_child_with_stdin(
                attempt_id,
                generation,
                &attempt,
                command,
                ChildPolicy {
                    stdin,
                    timeout_ms: request.timeout_ms,
                    maximum: request.max_output_bytes,
                },
            )
            .await;
        invocation.cleanup();
        let result = result?;
        let stdout = result
            .get("stdout")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let structured = extract_structured_agent_result(stdout);
        let repair = request.prompt.starts_with("agent-repair-");
        if repair && structured.is_none() {
            return Err(ProtocolError::new(
                "malformed_agent_output",
                "repair Agent did not emit a structured JSON result",
            ));
        }
        let mut output = structured.unwrap_or_else(|| serde_json::json!({"summary": stdout}));
        if let Some(object) = output.as_object_mut() {
            object.insert("process".into(), result);
        }
        Ok(output)
    }

    async fn run_child(
        &self,
        attempt_id: &str,
        generation: u64,
        attempt: &ClaimedAttempt,
        command: Command,
        timeout_ms: u64,
        maximum: u64,
    ) -> Result<Value, ProtocolError> {
        self.run_child_with_stdin(
            attempt_id,
            generation,
            attempt,
            command,
            ChildPolicy {
                stdin: None,
                timeout_ms,
                maximum,
            },
        )
        .await
    }

    async fn run_child_with_stdin(
        &self,
        attempt_id: &str,
        generation: u64,
        attempt: &ClaimedAttempt,
        mut command: Command,
        policy: ChildPolicy,
    ) -> Result<Value, ProtocolError> {
        let ChildPolicy {
            stdin,
            timeout_ms,
            maximum,
        } = policy;
        let mut child = command
            .spawn()
            .map_err(|error| ProtocolError::new("process_spawn", error.to_string()))?;
        if let Some(stdin) = stdin
            && let Some(mut writer) = child.stdin.take()
        {
            writer
                .write_all(stdin.as_bytes())
                .await
                .map_err(|error| ProtocolError::new("process_stdin", error.to_string()))?;
        }
        let pid = child
            .id()
            .ok_or_else(|| ProtocolError::new("process_identity", "process has no PID"))?;
        let recorded = tokio::task::spawn_blocking(move || crate::process::record_process(pid))
            .await
            .map_err(|error| ProtocolError::new("process_identity", error.to_string()))?
            .map_err(|error| ProtocolError::new("process_identity", error.to_string()))?;
        // This durable write happens before any output or exit status can affect the Run Ledger.
        if let Err(error) = self.record_process(attempt_id, attempt, recorded).await {
            terminate(recorded).await;
            return Err(error);
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProtocolError::new("process_output", "stdout is unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ProtocolError::new("process_output", "stderr is unavailable"))?;
        let stdout_task = tokio::spawn(read_bounded(stdout, maximum));
        let stderr_task = tokio::spawn(read_bounded(stderr, maximum));
        let deadline = tokio::time::sleep(Duration::from_millis(timeout_ms));
        tokio::pin!(deadline);
        let status = loop {
            tokio::select! {
                status = child.wait() => break status.map_err(|error| ProtocolError::new("process_wait", error.to_string()))?,
                _ = &mut deadline => {
                    terminate(recorded).await;
                    let _ = child.wait().await;
                    return Err(ProtocolError::new("process_timeout", format!("process exceeded {timeout_ms}ms")));
                }
                _ = tokio::time::sleep(Duration::from_millis(200)) => {
                    if self.claimed_attempt(attempt_id, generation).await.is_err() {
                        terminate(recorded).await;
                        let _ = child.wait().await;
                        return Err(ProtocolError::new("process_cancelled", "Attempt was cancelled or lost its lease"));
                    }
                }
            }
        };
        let stdout = join_output(stdout_task).await?;
        let stderr = join_output(stderr_task).await?;
        if stdout.len().saturating_add(stderr.len())
            > usize::try_from(maximum).unwrap_or(usize::MAX)
        {
            return Err(ProtocolError::new(
                "output_limit",
                "combined process output exceeded its bound",
            ));
        }
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        let stderr = String::from_utf8_lossy(&stderr).into_owned();
        if !status.success() {
            return Err(ProtocolError::new(
                "process_failed",
                format!(
                    "process exited with {status}: {}",
                    truncate_message(&stderr)
                ),
            ));
        }
        Ok(serde_json::json!({"exit_code":status.code(),"stdout":stdout,"stderr":stderr}))
    }

    fn unavailable(operation: &HostOperation) -> ProtocolError {
        ProtocolError::new(
            "operation_unavailable",
            format!("Standard host operation is not implemented: {operation:?}"),
        )
    }
}

impl HostOperationServices for StandardHostServices {
    fn read_artifact<'a>(
        &'a self,
        attempt_id: &'a str,
        generation: u64,
        artifact: ArtifactReference,
    ) -> HostFuture<'a> {
        Box::pin(async move {
            self.claimed_attempt(attempt_id, generation).await?;
            let revision = i64::try_from(artifact.revision).map_err(|_| {
                ProtocolError::new("invalid_artifact", "Artifact revision is too large")
            })?;
            let row = self
                .database
                .read_attempt_artifact(
                    attempt_id,
                    &artifact.id,
                    revision,
                    &artifact.digest,
                    &artifact.schema,
                )
                .await
                .map_err(|error| ProtocolError::new("workflow_store", error.to_string()))?;
            let Some((_digest, size, inline, file)) = row else {
                return Err(ProtocolError::new(
                    "artifact_mismatch",
                    "Artifact ID, revision, digest, or Run does not match",
                ));
            };
            let bytes = match (inline, file) {
                (Some(bytes), None) => bytes,
                (None, Some(path)) => tokio::fs::read(path)
                    .await
                    .map_err(|error| ProtocolError::new("artifact_read", error.to_string()))?,
                _ => {
                    return Err(ProtocolError::new(
                        "artifact_corrupt",
                        "Artifact has an invalid body representation",
                    ));
                }
            };
            if i64::try_from(bytes.len()).ok() != Some(size) {
                return Err(ProtocolError::new(
                    "artifact_corrupt",
                    "Artifact size does not match its body",
                ));
            }
            serde_json::from_slice(&bytes)
                .or_else(|_| Ok(Value::String(String::from_utf8_lossy(&bytes).into_owned())))
        })
    }

    fn trace_process<'a>(
        &'a self,
        _attempt_id: &'a str,
        _generation: u64,
        pid: u32,
        identity: Option<String>,
    ) -> HostFuture<'a> {
        Box::pin(async move { Ok(serde_json::json!({"pid":pid,"identity":identity})) })
    }

    fn trace_agent<'a>(
        &'a self,
        _attempt_id: &'a str,
        _generation: u64,
        session_id: String,
        metadata: Value,
    ) -> HostFuture<'a> {
        Box::pin(
            async move { Ok(serde_json::json!({"session_id":session_id,"metadata":metadata})) },
        )
    }

    fn standard_operation<'a>(
        &'a self,
        attempt_id: &'a str,
        generation: u64,
        operation: HostOperation,
    ) -> HostFuture<'a> {
        Box::pin(async move {
            match operation {
                HostOperation::RunProcess { request } => {
                    self.run_process(attempt_id, generation, request).await
                }
                HostOperation::RunAgent { request } => {
                    self.run_agent(attempt_id, generation, request).await
                }
                HostOperation::ObserveProvider { request } => {
                    let attempt = self.claimed_attempt(attempt_id, generation).await?;
                    let subject_id = request.subject.id;
                    let expected_head = request.subject.revision;
                    let provider_operation = request.operation;
                    tokio::task::spawn_blocking(move || {
                        let repository = crate::repo::Repository {
                            root: attempt.repository,
                        };
                        let config = crate::config::Config::load(&repository);
                        if !config.config_errors.is_empty() {
                            return Err(ProtocolError::new(
                                "invalid_configuration",
                                config.config_errors.join("; "),
                            ));
                        }
                        crate::remote::dispatcher::observe_workflow_change_request(
                            &repository.root,
                            &config,
                            &subject_id,
                            &expected_head,
                            &provider_operation,
                        )
                        .map_err(|error| ProtocolError::new("provider_observation", error))
                    })
                    .await
                    .map_err(|error| {
                        ProtocolError::new("provider_observation", error.to_string())
                    })?
                }
                operation => Err(Self::unavailable(&operation)),
            }
        })
    }
}

#[derive(Clone)]
pub(crate) struct StandardProtectedEffects {
    services: StandardHostServices,
}

impl StandardProtectedEffects {
    pub(crate) fn new(database: WorkflowDatabase) -> Self {
        Self {
            services: StandardHostServices::new(database),
        }
    }
}

impl ProtectedEffectBackend for StandardProtectedEffects {
    fn dispatch<'a>(
        &'a self,
        attempt_id: &'a str,
        kind: ProtectedEffectKind,
        request: BrokeredEffectRequest,
    ) -> BrokerFuture<'a, Value> {
        Box::pin(async move {
            let repository = self.services.attempt_repository(attempt_id).await?;
            tokio::task::spawn_blocking(move || {
                dispatch_protected_effect(Path::new(&repository), kind, request)
            })
            .await
            .map_err(|error| ProtocolError::new("protected_effect", error.to_string()))?
        })
    }
}

fn dispatch_protected_effect(
    repository_path: &Path,
    kind: ProtectedEffectKind,
    request: BrokeredEffectRequest,
) -> Result<Value, ProtocolError> {
    let repository = crate::repo::Repository {
        root: repository_path.into(),
    };
    let config = crate::config::Config::load(&repository);
    if !config.config_errors.is_empty() {
        return Err(ProtocolError::new(
            "invalid_configuration",
            config.config_errors.join("; "),
        ));
    }
    match kind {
        ProtectedEffectKind::Commit => {
            let worktree = effect_worktree(&repository.root, &request)?;
            ensure_head(
                &worktree,
                &config,
                request.preconditions.expected_head.as_deref(),
            )?;
            let previous_head = request
                .preconditions
                .expected_head
                .clone()
                .unwrap_or_default();
            let message = request
                .parameters
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Apply Prism workflow repair");
            run_git(&worktree, &config, &["diff", "--check"])?;
            run_git(&worktree, &config, &["add", "--all"])?;
            let status = git_capture(&worktree, &config, &["status", "--porcelain"])?;
            if status.trim().is_empty() {
                return Err(ProtocolError::new(
                    "clean_worktree",
                    "repair produced no changes to commit",
                ));
            }
            run_git(&worktree, &config, &["commit", "-m", message])?;
            let head =
                crate::git::current_head_sha(&worktree, &config).map_err(effect_error("commit"))?;
            Ok(serde_json::json!({"head":head,"previous_head":previous_head,"status":"committed"}))
        }
        ProtectedEffectKind::Push => {
            let worktree = effect_worktree(&repository.root, &request)?;
            let head = request
                .preconditions
                .expected_head
                .as_deref()
                .ok_or_else(|| {
                    ProtocolError::new("invalid_effect", "push requires a local head")
                })?;
            ensure_head(&worktree, &config, Some(head))?;
            let branch = request
                .parameters
                .get("branch")
                .and_then(Value::as_str)
                .or_else(|| {
                    request
                        .parameters
                        .get("expected_branch")
                        .and_then(Value::as_str)
                })
                .ok_or_else(|| {
                    ProtocolError::new("invalid_effect", "push requires an exact branch")
                })?;
            let current = crate::git::current_branch_name(&worktree, &config)
                .map_err(effect_error("push"))?;
            if current.as_deref() != Some(branch) {
                return Err(ProtocolError::new(
                    "stale_branch",
                    "worktree branch changed before push",
                ));
            }
            let remote = request
                .parameters
                .get("remote")
                .and_then(Value::as_str)
                .unwrap_or("origin");
            let expected_remote = request
                .parameters
                .get("expected_remote_head")
                .and_then(Value::as_str);
            let actual_remote =
                crate::git::push_remote_branch_head_sha(&worktree, remote, branch, &config)
                    .map_err(effect_error("push"))?;
            if expected_remote != actual_remote.as_deref() {
                return Err(ProtocolError::new(
                    "remote_head_drift",
                    "remote branch head changed before push",
                ));
            }
            run_git(
                &worktree,
                &config,
                &["push", remote, &format!("HEAD:refs/heads/{branch}")],
            )?;
            let pushed =
                crate::git::push_remote_branch_head_sha(&worktree, remote, branch, &config)
                    .map_err(effect_error("push"))?;
            if pushed.as_deref() != Some(head) {
                return Err(ProtocolError::new(
                    "push_unconfirmed",
                    "remote did not reach the exact local commit",
                ));
            }
            Ok(serde_json::json!({"head":head,"status":"pushed","remote":remote,"branch":branch}))
        }
        ProtectedEffectKind::ResolveReviewThreads => {
            let subject = request
                .parameters
                .get("change_request")
                .and_then(|value| value.get("id").or(Some(value)))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ProtocolError::new(
                        "invalid_effect",
                        "thread resolution requires a Change Request identity",
                    )
                })?;
            let head = request
                .preconditions
                .expected_head
                .as_deref()
                .ok_or_else(|| {
                    ProtocolError::new("invalid_effect", "thread resolution requires an exact head")
                })?;
            let threads = request
                .parameters
                .get("threads")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    ProtocolError::new("invalid_effect", "thread resolution requires threads")
                })?;
            for thread in threads {
                let id = thread
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ProtocolError::new("invalid_effect", "thread has no ID"))?;
                crate::remote::dispatcher::resolve_workflow_review_thread(
                    &repository.root,
                    &config,
                    subject,
                    head,
                    id,
                )
                .map_err(|error| ProtocolError::new("resolve_review_threads", error))?;
            }
            Ok(
                serde_json::json!({"status":"resolved","thread_ids":threads.iter().filter_map(|thread|thread.get("id")).collect::<Vec<_>>() }),
            )
        }
        ProtectedEffectKind::SquashMerge => {
            let subject = request
                .parameters
                .get("change_request")
                .and_then(|value| value.get("id").or(Some(value)))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ProtocolError::new(
                        "invalid_effect",
                        "squash merge requires a Change Request identity",
                    )
                })?;
            let head = request
                .preconditions
                .expected_head
                .as_deref()
                .ok_or_else(|| {
                    ProtocolError::new("invalid_effect", "squash merge requires an exact head")
                })?;
            crate::remote::dispatcher::merge_workflow_change_request(
                &repository.root,
                &config,
                subject,
                head,
            )
            .map_err(|error| ProtocolError::new("squash_merge", error))
        }
        ProtectedEffectKind::DeleteWorktree => {
            let path = request
                .parameters
                .get("expected_path")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ProtocolError::new("invalid_effect", "worktree deletion requires an exact path")
                })?;
            let branch = request
                .parameters
                .get("branch")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ProtocolError::new("invalid_effect", "worktree deletion requires a branch")
                })?;
            let incarnation = request
                .preconditions
                .worktree_session
                .as_ref()
                .map(|reference| reference.revision.as_str());
            crate::session::delete_worktree_session_if_current(
                &repository,
                &config,
                Path::new(path),
                branch,
                incarnation,
            )
            .map(|_| serde_json::json!({"status":"deleted","path":path}))
            .map_err(|error| ProtocolError::new("delete_worktree", error))
        }
        ProtectedEffectKind::CreateChangeRequest => Err(ProtocolError::new(
            "operation_unavailable",
            "Change Request creation requires a prepared provider guard",
        )),
    }
}

fn resolve_worktree(
    repository: &Path,
    reference: &OpaqueReference,
) -> Result<PathBuf, ProtocolError> {
    if reference.id.trim().is_empty() || reference.revision.trim().is_empty() {
        return Err(ProtocolError::new(
            "invalid_worktree",
            "Worktree Session reference is incomplete",
        ));
    }
    let prefix = format!("{}:", repository.display());
    let path = reference
        .id
        .strip_prefix(&prefix)
        .map(PathBuf::from)
        .ok_or_else(|| {
            ProtocolError::new(
                "invalid_worktree",
                "Worktree Session does not belong to the Attempt Repository",
            )
        })?;
    if !path.is_absolute() || !path.is_dir() {
        return Err(ProtocolError::new(
            "missing_worktree",
            "Worktree Session path is unavailable",
        ));
    }
    let actual = crate::session::worktree_incarnation(&path);
    if actual.is_empty() || actual != reference.revision {
        return Err(ProtocolError::new(
            "replaced_worktree",
            "Worktree Session incarnation was replaced",
        ));
    }
    Ok(path)
}

fn effect_worktree(
    repository: &Path,
    request: &BrokeredEffectRequest,
) -> Result<PathBuf, ProtocolError> {
    resolve_worktree(
        repository,
        request
            .preconditions
            .worktree_session
            .as_ref()
            .ok_or_else(|| {
                ProtocolError::new("invalid_effect", "effect requires a Worktree Session")
            })?,
    )
}

fn ensure_head(
    path: &Path,
    config: &crate::config::Config,
    expected: Option<&str>,
) -> Result<(), ProtocolError> {
    let expected = expected
        .ok_or_else(|| ProtocolError::new("invalid_effect", "effect requires an exact head"))?;
    let actual = crate::git::current_head_sha(path, config).map_err(effect_error("git guard"))?;
    if actual == expected {
        Ok(())
    } else {
        Err(ProtocolError::new(
            "stale_head",
            format!("expected {expected}, found {actual}"),
        ))
    }
}

fn run_git(
    path: &Path,
    config: &crate::config::Config,
    arguments: &[&str],
) -> Result<(), ProtocolError> {
    let output = std::process::Command::new(config.tool("git"))
        .arg("-C")
        .arg(path)
        .args(arguments)
        .output()
        .map_err(|error| ProtocolError::new("git", error.to_string()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(ProtocolError::new(
            "git",
            truncate_message(&String::from_utf8_lossy(&output.stderr)),
        ))
    }
}

fn git_capture(
    path: &Path,
    config: &crate::config::Config,
    arguments: &[&str],
) -> Result<String, ProtocolError> {
    let output = std::process::Command::new(config.tool("git"))
        .arg("-C")
        .arg(path)
        .args(arguments)
        .output()
        .map_err(|error| ProtocolError::new("git", error.to_string()))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(ProtocolError::new(
            "git",
            truncate_message(&String::from_utf8_lossy(&output.stderr)),
        ))
    }
}

fn validate_launch_limits(timeout_ms: u64, maximum: u64) -> Result<(), ProtocolError> {
    if timeout_ms == 0 || timeout_ms > MAX_HOST_TIMEOUT_MS {
        return Err(ProtocolError::new(
            "invalid_timeout",
            format!("timeout must be between 1 and {MAX_HOST_TIMEOUT_MS}ms"),
        ));
    }
    if maximum == 0 || maximum > MAX_HOST_OUTPUT_BYTES {
        return Err(ProtocolError::new(
            "invalid_output_limit",
            format!("output limit must be between 1 and {MAX_HOST_OUTPUT_BYTES} bytes"),
        ));
    }
    Ok(())
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    maximum: u64,
) -> Result<Vec<u8>, ProtocolError> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let count = reader
            .read(&mut chunk)
            .await
            .map_err(|error| ProtocolError::new("process_output", error.to_string()))?;
        if count == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(count) > usize::try_from(maximum).unwrap_or(usize::MAX) {
            return Err(ProtocolError::new(
                "output_limit",
                "process output exceeded its bound",
            ));
        }
        output.extend_from_slice(&chunk[..count]);
    }
}

async fn join_output(
    task: tokio::task::JoinHandle<Result<Vec<u8>, ProtocolError>>,
) -> Result<Vec<u8>, ProtocolError> {
    task.await
        .map_err(|error| ProtocolError::new("process_output", error.to_string()))?
}

async fn terminate(process: crate::process::RecordedProcess) {
    let _ = tokio::task::spawn_blocking(move || {
        crate::process::terminate_recorded_process(process, Duration::from_secs(2))
    })
    .await;
}

fn extract_structured_agent_result(output: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(output.trim())
        && let Some(result) = find_agent_result(&value)
    {
        return Some(result);
    }
    output.lines().rev().find_map(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|value| find_agent_result(&value))
    })
}

fn find_agent_result(value: &Value) -> Option<Value> {
    if value
        .get("addressed_thread_ids")
        .and_then(Value::as_array)
        .is_some()
        || value.get("summary").and_then(Value::as_str).is_some()
        || value.get("candidate").is_some()
    {
        return Some(value.clone());
    }
    match value {
        Value::Object(object) => object.values().find_map(find_agent_result),
        Value::Array(values) => values.iter().rev().find_map(find_agent_result),
        Value::String(text) => serde_json::from_str(text)
            .ok()
            .and_then(|nested| find_agent_result(&nested)),
        _ => None,
    }
}

fn effect_error(label: &'static str) -> impl FnOnce(String) -> ProtocolError {
    move |error| ProtocolError::new(label, error)
}

fn truncate_message(value: &str) -> String {
    value.chars().take(4096).collect()
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub(crate) fn dispatcher(database: WorkflowDatabase) -> Arc<dyn crate::extension::HostDispatcher> {
    Arc::new(crate::extension::AllowlistedHostDispatcher::new(
        StandardHostServices::new(database),
    ))
}
