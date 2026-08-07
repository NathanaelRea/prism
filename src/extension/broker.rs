//! Intent-first adapter for Standard protected host operations.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use prism_extension_protocol::{BrokeredEffectRequest, HostOperation, ProtocolError};
use serde_json::Value;

use super::{HostDispatcher, HostFuture};
use crate::workflow::effect::{ProtectedEffectKind, protected_effect, validate_effect_request};

pub type BrokerFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ProtocolError>> + Send + 'a>>;

#[derive(Clone, Debug)]
pub struct PreparedEffect {
    pub token: String,
    /// A completed idempotent replay returns the durable prior result without dispatching again.
    pub prior_result: Option<Result<Value, ProtocolError>>,
}

/// Persistence seam used by the broker. Implementations must durably commit each transition
/// before returning. An effect token identifies the already-persisted intent.
pub trait EffectLedger: Send + Sync + 'static {
    fn prepare<'a>(
        &'a self,
        attempt_id: &'a str,
        generation: u64,
        kind: ProtectedEffectKind,
        request: &'a BrokeredEffectRequest,
    ) -> BrokerFuture<'a, PreparedEffect>;

    fn mark_dispatching<'a>(&'a self, effect_token: &'a str) -> BrokerFuture<'a, ()>;

    /// `authoritative == false` means the result raced lease loss and reconciliation is required.
    fn record_result<'a>(
        &'a self,
        effect_token: &'a str,
        result: &'a Result<Value, ProtocolError>,
    ) -> BrokerFuture<'a, bool>;
}

/// Adapter seam behind the broker. It receives only validated protocol values and is responsible
/// for resolving opaque references and revalidating their exact revisions before mutation.
pub trait ProtectedEffectBackend: Send + Sync + 'static {
    fn dispatch<'a>(
        &'a self,
        kind: ProtectedEffectKind,
        request: BrokeredEffectRequest,
    ) -> BrokerFuture<'a, Value>;
}

/// Wraps an allowlisted host dispatcher. Observations and bounded execution pass through to the
/// inner dispatcher; Standard protected effects always take the persisted broker path.
pub struct BrokeredHostDispatcher<L, B> {
    ledger: Arc<L>,
    backend: Arc<B>,
    inner: Arc<dyn HostDispatcher>,
}

impl<L, B> BrokeredHostDispatcher<L, B> {
    pub fn new(ledger: Arc<L>, backend: Arc<B>, inner: Arc<dyn HostDispatcher>) -> Self {
        Self {
            ledger,
            backend,
            inner,
        }
    }
}

