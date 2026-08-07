//! Provider-neutral validation and reconciliation contracts for Standard host effects.
//!
//! This module deliberately deals only in public protocol values. Resolving an opaque
//! repository or Worktree Session reference to a local path, provider adapter, or credential is
//! a host responsibility and never part of the extension protocol.

use std::collections::BTreeSet;
use std::fmt;

use prism_extension_protocol::{BrokeredEffectRequest, HostOperation};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtectedEffectKind {
    Commit,
    Push,
    CreateChangeRequest,
    ResolveReviewThreads,
    SquashMerge,
    DeleteWorktree,
}

impl ProtectedEffectKind {
    pub const fn authority_scope(self) -> &'static str {
        match self {
            Self::Commit | Self::Push => "git:write",
            Self::CreateChangeRequest | Self::ResolveReviewThreads | Self::SquashMerge => {
                "provider:write"
            }
            Self::DeleteWorktree => "worktrunk:write",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Push => "push",
            Self::CreateChangeRequest => "create_change_request",
            Self::ResolveReviewThreads => "resolve_review_threads",
            Self::SquashMerge => "squash_merge",
            Self::DeleteWorktree => "delete_worktree",
        }
    }
}

pub fn protected_effect(
    operation: &HostOperation,
) -> Option<(ProtectedEffectKind, &BrokeredEffectRequest)> {
    match operation {
        HostOperation::Commit { request } => Some((ProtectedEffectKind::Commit, request)),
        HostOperation::Push { request } => Some((ProtectedEffectKind::Push, request)),
        HostOperation::CreateChangeRequest { request } => {
            Some((ProtectedEffectKind::CreateChangeRequest, request))
        }
        HostOperation::ResolveReviewThreads { request } => {
            Some((ProtectedEffectKind::ResolveReviewThreads, request))
        }
        HostOperation::SquashMerge { request } => Some((ProtectedEffectKind::SquashMerge, request)),
        HostOperation::DeleteWorktree { request } => {
            Some((ProtectedEffectKind::DeleteWorktree, request))
        }
        HostOperation::ReadArtifact { .. }
        | HostOperation::TraceProcess { .. }
        | HostOperation::TraceAgent { .. }
        | HostOperation::RunProcess { .. }
        | HostOperation::RunAgent { .. }
        | HostOperation::ObserveProvider { .. } => None,
    }
}

/// Rejects incomplete intents before they can be persisted or dispatched. The backend must still
/// re-resolve and revalidate every referenced revision immediately before mutation.
pub fn validate_effect_request(
    kind: ProtectedEffectKind,
    request: &BrokeredEffectRequest,
) -> Result<(), EffectContractError> {
    required("effect ID", &request.effect_id)?;
    required("idempotency key", &request.idempotency_key)?;
    if request.authority_scope != kind.authority_scope() {
        return Err(EffectContractError::new(format!(
            "{} requires authority scope '{}'",
            kind.label(),
            kind.authority_scope()
        )));
    }
    validate_reference("repository", &request.preconditions.repository)?;
    if !request.parameters.is_object() {
        return Err(EffectContractError::new(
            "protected effect parameters must be a JSON object",
        ));
    }

    let needs_worktree = matches!(
        kind,
        ProtectedEffectKind::Commit
            | ProtectedEffectKind::Push
            | ProtectedEffectKind::CreateChangeRequest
            | ProtectedEffectKind::DeleteWorktree
    );
    if needs_worktree {
        validate_reference(
            "Worktree Session",
            request
                .preconditions
                .worktree_session
                .as_ref()
                .ok_or_else(|| {
                    EffectContractError::new(
                        "protected effect requires a Worktree Session incarnation",
                    )
                })?,
        )?;
    }

    let needs_head = !matches!(kind, ProtectedEffectKind::DeleteWorktree);
    if needs_head {
        validate_sha(
            request
                .preconditions
                .expected_head
                .as_deref()
                .ok_or_else(|| {
                    EffectContractError::new("protected effect requires an exact head")
                })?,
        )?;
    }

    let provider_mutation = matches!(
        kind,
        ProtectedEffectKind::CreateChangeRequest
            | ProtectedEffectKind::ResolveReviewThreads
            | ProtectedEffectKind::SquashMerge
    );
    if provider_mutation {
        validate_reference(
            "target repository",
            request
                .preconditions
                .target_repository
                .as_ref()
                .ok_or_else(|| {
                    EffectContractError::new(
                        "provider mutation requires an exact target repository",
                    )
                })?,
        )?;
    }

    match kind {
        ProtectedEffectKind::ResolveReviewThreads => validate_thread_resolution(request)?,
        ProtectedEffectKind::SquashMerge => validate_merge(request)?,
        ProtectedEffectKind::DeleteWorktree => {
            required_parameter_string(&request.parameters, "expected_path")?;
        }
        ProtectedEffectKind::Commit
        | ProtectedEffectKind::Push
        | ProtectedEffectKind::CreateChangeRequest => {}
    }
    Ok(())
}

