use super::RemoteOperation;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SupportLevel {
    Supported,
    Unsupported,
    Conditional,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Capabilities {
    pub(crate) list_change_requests: SupportLevel,
    pub(crate) change_request_details: SupportLevel,
    pub(crate) review_threads: SupportLevel,
    pub(crate) resolve_review_thread: SupportLevel,
    pub(crate) check_rollup: SupportLevel,
    pub(crate) ci_logs: SupportLevel,
    pub(crate) changed_files: SupportLevel,
    pub(crate) repository_policy: SupportLevel,
    pub(crate) fetch_change_request: SupportLevel,
    pub(crate) create_change_request: SupportLevel,
    pub(crate) guarded_merge: SupportLevel,
    pub(crate) guarded_merge_reason: Option<String>,
    pub(crate) merge_queue: SupportLevel,
}

impl Capabilities {
    pub(crate) fn for_provider(provider: super::ProviderKind) -> Self {
        match provider {
            super::ProviderKind::GitHub => Self {
                list_change_requests: SupportLevel::Supported,
                change_request_details: SupportLevel::Supported,
                review_threads: SupportLevel::Supported,
                resolve_review_thread: SupportLevel::Supported,
                check_rollup: SupportLevel::Supported,
                ci_logs: SupportLevel::Supported,
                changed_files: SupportLevel::Supported,
                repository_policy: SupportLevel::Conditional,
                fetch_change_request: SupportLevel::Supported,
                create_change_request: SupportLevel::Supported,
                guarded_merge: SupportLevel::Supported,
                guarded_merge_reason: None,
                merge_queue: SupportLevel::Unknown,
            },
            super::ProviderKind::GitLab => Self {
                list_change_requests: SupportLevel::Supported,
                change_request_details: SupportLevel::Supported,
                review_threads: SupportLevel::Supported,
                resolve_review_thread: SupportLevel::Supported,
                check_rollup: SupportLevel::Supported,
                ci_logs: SupportLevel::Conditional,
                changed_files: SupportLevel::Supported,
                repository_policy: SupportLevel::Conditional,
                fetch_change_request: SupportLevel::Supported,
                create_change_request: SupportLevel::Supported,
                guarded_merge: SupportLevel::Conditional,
                guarded_merge_reason: None,
                merge_queue: SupportLevel::Conditional,
            },
            super::ProviderKind::Forgejo => Self {
                list_change_requests: SupportLevel::Supported,
                change_request_details: SupportLevel::Supported,
                review_threads: SupportLevel::Conditional,
                resolve_review_thread: SupportLevel::Unsupported,
                check_rollup: SupportLevel::Supported,
                ci_logs: SupportLevel::Conditional,
                changed_files: SupportLevel::Supported,
                repository_policy: SupportLevel::Conditional,
                fetch_change_request: SupportLevel::Supported,
                create_change_request: SupportLevel::Conditional,
                guarded_merge: SupportLevel::Conditional,
                guarded_merge_reason: None,
                merge_queue: SupportLevel::Unsupported,
            },
        }
    }

    pub(crate) fn support_for(&self, operation: RemoteOperation) -> SupportLevel {
        match operation {
            RemoteOperation::DiscoverRepository => SupportLevel::Supported,
            RemoteOperation::ListChangeRequests => self.list_change_requests,
            RemoteOperation::ObserveChangeRequest => self.change_request_details,
            RemoteOperation::ObserveReviewThreads => self.review_threads,
            RemoteOperation::ResolveReviewThread => self.resolve_review_thread,
            RemoteOperation::ObserveChecks => self.check_rollup,
            RemoteOperation::LoadCiLogs => self.ci_logs,
            RemoteOperation::ObserveChangedFiles => self.changed_files,
            RemoteOperation::ObserveRepositoryPolicy => self.repository_policy,
            RemoteOperation::FetchChangeRequest => self.fetch_change_request,
            RemoteOperation::CreateChangeRequest => self.create_change_request,
            RemoteOperation::MergeChangeRequest => self.guarded_merge,
            RemoteOperation::ObserveMergeQueue => self.merge_queue,
        }
    }
}

impl SupportLevel {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Conditional => "conditional",
            Self::Unknown => "unknown",
        }
    }
}