impl<L: EffectLedger, B: ProtectedEffectBackend> HostDispatcher for BrokeredHostDispatcher<L, B> {
    fn dispatch<'a>(
        &'a self,
        attempt_id: &'a str,
        generation: u64,
        operation: HostOperation,
    ) -> HostFuture<'a> {
        let Some((kind, request)) = protected_effect(&operation) else {
            return self.inner.dispatch(attempt_id, generation, operation);
        };
        let request = request.clone();
        Box::pin(async move {
            validate_effect_request(kind, &request)
                .map_err(|error| ProtocolError::new("invalid_effect", error.to_string()))?;
            let prepared = self
                .ledger
                .prepare(attempt_id, generation, kind, &request)
                .await?;
            if let Some(result) = prepared.prior_result {
                return result;
            }
            let token = prepared.token;
            // Returning from this call proves the dispatching transition is durable. No adapter
            // invocation may happen before it.
            self.ledger.mark_dispatching(&token).await?;
            let result = self.backend.dispatch(kind, request).await;
            let authoritative = self.ledger.record_result(&token, &result).await?;
            if !authoritative {
                return Err(ProtocolError::new(
                    "reconciliation_required",
                    format!("effect '{token}' lost its fence after dispatch"),
                ));
            }
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use prism_extension_protocol::{EffectPreconditions, OpaqueReference};

    use super::*;
    use crate::extension::NoHostOperations;

    #[derive(Default)]
    struct FakeLedger {
        events: Mutex<Vec<String>>,
        fail_mark: bool,
        authoritative: bool,
        prior_result: Option<Result<Value, ProtocolError>>,
    }

    impl EffectLedger for FakeLedger {
        fn prepare<'a>(
            &'a self,
            _attempt_id: &'a str,
            _generation: u64,
            kind: ProtectedEffectKind,
            _request: &'a BrokeredEffectRequest,
        ) -> BrokerFuture<'a, PreparedEffect> {
            Box::pin(async move {
                self.events.lock().unwrap().push("prepared".into());
                Ok(PreparedEffect {
                    token: format!("{}-intent", kind.label()),
                    prior_result: self.prior_result.clone(),
                })
            })
        }

        fn mark_dispatching<'a>(&'a self, _effect_token: &'a str) -> BrokerFuture<'a, ()> {
            Box::pin(async move {
                self.events.lock().unwrap().push("dispatching".into());
                if self.fail_mark {
                    Err(ProtocolError::new("stale_fence", "lease expired"))
                } else {
                    Ok(())
                }
            })
        }

        fn record_result<'a>(
            &'a self,
            _effect_token: &'a str,
            _result: &'a Result<Value, ProtocolError>,
        ) -> BrokerFuture<'a, bool> {
            Box::pin(async move {
                self.events.lock().unwrap().push("result".into());
                Ok(self.authoritative)
            })
        }
    }

    #[derive(Default)]
    struct FakeBackend(Mutex<Vec<String>>);

    impl ProtectedEffectBackend for FakeBackend {
        fn dispatch<'a>(
            &'a self,
            _kind: ProtectedEffectKind,
            _request: BrokeredEffectRequest,
        ) -> BrokerFuture<'a, Value> {
            Box::pin(async move {
                self.0.lock().unwrap().push("adapter".into());
                Ok(serde_json::json!({"status":"pushed"}))
            })
        }
    }

    fn push() -> HostOperation {
        HostOperation::Push {
            request: BrokeredEffectRequest {
                effect_id: "push-1".into(),
                idempotency_key: "run/attempt/push-1".into(),
                authority_scope: "git:write".into(),
                preconditions: EffectPreconditions {
                    repository: OpaqueReference {
                        id: "github:acme/widget".into(),
                        revision: "repository-1".into(),
                    },
                    worktree_session: Some(OpaqueReference {
                        id: "worktree-1".into(),
                        revision: "incarnation-9".into(),
                    }),
                    expected_head: Some("0123456789abcdef0123456789abcdef01234567".into()),
                    target_repository: None,
                    policy_revision: None,
                    gate_revisions: Default::default(),
                },
                parameters: serde_json::json!({}),
            },
        }
    }

    fn every_protected_operation() -> Vec<HostOperation> {
        [
            ProtectedEffectKind::Commit,
            ProtectedEffectKind::Push,
            ProtectedEffectKind::CreateChangeRequest,
            ProtectedEffectKind::ResolveReviewThreads,
            ProtectedEffectKind::SquashMerge,
            ProtectedEffectKind::DeleteWorktree,
        ]
        .into_iter()
        .map(|kind| {
            let request = BrokeredEffectRequest {
                effect_id: format!("{}-1", kind.label()),
                idempotency_key: format!("run/attempt/{}-1", kind.label()),
                authority_scope: kind.authority_scope().into(),
                preconditions: EffectPreconditions {
                    repository: OpaqueReference {
                        id: "github:acme/widget".into(),
                        revision: "repository-1".into(),
                    },
                    worktree_session: Some(OpaqueReference {
                        id: "worktree-1".into(),
                        revision: "incarnation-9".into(),
                    }),
                    expected_head: Some("0123456789abcdef0123456789abcdef01234567".into()),
                    target_repository: Some(OpaqueReference {
                        id: "github:acme/widget".into(),
                        revision: "repository-1".into(),
                    }),
                    policy_revision: Some("policy-1".into()),
                    gate_revisions: [("ci".into(), "observation-1".into())]
                        .into_iter()
                        .collect(),
                },
                parameters: serde_json::json!({
                    "expected_path": "/worktrees/change",
                    "method": "squash",
                    "threads": [{
                        "id": "thread-1",
                        "observed_revision": "thread-revision-1",
                        "addressed_by_artifact": "artifact-1"
                    }]
                }),
            };
            match kind {
                ProtectedEffectKind::Commit => HostOperation::Commit { request },
                ProtectedEffectKind::Push => HostOperation::Push { request },
                ProtectedEffectKind::CreateChangeRequest => {
                    HostOperation::CreateChangeRequest { request }
                }
                ProtectedEffectKind::ResolveReviewThreads => {
                    HostOperation::ResolveReviewThreads { request }
                }
                ProtectedEffectKind::SquashMerge => HostOperation::SquashMerge { request },
                ProtectedEffectKind::DeleteWorktree => HostOperation::DeleteWorktree { request },
            }
        })
        .collect()
    }

    #[tokio::test]
    async fn crash_before_protected_dispatch_never_calls_adapter() {
        for operation in every_protected_operation() {
            let ledger = Arc::new(FakeLedger {
                fail_mark: true,
                ..Default::default()
            });
            let backend = Arc::new(FakeBackend::default());
            let dispatcher = BrokeredHostDispatcher::new(
                ledger.clone(),
                backend.clone(),
                Arc::new(NoHostOperations),
            );
            assert!(dispatcher.dispatch("attempt", 1, operation).await.is_err());
            assert!(backend.0.lock().unwrap().is_empty());
            assert_eq!(
                &*ledger.events.lock().unwrap(),
                &["prepared", "dispatching"]
            );
        }
    }

    #[tokio::test]
    async fn fence_loss_after_dispatch_is_reconciliation_required() {
        let ledger = Arc::new(FakeLedger {
            authoritative: false,
            ..Default::default()
        });
        let backend = Arc::new(FakeBackend::default());
        let dispatcher = BrokeredHostDispatcher::new(
            ledger.clone(),
            backend.clone(),
            Arc::new(NoHostOperations),
        );
        let error = dispatcher.dispatch("attempt", 1, push()).await.unwrap_err();
        assert_eq!(error.code, "reconciliation_required");
        assert_eq!(&*backend.0.lock().unwrap(), &["adapter"]);
        assert_eq!(
            &*ledger.events.lock().unwrap(),
            &["prepared", "dispatching", "result"]
        );
    }

    #[tokio::test]
    async fn completed_idempotent_replay_returns_prior_result_without_dispatch() {
        let ledger = Arc::new(FakeLedger {
            prior_result: Some(Ok(serde_json::json!({"status":"already_pushed"}))),
            ..Default::default()
        });
        let backend = Arc::new(FakeBackend::default());
        let dispatcher = BrokeredHostDispatcher::new(
            ledger.clone(),
            backend.clone(),
            Arc::new(NoHostOperations),
        );
        assert_eq!(
            dispatcher.dispatch("attempt", 1, push()).await.unwrap()["status"],
            "already_pushed"
        );
        assert!(backend.0.lock().unwrap().is_empty());
        assert_eq!(&*ledger.events.lock().unwrap(), &["prepared"]);
    }
}