fn validate_thread_resolution(request: &BrokeredEffectRequest) -> Result<(), EffectContractError> {
    let threads = request
        .parameters
        .get("threads")
        .and_then(Value::as_array)
        .ok_or_else(|| EffectContractError::new("thread resolution requires a threads array"))?;
    if threads.is_empty() {
        return Err(EffectContractError::new(
            "thread resolution requires at least one addressed thread",
        ));
    }
    let mut ids = BTreeSet::new();
    for thread in threads {
        let id = required_object_string(thread, "id")?;
        required_object_string(thread, "observed_revision")?;
        required_object_string(thread, "addressed_by_artifact")?;
        if !ids.insert(id) {
            return Err(EffectContractError::new(
                "thread resolution contains a duplicate thread ID",
            ));
        }
    }
    Ok(())
}

fn validate_merge(request: &BrokeredEffectRequest) -> Result<(), EffectContractError> {
    required(
        "repository policy revision",
        request
            .preconditions
            .policy_revision
            .as_deref()
            .ok_or_else(|| EffectContractError::new("squash merge requires repository policy"))?,
    )?;
    if request.preconditions.gate_revisions.is_empty()
        || request
            .preconditions
            .gate_revisions
            .iter()
            .any(|(gate, revision)| gate.trim().is_empty() || revision.trim().is_empty())
    {
        return Err(EffectContractError::new(
            "squash merge requires exact non-empty Gate revisions",
        ));
    }
    if request.parameters.get("method").and_then(Value::as_str) != Some("squash") {
        return Err(EffectContractError::new(
            "Standard guarded merge accepts only the squash method",
        ));
    }
    Ok(())
}

fn validate_reference(
    label: &str,
    reference: &prism_extension_protocol::OpaqueReference,
) -> Result<(), EffectContractError> {
    required(&format!("{label} ID"), &reference.id)?;
    required(&format!("{label} revision"), &reference.revision)
}

fn required(label: &str, value: &str) -> Result<(), EffectContractError> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        Err(EffectContractError::new(format!(
            "{label} is empty or invalid"
        )))
    } else {
        Ok(())
    }
}

fn validate_sha(value: &str) -> Result<(), EffectContractError> {
    if (7..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(EffectContractError::new(
            "expected head must be an exact hexadecimal object ID",
        ))
    }
}

fn required_parameter_string<'a>(
    value: &'a Value,
    field: &str,
) -> Result<&'a str, EffectContractError> {
    let value = value.get(field).and_then(Value::as_str).ok_or_else(|| {
        EffectContractError::new(format!("protected effect requires parameter '{field}'"))
    })?;
    required(field, value)?;
    Ok(value)
}

fn required_object_string<'a>(
    value: &'a Value,
    field: &str,
) -> Result<&'a str, EffectContractError> {
    let value = value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| EffectContractError::new(format!("thread provenance requires '{field}'")))?;
    required(field, value)?;
    Ok(value)
}

/// Authoritative reconciliation outcome for a persisted mutation intent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReconciliationStatus {
    ExactResultApplied { result_revision: String },
    NotAppliedPreconditionsIntact,
    ExternallySatisfied { result_revision: String },
    Superseded { observed_revision: String },
    Diverged { observed_revision: String },
    Indeterminate { reason: String },
}

impl ReconciliationStatus {
    pub const fn permits_automatic_retry(&self) -> bool {
        matches!(self, Self::NotAppliedPreconditionsIntact)
    }

    pub const fn succeeded(&self) -> bool {
        matches!(
            self,
            Self::ExactResultApplied { .. } | Self::ExternallySatisfied { .. }
        )
    }
}

