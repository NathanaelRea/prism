//! Production host operations used by the Standard Extension.
//!
//! The extension receives opaque references only. This module resolves them against the durable
//! Attempt and its Repository before crossing an existing provider or harness seam.

use std::sync::Arc;

use prism_extension_protocol::{
    ArtifactReference, BrokeredEffectRequest, HostOperation, ProtocolError,
};
use serde_json::Value;

use crate::extension::{BrokerFuture, HostFuture, HostOperationServices, ProtectedEffectBackend};
use crate::persistence::pools::WorkflowDatabase;
use crate::workflow::effect::ProtectedEffectKind;

#[derive(Clone)]
pub(crate) struct StandardHostServices {
    database: WorkflowDatabase,
}

impl StandardHostServices {
    pub(crate) fn new(database: WorkflowDatabase) -> Self {
        Self { database }
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
        _attempt_id: &'a str,
        _generation: u64,
        _artifact: ArtifactReference,
    ) -> HostFuture<'a> {
        Box::pin(async {
            Err(ProtocolError::new(
                "operation_unavailable",
                "direct Artifact reads are not implemented",
            ))
        })
    }

    fn trace_process<'a>(
        &'a self,
        _attempt_id: &'a str,
        _generation: u64,
        pid: u32,
        identity: Option<String>,
    ) -> HostFuture<'a> {
        Box::pin(async move { Ok(serde_json::json!({"pid": pid, "identity": identity})) })
    }

    fn trace_agent<'a>(
        &'a self,
        _attempt_id: &'a str,
        _generation: u64,
        session_id: String,
        metadata: Value,
    ) -> HostFuture<'a> {
        Box::pin(
            async move { Ok(serde_json::json!({"session_id": session_id, "metadata": metadata})) },
        )
    }

    fn standard_operation<'a>(
        &'a self,
        attempt_id: &'a str,
        _generation: u64,
        operation: HostOperation,
    ) -> HostFuture<'a> {
        Box::pin(async move {
            let HostOperation::ObserveProvider { request } = operation else {
                return Err(Self::unavailable(&operation));
            };
            let repository = self.attempt_repository(attempt_id).await?;
            let subject_id = request.subject.id;
            let expected_head = request.subject.revision;
            let provider_operation = request.operation;
            tokio::task::spawn_blocking(move || {
                let repository = crate::repo::Repository {
                    root: repository.into(),
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
            .map_err(|error| ProtocolError::new("provider_observation", error.to_string()))?
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
                let repository = crate::repo::Repository {
                    root: repository.into(),
                };
                let config = crate::config::Config::load(&repository);
                if !config.config_errors.is_empty() {
                    return Err(ProtocolError::new(
                        "invalid_configuration",
                        config.config_errors.join("; "),
                    ));
                }
                match kind {
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
                        let head =
                            request
                                .preconditions
                                .expected_head
                                .as_deref()
                                .ok_or_else(|| {
                                    ProtocolError::new(
                                        "invalid_effect",
                                        "squash merge requires an exact head",
                                    )
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
                                ProtocolError::new(
                                    "invalid_effect",
                                    "worktree deletion requires an exact path",
                                )
                            })?;
                        let branch = request
                            .parameters
                            .get("branch")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                ProtocolError::new(
                                    "invalid_effect",
                                    "worktree deletion requires a branch",
                                )
                            })?;
                        let incarnation = request
                            .preconditions
                            .worktree_session
                            .as_ref()
                            .map(|reference| reference.revision.as_str());
                        crate::session::delete_worktree_session_if_current(
                            &repository,
                            &config,
                            std::path::Path::new(path),
                            branch,
                            incarnation,
                        )
                        .map(|_| serde_json::json!({"status":"deleted", "path":path}))
                        .map_err(|error| ProtocolError::new("delete_worktree", error))
                    }
                    _ => Err(ProtocolError::new(
                        "operation_unavailable",
                        format!(
                            "protected Standard effect is not implemented: {}",
                            kind.label()
                        ),
                    )),
                }
            })
            .await
            .map_err(|error| ProtocolError::new("protected_effect", error.to_string()))?
        })
    }
}

pub(crate) fn dispatcher(database: WorkflowDatabase) -> Arc<dyn crate::extension::HostDispatcher> {
    Arc::new(crate::extension::AllowlistedHostDispatcher::new(
        StandardHostServices::new(database),
    ))
}
