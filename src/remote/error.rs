use std::fmt;
use std::time::Duration;

use super::ProviderKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteOperation {
    DiscoverRepository,
    ListChangeRequests,
    ObserveChangeRequest,
    ObserveReviewThreads,
    ResolveReviewThread,
    ObserveChecks,
    LoadCiLogs,
    ObserveChangedFiles,
    ObserveRepositoryPolicy,
    FetchChangeRequest,
    CreateChangeRequest,
    SubmitReview,
    MergeChangeRequest,
    ObserveMergeQueue,
    DiscoverIssues,
    ObserveProviderEvents,
    MutateLabels,
    MutateAssignment,
    CreateIssueComment,
    MutateIssueLifecycle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteErrorClass {
    Configuration,
    Authentication,
    Authorization,
    NotFound,
    Unsupported,
    Validation,
    Conflict,
    StaleHead,
    RateLimited,
    Timeout,
    Transport,
    InvalidResponse,
    Cancelled,
    Provider,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Retryability {
    Retryable,
    NotRetryable,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryHint {
    After(Duration),
    Backoff,
    Reauthenticate,
    RefreshObservation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteError {
    provider: ProviderKind,
    operation: RemoteOperation,
    class: RemoteErrorClass,
    retryability: Retryability,
    status: Option<u16>,
    exit_code: Option<i32>,
    retry_hint: Option<RetryHint>,
    safe_message: String,
}

impl RemoteError {
    pub(crate) fn new(
        provider: ProviderKind,
        operation: RemoteOperation,
        class: RemoteErrorClass,
        retryability: Retryability,
        safe_message: impl AsRef<str>,
    ) -> Self {
        Self {
            provider,
            operation,
            class,
            retryability,
            status: None,
            exit_code: None,
            retry_hint: None,
            safe_message: sanitize_safe_message(safe_message.as_ref()),
        }
    }

    pub(crate) fn classified(
        provider: ProviderKind,
        operation: RemoteOperation,
        class: RemoteErrorClass,
        retryability: Retryability,
        status: Option<u16>,
        exit_code: Option<i32>,
        retry_hint: Option<RetryHint>,
    ) -> Self {
        let mut safe_message = format!(
            "{provider} {} failed: {}; retry={}",
            operation.label(),
            class.label(),
            retryability.label()
        );
        if let Some(status) = status {
            safe_message.push_str(&format!("; status={status}"));
        }
        if let Some(exit_code) = exit_code {
            safe_message.push_str(&format!("; exit={exit_code}"));
        }
        if let Some(retry_hint) = retry_hint {
            safe_message.push_str(&format!("; hint={}", retry_hint.label()));
        }
        Self {
            provider,
            operation,
            class,
            retryability,
            status,
            exit_code,
            retry_hint,
            safe_message,
        }
    }

    pub(crate) fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub(crate) fn with_exit_code(mut self, exit_code: i32) -> Self {
        self.exit_code = Some(exit_code);
        self
    }

    pub(crate) fn with_retry_hint(mut self, retry_hint: RetryHint) -> Self {
        self.retry_hint = Some(retry_hint);
        self
    }

    pub(crate) fn provider(&self) -> ProviderKind {
        self.provider
    }

    pub(crate) fn operation(&self) -> RemoteOperation {
        self.operation
    }

    pub(crate) fn class(&self) -> RemoteErrorClass {
        self.class
    }

    pub(crate) fn retryability(&self) -> Retryability {
        self.retryability
    }

    pub(crate) fn status(&self) -> Option<u16> {
        self.status
    }

    pub(crate) fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    pub(crate) fn retry_hint(&self) -> Option<RetryHint> {
        self.retry_hint
    }

    pub(crate) fn safe_message(&self) -> &str {
        &self.safe_message
    }
}

impl RemoteOperation {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::DiscoverRepository => "discover_repository",
            Self::ListChangeRequests => "list_change_requests",
            Self::ObserveChangeRequest => "observe_change_request",
            Self::ObserveReviewThreads => "observe_review_threads",
            Self::ResolveReviewThread => "resolve_review_thread",
            Self::ObserveChecks => "observe_checks",
            Self::LoadCiLogs => "load_ci_logs",
            Self::ObserveChangedFiles => "observe_changed_files",
            Self::ObserveRepositoryPolicy => "observe_repository_policy",
            Self::FetchChangeRequest => "fetch_change_request",
            Self::CreateChangeRequest => "create_change_request",
            Self::SubmitReview => "submit_review",
            Self::MergeChangeRequest => "merge_change_request",
            Self::ObserveMergeQueue => "observe_merge_queue",
            Self::DiscoverIssues => "discover_issues",
            Self::ObserveProviderEvents => "observe_provider_events",
            Self::MutateLabels => "mutate_labels",
            Self::MutateAssignment => "mutate_assignment",
            Self::CreateIssueComment => "create_issue_comment",
            Self::MutateIssueLifecycle => "mutate_issue_lifecycle",
        }
    }
}

impl RemoteErrorClass {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::Authentication => "authentication",
            Self::Authorization => "authorization",
            Self::NotFound => "not_found",
            Self::Unsupported => "unsupported",
            Self::Validation => "validation",
            Self::Conflict => "conflict",
            Self::StaleHead => "stale_head",
            Self::RateLimited => "rate_limited",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::InvalidResponse => "invalid_response",
            Self::Cancelled => "cancelled",
            Self::Provider => "provider",
        }
    }
}

impl Retryability {
    const fn label(self) -> &'static str {
        match self {
            Self::Retryable => "retryable",
            Self::NotRetryable => "not_retryable",
            Self::Unknown => "unknown",
        }
    }
}

impl RetryHint {
    fn label(self) -> String {
        match self {
            Self::After(duration) => format!("after_{}ms", duration.as_millis()),
            Self::Backoff => "backoff".to_string(),
            Self::Reauthenticate => "reauthenticate".to_string(),
            Self::RefreshObservation => "refresh_observation".to_string(),
        }
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.safe_message)
    }
}

impl std::error::Error for RemoteError {}

fn sanitize_safe_message(message: &str) -> String {
    const MAX_CHARS: usize = 512;
    crate::observability::redact_freeform(message, MAX_CHARS)
}
