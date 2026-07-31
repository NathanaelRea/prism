#![allow(dead_code)]

mod capability;
mod discovery;
pub(crate) mod dispatcher;
mod error;
mod forgejo;
mod github;
mod gitlab;
mod http;
mod model;

pub(crate) use capability::{Capabilities, SupportLevel};
pub(crate) use discovery::{
    DiscoveredRemote, HostProfile, RemoteDiscovery, RemoteUrlKind, discover_git_remote,
};
#[cfg(test)]
pub(crate) use discovery::{DiscoveryError, GitRemoteParser, GitTransport};
pub(crate) use error::{RemoteError, RemoteErrorClass, RemoteOperation, RetryHint, Retryability};
pub(crate) use github::{
    CiFailure as CachedCiFailure, PR_SUMMARY_POLL_INTERVAL, PrCache, PrCheckState, PrComment,
    PrDetails, PrReview, PrReviewComment, PrSummary, RepoPolicyCache, apply_pr_details_poll_result,
    apply_pr_summary_poll_result, load_pr_cache, load_pr_cache_for_branch,
    load_repo_policy_cache_for_repository, migrate_pr_cache_schema, persist_pr_cache_snapshot,
    pr_cache_comment_count, pr_cache_render_signature, pr_details_pollable, pr_summary_or_error,
    remove_pr_cache_with_conn, resolve_pr_summary_for_session, trusted_pr_for_session,
};
#[cfg(test)]
pub(crate) use github::{
    PrCheckContext, record_pr_summary, save_pr_cache, save_pr_details_cache, save_repo_policy_cache,
};
pub(crate) use model::{
    CanonicalChangeRequestIdentity, ChangeRequest, ChangeRequestDetails, ChangeRequestId,
    ChangeRequestSummary, CheckContext, CheckState, CiFailure, Comment, CreateChangeRequest,
    FetchChangeRequest, GuardedMerge, HostIdentity, IdentityError, LifecycleState, MergeMethod,
    MergeMutationOutcome, MergeMutationResult, MergeabilityState, NativeChangeRequestId,
    NativeReviewThreadId, NativeStateEvidence, Observation, PolicyFacts, ProviderKind, QueueState,
    RemoteBase, RemoteRepository, RemoteRepositoryId, RepositoryPolicy, ResolveReviewThread,
    Review, ReviewDecision, ReviewThread, WebScheme,
};
#[cfg(test)]
pub(crate) use model::{HeadAssociation, NativeMergeGuard};

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
