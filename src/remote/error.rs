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
    MergeChangeRequest,
    ObserveMergeQueue,
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

impl fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.safe_message)
    }
}

impl std::error::Error for RemoteError {}

fn sanitize_safe_message(message: &str) -> String {
    const MAX_CHARS: usize = 512;

    let mut sanitized = String::new();
    let mut previous_was_space = false;
    let mut character_count = 0;
    for character in message.chars().filter(|character| !character.is_control()) {
        let is_space = character.is_whitespace();
        if is_space {
            if previous_was_space || sanitized.is_empty() {
                continue;
            }
            sanitized.push(' ');
        } else {
            sanitized.push(character);
        }
        character_count += 1;
        previous_was_space = is_space;
        if character_count == MAX_CHARS {
            break;
        }
    }
    sanitized.trim_end().to_string()
}
