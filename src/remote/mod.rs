#![allow(dead_code)]

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
mod migrations;
mod model;
mod store;

#[cfg(test)]
pub(crate) use cache::PrCheckContext;
pub(crate) use cache::{
    CiFailure as CachedCiFailure, PR_SUMMARY_POLL_INTERVAL, PrCache, PrCheckState, PrComment,
    PrDetails, PrReview, PrReviewComment, PrSummary, RepoPolicyCache, apply_pr_details_poll_result,
    apply_pr_summary_poll_result, pr_cache_comment_count, pr_cache_render_signature,
    pr_summary_or_error, trusted_pr_for_session,
};
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
pub(crate) use migrations::migrate_pr_cache_schema;
#[cfg(test)]
pub(crate) use model::HeadAssociation;
#[allow(unused_imports)]
pub(crate) use model::{
    CanonicalChangeRequestIdentity, ChangeRequest, ChangeRequestDetails, ChangeRequestId,
    ChangeRequestSummary, CheckContext, CheckState, CiFailure, Comment, CreateChangeRequest,
    FetchChangeRequest, GuardedMerge, HostIdentity, IdentityError, IssueId, LifecycleState,
    MergeMethod, MergeMutationOutcome, MergeMutationResult, MergeSubmissionMode, MergeabilityState,
    NativeChangeRequestId, NativeReviewThreadId, NativeStateEvidence, Observation, PolicyFacts,
    ProviderItemId, ProviderItemKind, ProviderItemObservation, ProviderItemObservationState,
    ProviderKind, QueueState, RemoteBase, RemoteRepository, RemoteRepositoryId, RepositoryPolicy,
    ResolveReviewThread, Review, ReviewDecision, ReviewSubmissionKind, ReviewThread, SubmitReview,
    WebScheme,
};
#[cfg(test)]
pub(crate) use store::record_pr_summary;
#[cfg(test)]
pub(crate) use store::save_repo_policy_cache;
pub(crate) use store::{
    load_pr_cache, load_repo_policy_cache_for_repository, persist_pr_cache_snapshot,
    remove_pr_cache_with_conn,
};
#[cfg(test)]
pub(crate) use store::{save_pr_cache, save_pr_details_cache};

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
