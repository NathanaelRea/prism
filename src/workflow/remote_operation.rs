//! Typed Worker/coordinator operations. Serde tags preserve the existing socket operation labels.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::step_trigger::TriggerSubject;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TuiRemoteListPayload {
    pub repository: PathBuf,
    pub worktree: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TuiRemoteBranchHeadPayload {
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub remote: String,
    pub branch: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TuiRemoteCachePayload {
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    pub force_details: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TuiLocalBranchHeadPayload {
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TuiRemotePushPayload {
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    pub expected: crate::remote::dispatcher::PushGuard,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TuiRemoteFetchPayload {
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    pub summary: crate::remote::PrSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TuiRemoteResolvePayload {
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub summary: crate::remote::PrSummary,
    pub thread_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TuiRemoteReviewPayload {
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub summary: crate::remote::PrSummary,
    pub kind: crate::remote::ReviewSubmissionKind,
    pub body: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TuiRemoteMergePayload {
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub change_request: crate::remote::CanonicalChangeRequestIdentity,
    pub display_number: u64,
    pub expected_head_sha: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResolveThreadsPayload {
    pub subject: TriggerSubject,
    pub observation_revision: String,
    pub thread_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", content = "payload")]
pub enum RemoteObservationOperation {
    #[serde(rename = "change_request.stabilization")]
    ChangeRequestStabilization(TriggerSubject),
    #[serde(rename = "tui.change_requests")]
    TuiChangeRequests(TuiRemoteListPayload),
    #[serde(rename = "tui.repository_policy")]
    TuiRepositoryPolicy(TuiRemoteListPayload),
    #[serde(rename = "tui.remote_branch_head")]
    TuiRemoteBranchHead(TuiRemoteBranchHeadPayload),
    #[serde(rename = "tui.change_request_cache")]
    TuiChangeRequestCache(TuiRemoteCachePayload),
    #[serde(rename = "tui.local_branch_head")]
    TuiLocalBranchHead(TuiLocalBranchHeadPayload),
}

impl RemoteObservationOperation {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::ChangeRequestStabilization(_) => "change_request.stabilization",
            Self::TuiChangeRequests(_) => "tui.change_requests",
            Self::TuiRepositoryPolicy(_) => "tui.repository_policy",
            Self::TuiRemoteBranchHead(_) => "tui.remote_branch_head",
            Self::TuiChangeRequestCache(_) => "tui.change_request_cache",
            Self::TuiLocalBranchHead(_) => "tui.local_branch_head",
        }
    }

    pub fn paths(&self) -> (&std::path::Path, &std::path::Path) {
        match self {
            Self::ChangeRequestStabilization(payload) => (&payload.repository, &payload.worktree),
            Self::TuiChangeRequests(payload) | Self::TuiRepositoryPolicy(payload) => {
                (&payload.repository, &payload.worktree)
            }
            Self::TuiRemoteBranchHead(payload) => (&payload.repository, &payload.worktree),
            Self::TuiChangeRequestCache(payload) => (&payload.repository, &payload.worktree),
            Self::TuiLocalBranchHead(payload) => (&payload.repository, &payload.worktree),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", content = "payload")]
pub enum RemoteMutationOperation {
    #[serde(rename = "change_request.resolve_review_threads")]
    ChangeRequestResolveReviewThreads(ResolveThreadsPayload),
    #[serde(rename = "tui.resolve_review_threads")]
    TuiResolveReviewThreads(TuiRemoteResolvePayload),
    #[serde(rename = "tui.push_branch")]
    TuiPushBranch(TuiRemotePushPayload),
    #[serde(rename = "tui.fetch_change_request")]
    TuiFetchChangeRequest(TuiRemoteFetchPayload),
    #[serde(rename = "tui.submit_review")]
    TuiSubmitReview(TuiRemoteReviewPayload),
    #[serde(rename = "tui.merge_change_request")]
    TuiMergeChangeRequest(TuiRemoteMergePayload),
}

impl RemoteMutationOperation {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::ChangeRequestResolveReviewThreads(_) => "change_request.resolve_review_threads",
            Self::TuiResolveReviewThreads(_) => "tui.resolve_review_threads",
            Self::TuiPushBranch(_) => "tui.push_branch",
            Self::TuiFetchChangeRequest(_) => "tui.fetch_change_request",
            Self::TuiSubmitReview(_) => "tui.submit_review",
            Self::TuiMergeChangeRequest(_) => "tui.merge_change_request",
        }
    }

    pub fn paths(&self) -> (&std::path::Path, &std::path::Path) {
        match self {
            Self::ChangeRequestResolveReviewThreads(payload) => {
                (&payload.subject.repository, &payload.subject.worktree)
            }
            Self::TuiResolveReviewThreads(payload) => (&payload.repository, &payload.worktree),
            Self::TuiPushBranch(payload) => (&payload.repository, &payload.worktree),
            Self::TuiFetchChangeRequest(payload) => (&payload.repository, &payload.worktree),
            Self::TuiSubmitReview(payload) => (&payload.repository, &payload.worktree),
            Self::TuiMergeChangeRequest(payload) => (&payload.repository, &payload.worktree),
        }
    }
}

pub fn validate_envelope_paths(
    outer_repository: &std::path::Path,
    outer_worktree: &std::path::Path,
    payload_paths: (&std::path::Path, &std::path::Path),
) -> Result<(PathBuf, PathBuf), String> {
    let normalize = |path: &std::path::Path| {
        std::fs::canonicalize(path)
            .map_err(|error| format!("normalize remote request path {}: {error}", path.display()))
    };
    let repository = normalize(outer_repository)?;
    let worktree = normalize(outer_worktree)?;
    let payload_repository = normalize(payload_paths.0)?;
    let payload_worktree = normalize(payload_paths.1)?;
    if repository != payload_repository || worktree != payload_worktree {
        return Err("remote request payload paths do not match its envelope paths".into());
    }
    let common_dir = |path: &std::path::Path| -> Result<PathBuf, String> {
        let output = crate::process::run_output_named(
            std::process::Command::new("git").arg("-C").arg(path).args([
                "rev-parse",
                "--path-format=absolute",
                "--git-common-dir",
            ]),
            crate::process::ProcessPolicy::Metadata,
            crate::process::ProcessDescriptor::new("git.remote_request_common_dir"),
        )?;
        if !output.status.success() {
            return Err(format!(
                "remote request path {} is not a Git worktree",
                path.display()
            ));
        }
        let value = output.stdout.trim();
        std::fs::canonicalize(value)
            .map_err(|error| format!("normalize Git common directory {value}: {error}"))
    };
    if common_dir(&repository)? != common_dir(&worktree)? {
        return Err("remote request worktree does not belong to its repository".into());
    }
    Ok((repository, worktree))
}

pub fn namespaced_mutation_request_id(
    repository: &std::path::Path,
    subject: &str,
    client_request_id: &str,
) -> String {
    let mut digest = Sha256::new();
    for field in [
        repository.as_os_str().as_encoded_bytes(),
        subject.as_bytes(),
        client_request_id.as_bytes(),
    ] {
        digest.update((field.len() as u64).to_be_bytes());
        digest.update(field);
    }
    format!("v1:{:x}", digest.finalize())
}

pub fn decode_observation(
    label: &str,
    payload: serde_json::Value,
) -> Result<RemoteObservationOperation, serde_json::Error> {
    serde_json::from_value(serde_json::json!({"operation": label, "payload": payload}))
}

pub fn decode_mutation(
    label: &str,
    payload: serde_json::Value,
) -> Result<RemoteMutationOperation, serde_json::Error> {
    serde_json::from_value(serde_json::json!({"operation": label, "payload": payload}))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuiRemoteMergeOutcome {
    Merged,
    Pending,
    Uncertain,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum TuiRemoteMergeResult {
    Accepted {
        outcome: TuiRemoteMergeOutcome,
        summary: Box<crate::remote::PrSummary>,
    },
    Rejected {
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_paths_and_request_namespaces_fail_closed() {
        let root = std::env::temp_dir().join(format!(
            "prism-remote-operation-paths-{}",
            std::process::id()
        ));
        let repository_a = root.join("a");
        let repository_b = root.join("b");
        std::fs::create_dir_all(&repository_a).unwrap();
        std::fs::create_dir_all(&repository_b).unwrap();
        let operation = RemoteObservationOperation::TuiChangeRequests(TuiRemoteListPayload {
            repository: repository_b.clone(),
            worktree: repository_b.clone(),
        });
        assert!(
            validate_envelope_paths(&repository_a, &repository_a, operation.paths())
                .unwrap_err()
                .contains("do not match")
        );
        assert_ne!(
            namespaced_mutation_request_id(&repository_a, "subject", "same"),
            namespaced_mutation_request_id(&repository_b, "subject", "same")
        );
        assert_ne!(
            namespaced_mutation_request_id(&repository_a, "subject-a", "same"),
            namespaced_mutation_request_id(&repository_a, "subject-b", "same")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn operation_wire_names_are_stable_and_unknown_names_fail_closed() {
        let operation = RemoteObservationOperation::TuiRepositoryPolicy(TuiRemoteListPayload {
            repository: "/repo".into(),
            worktree: "/worktree".into(),
        });
        let value = serde_json::to_value(operation).unwrap();
        assert_eq!(value["operation"], "tui.repository_policy");
        assert!(
            serde_json::from_value::<RemoteObservationOperation>(serde_json::json!({
                "operation": "tui.repositroy_policy",
                "payload": {"repository":"/repo", "worktree":"/repo"}
            }))
            .unwrap_err()
            .to_string()
            .contains("unknown variant")
        );
    }
}