/// Normalized quality keeps unsupported, stale, partial, and unknown provider facts distinct.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "quality", content = "value", rename_all = "snake_case")]
pub enum Evidence<T> {
    Current(T),
    Stale(T),
    Partial(T),
    Unsupported { reason: String },
    Unknown { reason: String },
    Unavailable { reason: String },
}

impl<T> Evidence<T> {
    pub const fn authoritative(&self) -> Option<&T> {
        match self {
            Self::Current(value) => Some(value),
            Self::Stale(_)
            | Self::Partial(_)
            | Self::Unsupported { .. }
            | Self::Unknown { .. }
            | Self::Unavailable { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectContractError(String);

impl EffectContractError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for EffectContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for EffectContractError {}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_extension_protocol::{EffectPreconditions, OpaqueReference};

    fn request(kind: ProtectedEffectKind) -> BrokeredEffectRequest {
        BrokeredEffectRequest {
            effect_id: "effect-1".into(),
            idempotency_key: "run/attempt/effect-1".into(),
            authority_scope: kind.authority_scope().into(),
            preconditions: EffectPreconditions {
                repository: OpaqueReference {
                    id: "github:acme/widget".into(),
                    revision: "repo-r1".into(),
                },
                worktree_session: Some(OpaqueReference {
                    id: "session-1".into(),
                    revision: "incarnation-2".into(),
                }),
                expected_head: Some("0123456789abcdef0123456789abcdef01234567".into()),
                target_repository: Some(OpaqueReference {
                    id: "github:acme/widget".into(),
                    revision: "target-r3".into(),
                }),
                policy_revision: Some("policy-r4".into()),
                gate_revisions: [("ci".into(), "gate-r5".into())].into(),
            },
            parameters: serde_json::json!({"method":"squash"}),
        }
    }

    #[test]
    fn guarded_merge_binds_exact_head_target_policy_and_gates() {
        let valid = request(ProtectedEffectKind::SquashMerge);
        validate_effect_request(ProtectedEffectKind::SquashMerge, &valid).unwrap();

        let mut stale = valid.clone();
        stale.preconditions.expected_head = None;
        assert!(
            validate_effect_request(ProtectedEffectKind::SquashMerge, &stale)
                .unwrap_err()
                .to_string()
                .contains("exact head")
        );
        let mut no_policy = valid.clone();
        no_policy.preconditions.policy_revision = None;
        assert!(
            validate_effect_request(ProtectedEffectKind::SquashMerge, &no_policy)
                .unwrap_err()
                .to_string()
                .contains("policy")
        );
    }

    #[test]
    fn addressed_threads_require_exact_provenance_and_changed_threads_invalidate_it() {
        let mut valid = request(ProtectedEffectKind::ResolveReviewThreads);
        valid.parameters = serde_json::json!({"threads":[{
            "id":"thread-1",
            "observed_revision":"thread-r1",
            "addressed_by_artifact":"repair-r2"
        }]});
        validate_effect_request(ProtectedEffectKind::ResolveReviewThreads, &valid).unwrap();
        valid.parameters["threads"][0]["observed_revision"] = Value::String(String::new());
        assert!(
            validate_effect_request(ProtectedEffectKind::ResolveReviewThreads, &valid).is_err()
        );
    }

    #[test]
    fn only_intact_not_applied_effects_retry_automatically() {
        assert!(ReconciliationStatus::NotAppliedPreconditionsIntact.permits_automatic_retry());
        assert!(
            !ReconciliationStatus::Diverged {
                observed_revision: "new-head".into()
            }
            .permits_automatic_retry()
        );
        assert!(
            ReconciliationStatus::ExternallySatisfied {
                result_revision: "merged".into()
            }
            .succeeded()
        );
    }

    #[test]
    fn non_current_provider_evidence_never_authorizes_a_gate() {
        let stale = Evidence::Stale("old");
        let unsupported: Evidence<&str> = Evidence::Unsupported {
            reason: "Forgejo cannot resolve threads".into(),
        };
        assert_eq!(stale.authoritative(), None);
        assert_eq!(unsupported.authoritative(), None);
        assert_eq!(Evidence::Current("exact").authoritative(), Some(&"exact"));
    }
}
