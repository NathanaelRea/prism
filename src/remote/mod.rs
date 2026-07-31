// This seam is intentionally introduced before callers migrate in later phases.
#![allow(dead_code, unused_imports)]

mod capability;
mod discovery;
pub(crate) mod dispatcher;
mod error;
pub(crate) mod forgejo;
pub(crate) mod github;
pub(crate) mod gitlab;
mod http;
mod model;

pub(crate) use capability::{Capabilities, SupportLevel};
pub(crate) use discovery::{
    DiscoveredRemote, DiscoveryError, GitRemoteParser, GitTransport, HostProfile, ParsedGitRemote,
    RemoteDiscovery, RemoteUrlKind, discover_git_remote,
};
pub(crate) use error::{RemoteError, RemoteErrorClass, RemoteOperation, RetryHint, Retryability};
pub(crate) use model::{
    CanonicalChangeRequestIdentity, ChangeRequest, ChangeRequestDetails, ChangeRequestId,
    ChangeRequestSummary, CheckContext, CheckState, CiFailure, Comment, CreateChangeRequest,
    FetchChangeRequest, GuardedMerge, HeadAssociation, HostIdentity, IdentityError, LifecycleState,
    MergeMethod, MergeabilityState, NativeChangeRequestId, NativeMergeGuard, NativeRepositoryId,
    NativeReviewThreadId, Observation, PolicyFacts, ProviderKind, QueueState, RemoteBase,
    RemoteRepository, RemoteRepositoryId, RepositoryPolicy, ResolveReviewThread, Review,
    ReviewDecision, ReviewThread, WebScheme,
};

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
