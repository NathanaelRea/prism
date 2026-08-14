#![allow(
    dead_code,
    reason = "provider adapters expose capabilities that are activated by optional workflows"
)]

mod cache;
mod capability;
mod coordinator;
mod discovery;
pub(crate) mod dispatcher;
mod error;
mod forgejo;
mod github;
mod gitlab;
mod http;
mod model;
pub mod request_coordinator;
mod store;

pub(crate) use cache::PrCheckContext;
pub(crate) use cache::{
    PR_SUMMARY_POLL_INTERVAL, PrCache, PrCheckState, PrDetails, PrReviewComment, PrSummary,
    RepoPolicyCache, WorkerPrCacheSnapshot, apply_pr_details_poll_result,
    apply_pr_summary_poll_result, pr_cache_comment_count, pr_cache_render_signature,
    pr_summary_or_error,
};
#[cfg(test)]
pub(crate) use cache::{PrComment, PrReview};
pub(crate) use capability::{Capabilities, SupportLevel};
pub(crate) use coordinator::{
    load_pr_cache_for_branch, pr_details_pollable, resolve_pr_summary_for_session,
};
pub(crate) use discovery::{
    DiscoveredRemote, HostProfile, RemoteDiscovery, RemoteUrlKind, discover_git_remote,
};
#[cfg(test)]
pub(crate) use discovery::{DiscoveryError, GitRemoteParser, GitTransport};
pub(crate) use error::{RemoteError, RemoteErrorClass, RemoteOperation, RetryHint, Retryability};
pub(crate) use model::{
    CanonicalChangeRequestIdentity, ChangeRequest, ChangeRequestDetails, ChangeRequestId,
    ChangeRequestSummary, CheckContext, CheckState, CiFailure, Comment, CreateChangeRequest,
    FetchChangeRequest, GuardedMerge, HostIdentity, IdentityError, LifecycleState, MergeMethod,
    MergeMutationOutcome, MergeMutationResult, MergeabilityState, NativeChangeRequestId,
    NativeReviewThreadId, NativeStateEvidence, Observation, PolicyFacts, ProviderItemId,
    ProviderItemKind, ProviderItemObservation, ProviderKind, QueueState, RemoteBase,
    RemoteRepository, RemoteRepositoryId, RepositoryPolicy, ResolveReviewThread, Review,
    ReviewDecision, ReviewSubmissionKind, ReviewThread, SubmitReview, WebScheme,
    native_queue_evidence_is_positive,
};
#[cfg(test)]
pub(crate) use model::{HeadAssociation, NativeMergeGuard};
#[cfg(test)]
pub(crate) use store::save_pr_cache;
pub(crate) use store::{load_pr_cache, persist_pr_cache_snapshot};

#[cfg(test)]
pub(crate) fn test_change_request_identity() -> CanonicalChangeRequestIdentity {
    let host = HostIdentity::new("github.com", None).expect("test host");
    let repository =
        RemoteRepositoryId::new(ProviderKind::GitHub, host, "example/repo").expect("test repo");
    CanonicalChangeRequestIdentity::new(
        &repository,
        &NativeChangeRequestId::new("PR_test").expect("test change request"),
        &repository,
        &repository,
    )
}

#[cfg(test)]
mod tests;
