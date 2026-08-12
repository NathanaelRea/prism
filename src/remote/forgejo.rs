use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::http::HttpClient;
use super::{
    Capabilities, ChangeRequest, ChangeRequestDetails, ChangeRequestId, ChangeRequestSummary,
    CheckContext, CheckState, CiFailure, Comment, CreateChangeRequest, FetchChangeRequest,
    GuardedMerge, HostProfile, LifecycleState, MergeMethod, MergeMutationResult, MergeabilityState,
    NativeChangeRequestId, NativeReviewThreadId, NativeStateEvidence, Observation, PolicyFacts,
    ProviderKind, QueueState, RemoteError, RemoteErrorClass, RemoteOperation, RemoteRepositoryId,
    RepositoryPolicy, ResolveReviewThread, RetryHint, Retryability, Review, ReviewDecision,
    ReviewThread, SubmitReview, SupportLevel,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_RESPONSE_LIMIT: usize = 1024 * 1024;
const PAGE_SIZE: u32 = 50;
const INSTANCE_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const MINIMUM_MUTATION_MAJOR: u64 = 9;
const MAXIMUM_MUTATION_MAJOR: u64 = 16;
const MAX_REVIEW_COMMENT_REQUESTS: usize = 100;
const MAX_FAILED_JOBS: usize = 32;
const MAX_ACTION_PAGES: u32 = 100;
const MAX_LIST_SUMMARY_ENRICHMENTS: usize = 8;
const LOG_TAIL_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub(crate) struct ForgejoAdapter {
    profile: HostProfile,
    client: HttpClient,
    cancelled: Arc<AtomicBool>,
    _cancellation_waiter: Arc<CancellationWaiter>,
}

struct CancellationWaiter {
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl CancellationWaiter {
    fn new(task: Option<tokio::task::JoinHandle<()>>) -> Self {
        Self {
            task: Mutex::new(task),
        }
    }
}

impl Drop for CancellationWaiter {
    fn drop(&mut self) {
        if let Some(task) = self
            .task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            task.abort();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ForgejoInstance {
    pub(crate) version: String,
    pub(crate) settings: ForgejoApiSettings,
    pub(crate) observed_at: SystemTime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ForgejoRuntimeDiagnostics {
    pub(crate) instance: ForgejoInstance,
    pub(crate) capabilities: Capabilities,
}

static INSTANCE_CACHE: OnceLock<Mutex<HashMap<String, ForgejoInstance>>> = OnceLock::new();

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct ForgejoApiSettings {
    pub(crate) max_response_items: Option<u32>,
    pub(crate) default_paging_num: Option<u32>,
    pub(crate) default_git_trees_per_page: Option<u32>,
    pub(crate) default_max_blob_size: Option<u64>,
}

impl ForgejoAdapter {
    pub(crate) fn new(profile: HostProfile) -> Result<Self, RemoteError> {
        Self::with_transport_options(profile, DEFAULT_TIMEOUT, DEFAULT_RESPONSE_LIMIT)
    }

    fn with_transport_options(
        profile: HostProfile,
        timeout: Duration,
        response_limit: usize,
    ) -> Result<Self, RemoteError> {
        if profile.provider != ProviderKind::Forgejo {
            return Err(error(
                RemoteOperation::DiscoverRepository,
                RemoteErrorClass::Configuration,
                Retryability::NotRetryable,
                "Forgejo adapter requires a configured Forgejo host profile",
            ));
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation_waiter = crate::process::current_cancellation().map(|token| {
            let cancelled = Arc::clone(&cancelled);
            tokio::spawn(async move {
                token.cancelled().await;
                cancelled.store(true, Ordering::Relaxed);
            })
        });
        let client = HttpClient::new(&profile, timeout, response_limit, Arc::clone(&cancelled))?;
        Ok(Self {
            profile,
            client,
            cancelled,
            _cancellation_waiter: Arc::new(CancellationWaiter::new(cancellation_waiter)),
        })
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub(crate) fn capabilities(&self) -> Capabilities {
        let mut capabilities = Capabilities {
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
            submit_review: SupportLevel::Unsupported,
            guarded_merge: SupportLevel::Conditional,
            guarded_merge_reason: None,
            merge_queue: SupportLevel::Unsupported,
            issue_discovery: SupportLevel::Unsupported,
            provider_events: SupportLevel::Unsupported,
            issue_labels: SupportLevel::Unsupported,
            issue_assignment: SupportLevel::Unsupported,
            issue_comments: SupportLevel::Unsupported,
            issue_lifecycle: SupportLevel::Unsupported,
        };
        if let Some(instance) = self.cached_instance() {
            match forgejo_major(&instance.version) {
                Some(major)
                    if (MINIMUM_MUTATION_MAJOR..=MAXIMUM_MUTATION_MAJOR).contains(&major) =>
                {
                    capabilities.review_threads = SupportLevel::Supported;
                    capabilities.repository_policy = SupportLevel::Supported;
                    capabilities.create_change_request = SupportLevel::Supported;
                    capabilities.guarded_merge = SupportLevel::Supported;
                }
                Some(_) => {
                    capabilities.create_change_request = SupportLevel::Unsupported;
                    capabilities.guarded_merge = SupportLevel::Unsupported;
                }
                None => {
                    capabilities.create_change_request = SupportLevel::Unknown;
                    capabilities.guarded_merge = SupportLevel::Unknown;
                }
            }
        }
        capabilities
    }

    pub(crate) fn discover_instance(&self) -> Result<ForgejoInstance, RemoteError> {
        self.discover_instance_for(RemoteOperation::DiscoverRepository)
    }

    pub(crate) fn runtime_diagnostics(
        &self,
        repository: &RemoteRepositoryId,
    ) -> Result<ForgejoRuntimeDiagnostics, RemoteError> {
        let instance = self.discover_instance()?;
        self.pagination(RemoteOperation::DiscoverRepository)?;
        let project = self.project(repository, RemoteOperation::DiscoverRepository)?;
        let repository = self.client.get_json::<RepositoryResponse>(
            RemoteOperation::DiscoverRepository,
            &format!("repos/{project}"),
            &[],
        )?;
        let mut capabilities = self.capabilities();
        capabilities.ci_logs = match repository.has_actions {
            Some(true) => SupportLevel::Supported,
            Some(false) => SupportLevel::Unsupported,
            None => SupportLevel::Unknown,
        };
        Ok(ForgejoRuntimeDiagnostics {
            instance,
            capabilities,
        })
    }

    fn discover_instance_for(
        &self,
        operation: RemoteOperation,
    ) -> Result<ForgejoInstance, RemoteError> {
        if let Some(instance) = self.cached_instance() {
            return Ok(instance);
        }
        let version = self
            .client
            .get_json::<VersionResponse>(operation, "version", &[])?;
        if version.version.trim().is_empty() {
            return Err(invalid_response(
                operation,
                "Forgejo version response omitted the version",
            ));
        }
        let settings =
            self.client
                .get_json::<ForgejoApiSettings>(operation, "settings/api", &[])?;
        let instance = ForgejoInstance {
            version: version.version,
            settings,
            observed_at: SystemTime::now(),
        };
        instance_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(self.instance_cache_key(), instance.clone());
        Ok(instance)
    }

    pub(crate) fn list_change_requests(
        &self,
        repository: &RemoteRepositoryId,
    ) -> Result<Vec<ChangeRequestSummary>, RemoteError> {
        let project = self.project(repository, RemoteOperation::ListChangeRequests)?;
        let pagination = self.pagination(RemoteOperation::ListChangeRequests)?;
        let pulls = self.client.get_json_pages::<PullRequestResponse>(
            RemoteOperation::ListChangeRequests,
            &format!("repos/{project}/pulls"),
            &[
                ("state", "open".to_string()),
                ("limit", pagination.limit),
                ("page", "1".to_string()),
            ],
        )?;
        let mut enriched = 0;
        pulls
            .into_iter()
            .map(|pull| {
                let actions_enabled = actions_enabled(&pull);
                let summary =
                    self.summary(repository, pull, RemoteOperation::ListChangeRequests)?;
                if summary.lifecycle == LifecycleState::Open
                    && enriched < MAX_LIST_SUMMARY_ENRICHMENTS
                {
                    enriched += 1;
                    self.enrich_summary(summary, &project, actions_enabled)
                } else {
                    Ok(summary)
                }
            })
            .collect()
    }

    pub(crate) fn change_request_summary(
        &self,
        id: &ChangeRequestId,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        let (summary, actions_enabled) =
            self.load_summary_and_settings(id, RemoteOperation::ObserveChangeRequest)?;
        let project = self.project(id.repository(), RemoteOperation::ObserveChangeRequest)?;
        self.enrich_summary(summary, &project, actions_enabled)
    }

    fn load_summary(
        &self,
        id: &ChangeRequestId,
        operation: RemoteOperation,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        self.load_summary_and_settings(id, operation)
            .map(|(summary, _)| summary)
    }

    fn load_summary_and_settings(
        &self,
        id: &ChangeRequestId,
        operation: RemoteOperation,
    ) -> Result<(ChangeRequestSummary, Option<bool>), RemoteError> {
        let (project, number) = self.change_request_route(id, operation)?;
        let pull = self.client.get_json::<PullRequestResponse>(
            operation,
            &format!("repos/{project}/pulls/{number}"),
            &[],
        )?;
        let actions_enabled = actions_enabled(&pull);
        self.summary(id.repository(), pull, operation)
            .map(|summary| (summary, actions_enabled))
    }

    pub(crate) fn change_request_details(
        &self,
        change_request: &ChangeRequest,
    ) -> Result<ChangeRequestDetails, RemoteError> {
        let (summary, actions_enabled) = self
            .load_summary_and_settings(&change_request.id, RemoteOperation::ObserveChangeRequest)?;
        ensure_details_association(&summary.change_request, change_request)?;
        let (project, number) =
            self.change_request_route(&change_request.id, RemoteOperation::ObserveChangeRequest)?;
        let head_sha = summary.change_request.head_sha.clone();
        let comments = self.observe_comments(&project, number);
        let reviews_result = self.load_reviews(&project, number);
        let reviews = observation(
            reviews_result
                .as_ref()
                .map(|reviews| reviews.iter().map(map_review).collect::<Vec<_>>())
                .map_err(Clone::clone),
            false,
        );
        let review_threads = match reviews_result {
            Ok(reviews) => observation(self.load_review_threads(&project, number, &reviews), true),
            Err(remote_error) => Observation::Failed(remote_error),
        };
        let changed_files = self.observe_changed_files(&project, number);
        let status_result = self.load_statuses(&project, &head_sha);
        let (checks, ci_failures) = if actions_enabled == Some(false) {
            (observation(status_result, true), Observation::Unsupported)
        } else {
            let action_runs = self.load_action_runs(&project, &head_sha);
            let checks = checks_observation(status_result, action_runs.as_ref().map(Vec::as_slice));
            let failures = match action_runs {
                Ok(runs) => observation(self.load_action_failures_for_runs(&project, runs), true),
                Err(remote_error) => observation(Err(remote_error), true),
            };
            (checks, failures)
        };
        let after = self.load_summary(&change_request.id, RemoteOperation::ObserveChangeRequest)?;
        ensure_details_association(&after.change_request, &summary.change_request)?;
        Ok(ChangeRequestDetails {
            association: Some(summary.change_request.head_association()),
            comments,
            reviews,
            review_threads,
            changed_files,
            checks,
            ci_failures,
        })
    }

    pub(crate) fn repository_policy(
        &self,
        repository: &RemoteRepositoryId,
        target_branch: &str,
    ) -> Result<RepositoryPolicy, RemoteError> {
        let project = self.project(repository, RemoteOperation::ObserveRepositoryPolicy)?;
        let result = self.client.get_json::<BranchProtectionResponse>(
            RemoteOperation::ObserveRepositoryPolicy,
            &format!(
                "repos/{project}/branch_protections/{}",
                path_segment(target_branch)
            ),
            &[],
        );
        let facts = match result {
            Ok(protection) => PolicyFacts {
                required_checks: match (
                    protection.enable_status_check,
                    protection.status_check_contexts,
                ) {
                    (Some(false), _) => Observation::Known(Vec::new()),
                    (Some(true), Some(contexts)) => Observation::Known(contexts),
                    _ => Observation::NotLoaded,
                },
                required_approvals: protection
                    .required_approvals
                    .map(Observation::Known)
                    .unwrap_or(Observation::NotLoaded),
                conversations_must_be_resolved: Observation::Known(false),
                source_must_be_up_to_date: protection
                    .block_on_outdated_branch
                    .map(Observation::Known)
                    .unwrap_or(Observation::NotLoaded),
                queue_required: Observation::Known(false),
            },
            Err(remote_error) if remote_error.class() == RemoteErrorClass::NotFound => {
                // Forgejo's 404 is ambiguous between no matching rule and hidden policy.
                PolicyFacts {
                    required_checks: Observation::NotLoaded,
                    required_approvals: Observation::NotLoaded,
                    conversations_must_be_resolved: Observation::Known(false),
                    source_must_be_up_to_date: Observation::NotLoaded,
                    queue_required: Observation::Known(false),
                }
            }
            Err(remote_error) => return Err(remote_error),
        };
        Ok(RepositoryPolicy {
            repository: Some(repository.clone()),
            target_branch: target_branch.to_string(),
            facts,
        })
    }

    pub(crate) fn fetch_change_request(
        &self,
        id: &ChangeRequestId,
    ) -> Result<FetchChangeRequest, RemoteError> {
        let summary = self.load_summary(id, RemoteOperation::FetchChangeRequest)?;
        Ok(FetchChangeRequest {
            id: summary.change_request.id,
            source_repository: summary.change_request.source_repository,
            source_branch: summary.change_request.source_branch,
            expected_head_sha: summary.change_request.head_sha,
        })
    }

    pub(crate) fn create_change_request(
        &self,
        request: CreateChangeRequest,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        let target = self.project(
            &request.target_repository,
            RemoteOperation::CreateChangeRequest,
        )?;
        let source = self.project(
            &request.source_repository,
            RemoteOperation::CreateChangeRequest,
        )?;
        self.require_supported_mutations(RemoteOperation::CreateChangeRequest)?;
        self.verify_branch_head(&source, &request.source_branch, &request.expected_head_sha)?;
        let lowercase_title = request.title.to_ascii_lowercase();
        let already_draft = ["wip:", "[wip]", "draft:", "[draft]"]
            .iter()
            .any(|prefix| lowercase_title.starts_with(prefix));
        let title = if request.draft && !already_draft {
            format!("WIP: {}", request.title)
        } else {
            request.title.clone()
        };
        let head = if request.source_repository == request.target_repository {
            request.source_branch.clone()
        } else {
            let (owner, _) = source.split_once('/').expect("validated Forgejo project");
            format!("{owner}:{}", request.source_branch)
        };
        let response = self.client.send_json(
            RemoteOperation::CreateChangeRequest,
            &format!("repos/{target}/pulls"),
            &CreatePullRequestBody {
                base: &request.target_branch,
                head: &head,
                title: &title,
                body: &request.body,
            },
        )?;
        let created = response.json::<PullRequestResponse>(RemoteOperation::CreateChangeRequest)?;
        let created_summary = self.summary(
            &request.target_repository,
            created,
            RemoteOperation::CreateChangeRequest,
        )?;
        if created_summary.change_request.head_sha != request.expected_head_sha {
            return Err(stale_head_error(RemoteOperation::CreateChangeRequest, None));
        }
        self.load_summary(
            &created_summary.change_request.id,
            RemoteOperation::CreateChangeRequest,
        )
    }

    pub(crate) fn merge_change_request(
        &self,
        request: GuardedMerge,
    ) -> Result<MergeMutationResult, RemoteError> {
        if request.id.repository() != &request.target_repository {
            return Err(validation_error(
                RemoteOperation::MergeChangeRequest,
                "Forgejo merge target does not match the change request repository",
            ));
        }
        self.require_supported_mutations(RemoteOperation::MergeChangeRequest)?;
        let observed = self.load_summary(&request.id, RemoteOperation::MergeChangeRequest)?;
        if observed.change_request.head_sha != request.expected_source_sha
            || observed.change_request.target_branch != request.target_branch
        {
            return Err(stale_head_error(RemoteOperation::MergeChangeRequest, None));
        }
        let (project, number) =
            self.change_request_route(&request.id, RemoteOperation::MergeChangeRequest)?;
        let merge_result = self.client.send_json(
            RemoteOperation::MergeChangeRequest,
            &format!("repos/{project}/pulls/{number}/merge"),
            &MergePullRequestBody {
                operation: match request.method {
                    MergeMethod::Merge => "merge",
                    MergeMethod::Squash => "squash",
                    MergeMethod::Rebase => "rebase",
                },
                head_commit_id: &request.expected_source_sha,
            },
        );
        if let Err(remote_error) = merge_result {
            if remote_error.class() == RemoteErrorClass::Conflict
                && self
                    .load_summary(&request.id, RemoteOperation::MergeChangeRequest)
                    .is_ok_and(|current| {
                        current.change_request.head_sha != request.expected_source_sha
                            || current.change_request.target_branch != request.target_branch
                    })
            {
                return Err(stale_head_error(
                    RemoteOperation::MergeChangeRequest,
                    remote_error.status(),
                ));
            }
            return Err(remote_error);
        }
        let summary = self.load_summary(&request.id, RemoteOperation::MergeChangeRequest)?;
        let native_state = match &summary.lifecycle {
            LifecycleState::Open => "open",
            LifecycleState::Closed => "closed",
            LifecycleState::Merged => "merged",
            LifecycleState::Unknown(native) => native,
        };
        Ok(MergeMutationResult::from_summary(
            summary.clone(),
            native_state,
        ))
    }

    pub(crate) fn submit_review(&self, _request: &SubmitReview) -> Result<(), RemoteError> {
        Err(unsupported(
            RemoteOperation::SubmitReview,
            "Forgejo review submission is not supported",
        ))
    }

    pub(crate) fn resolve_review_thread(
        &self,
        _request: ResolveReviewThread,
    ) -> Result<(), RemoteError> {
        Err(unsupported(
            RemoteOperation::ResolveReviewThread,
            "Forgejo does not expose conversation resolution through its public API",
        ))
    }

    pub(crate) fn observe_merge_queue(&self) -> Result<Observation<QueueState>, RemoteError> {
        Err(unsupported(
            RemoteOperation::ObserveMergeQueue,
            "Forgejo does not expose a merge queue",
        ))
    }

    fn project(
        &self,
        repository: &RemoteRepositoryId,
        operation: RemoteOperation,
    ) -> Result<String, RemoteError> {
        if repository.provider() != ProviderKind::Forgejo || repository.host() != &self.profile.host
        {
            return Err(validation_error(
                operation,
                "repository does not belong to the configured Forgejo profile",
            ));
        }
        let mut components = repository.project_path().split('/');
        let (Some(owner), Some(name), None) =
            (components.next(), components.next(), components.next())
        else {
            return Err(validation_error(
                operation,
                "Forgejo repository path must contain exactly an owner and repository",
            ));
        };
        Ok(format!("{}/{}", path_segment(owner), path_segment(name)))
    }

    fn change_request_route(
        &self,
        id: &ChangeRequestId,
        operation: RemoteOperation,
    ) -> Result<(String, u64), RemoteError> {
        let project = self.project(id.repository(), operation)?;
        let number = id.display_number().ok_or_else(|| {
            validation_error(
                operation,
                "Forgejo change request is missing its repository-local number",
            )
        })?;
        Ok((project, number))
    }

    fn summary(
        &self,
        repository: &RemoteRepositoryId,
        pull: PullRequestResponse,
        operation: RemoteOperation,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        let id = pull
            .id
            .ok_or_else(|| invalid_response(operation, "Forgejo pull request omitted its ID"))?;
        let number = pull.number.ok_or_else(|| {
            invalid_response(operation, "Forgejo pull request omitted its number")
        })?;
        let head = pull.head.ok_or_else(|| {
            invalid_response(operation, "Forgejo pull request omitted its source branch")
        })?;
        let base = pull.base.ok_or_else(|| {
            invalid_response(operation, "Forgejo pull request omitted its target branch")
        })?;
        let source_repository = self.repository_from_branch(&head, operation)?;
        let target_repository = self.repository_from_branch(&base, operation)?;
        if &target_repository != repository {
            return Err(invalid_response(
                operation,
                "Forgejo pull request target repository did not match the request",
            ));
        }
        if head.sha.is_empty() || head.branch_ref.is_empty() || base.branch_ref.is_empty() {
            return Err(invalid_response(
                operation,
                "Forgejo pull request omitted branch or head information",
            ));
        }
        let native_lifecycle = pull.state.clone();
        let native_mergeability = pull.mergeable.map(|value| value.to_string());
        let lifecycle = if pull.merged {
            LifecycleState::Merged
        } else {
            LifecycleState::from_native(pull.state)
        };
        let mergeability = match pull.mergeable {
            Some(true) => MergeabilityState::Mergeable,
            Some(false) => MergeabilityState::Conflicting,
            None => MergeabilityState::Unknown("not_reported".to_string()),
        };
        let review_decision = review_decision_from_requested_reviewers(&pull.requested_reviewers);
        let requested_reviewers = pull
            .requested_reviewers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|reviewer| reviewer.login.clone())
            .collect();
        let change_request = ChangeRequest {
            id: ChangeRequestId::new(
                repository.clone(),
                NativeChangeRequestId::new(id.to_string()).map_err(|_| {
                    invalid_response(operation, "Forgejo pull request ID was invalid")
                })?,
                Some(number),
            ),
            source_repository,
            target_repository,
            source_branch: head.branch_ref,
            target_branch: base.branch_ref,
            head_sha: head.sha,
        };
        Ok(ChangeRequestSummary {
            change_request,
            title: pull.title,
            author: pull.user.map(|user| user.login).unwrap_or_default(),
            body: pull.body,
            web_url: nonempty(pull.html_url),
            lifecycle,
            review_decision,
            requested_reviewers,
            mergeability,
            check_state: CheckState::Unknown("not_observed".to_string()),
            queue_state: QueueState::Unknown("unsupported".to_string()),
            native_state_evidence: NativeStateEvidence {
                lifecycle: NativeStateEvidence::retain([
                    native_lifecycle,
                    format!("merged={}", pull.merged),
                ]),
                mergeability: NativeStateEvidence::retain(native_mergeability),
                ..NativeStateEvidence::default()
            },
            comment_count: 0,
            draft: pull.draft,
            updated_at: nonempty(pull.updated_at),
        })
    }

    fn enrich_summary(
        &self,
        mut summary: ChangeRequestSummary,
        project: &str,
        actions_enabled: Option<bool>,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        if let Some(number) = summary.change_request.id.display_number() {
            match self.load_reviews(project, number) {
                Ok(reviews) => {
                    summary.native_state_evidence.review = NativeStateEvidence::retain(
                        reviews.iter().flat_map(forgejo_review_evidence),
                    );
                    summary.review_decision = summarize_reviews(&reviews, summary.review_decision);
                }
                Err(remote_error) if remote_error.class() == RemoteErrorClass::Cancelled => {
                    return Err(remote_error);
                }
                Err(_) => {}
            }
        }
        let checks = self.observe_checks_with_actions(
            project,
            &summary.change_request.head_sha,
            actions_enabled,
        );
        if let Observation::Failed(remote_error) = &checks
            && remote_error.class() == RemoteErrorClass::Cancelled
        {
            return Err(remote_error.clone());
        }
        summary.check_state = summarize_check_observation(&checks);
        if let Some(checks) = checks.known() {
            summary.native_state_evidence.check =
                NativeStateEvidence::retain(checks.iter().map(|check| check.native_state.clone()));
        }
        Ok(summary)
    }

    fn repository_from_branch(
        &self,
        branch: &PullBranchResponse,
        operation: RemoteOperation,
    ) -> Result<RemoteRepositoryId, RemoteError> {
        let repository = branch.repository.as_ref().ok_or_else(|| {
            invalid_response(
                operation,
                "Forgejo pull request omitted repository identity",
            )
        })?;
        RemoteRepositoryId::new(
            ProviderKind::Forgejo,
            self.profile.host.clone(),
            &repository.full_name,
        )
        .map_err(|_| invalid_response(operation, "Forgejo returned an invalid repository identity"))
    }

    fn observe_comments(&self, project: &str, number: u64) -> Observation<Vec<Comment>> {
        let pagination = match self.pagination(RemoteOperation::ObserveChangeRequest) {
            Ok(pagination) => pagination,
            Err(remote_error) => return Observation::Failed(remote_error),
        };
        observation(
            self.client
                .get_json_pages::<CommentResponse>(
                    RemoteOperation::ObserveChangeRequest,
                    &format!("repos/{project}/issues/{number}/comments"),
                    &[("limit", pagination.limit), ("page", "1".to_string())],
                )
                .map(|comments| comments.into_iter().map(map_comment).collect()),
            false,
        )
    }

    fn load_reviews(&self, project: &str, number: u64) -> Result<Vec<ReviewResponse>, RemoteError> {
        let pagination = self.pagination(RemoteOperation::ObserveChangeRequest)?;
        self.client.get_json_pages::<ReviewResponse>(
            RemoteOperation::ObserveChangeRequest,
            &format!("repos/{project}/pulls/{number}/reviews"),
            &[("limit", pagination.limit), ("page", "1".to_string())],
        )
    }

    fn load_review_threads(
        &self,
        project: &str,
        number: u64,
        reviews: &[ReviewResponse],
    ) -> Result<Vec<ReviewThread>, RemoteError> {
        if reviews.len() > MAX_REVIEW_COMMENT_REQUESTS {
            return Err(invalid_response(
                RemoteOperation::ObserveReviewThreads,
                "Forgejo review count exceeded the review-comment request limit",
            ));
        }
        let mut threads = Vec::new();
        let pagination = self.pagination(RemoteOperation::ObserveReviewThreads)?;
        for review in reviews {
            let comments = self.client.get_json_pages::<ReviewCommentResponse>(
                RemoteOperation::ObserveReviewThreads,
                &format!(
                    "repos/{project}/pulls/{number}/reviews/{}/comments",
                    review.id
                ),
                &[
                    ("limit", pagination.limit.clone()),
                    ("page", "1".to_string()),
                ],
            )?;
            for comment in comments {
                let id = NativeReviewThreadId::new(comment.id.to_string()).map_err(|_| {
                    invalid_response(
                        RemoteOperation::ObserveReviewThreads,
                        "Forgejo review comment ID was invalid",
                    )
                })?;
                threads.push(ReviewThread {
                    native_id: id,
                    resolvable: false,
                    resolved: comment.resolver.is_some(),
                    comments: vec![Comment {
                        native_id: comment.id.to_string(),
                        author: comment.user.map(|user| user.login).unwrap_or_default(),
                        body: comment.body,
                        created_at: nonempty(comment.created_at),
                        path: nonempty(comment.path),
                        line: comment.line,
                    }],
                });
            }
        }
        Ok(threads)
    }

    fn observe_changed_files(&self, project: &str, number: u64) -> Observation<Vec<String>> {
        let pagination = match self.pagination(RemoteOperation::ObserveChangedFiles) {
            Ok(pagination) => pagination,
            Err(remote_error) => return Observation::Failed(remote_error),
        };
        observation(
            self.client
                .get_json_pages::<ChangedFileResponse>(
                    RemoteOperation::ObserveChangedFiles,
                    &format!("repos/{project}/pulls/{number}/files"),
                    &[("limit", pagination.limit), ("page", "1".to_string())],
                )
                .map(|files| files.into_iter().map(|file| file.filename).collect()),
            true,
        )
    }

    fn observe_checks(&self, project: &str, head_sha: &str) -> Observation<Vec<CheckContext>> {
        self.observe_checks_with_actions(project, head_sha, None)
    }

    fn observe_checks_with_actions(
        &self,
        project: &str,
        head_sha: &str,
        actions_enabled: Option<bool>,
    ) -> Observation<Vec<CheckContext>> {
        let statuses = self.load_statuses(project, head_sha);
        if actions_enabled == Some(false) {
            return observation(statuses, true);
        }
        let actions = self.load_action_runs(project, head_sha);
        checks_observation(statuses, actions.as_ref().map(Vec::as_slice))
    }

    fn load_statuses(
        &self,
        project: &str,
        head_sha: &str,
    ) -> Result<Vec<CheckContext>, RemoteError> {
        let pagination = self.pagination(RemoteOperation::ObserveChecks)?;
        self.client
            .get_json_pages::<CommitStatusResponse>(
                RemoteOperation::ObserveChecks,
                &format!(
                    "repos/{project}/commits/{}/statuses",
                    path_segment(head_sha)
                ),
                &[("limit", pagination.limit), ("page", "1".to_string())],
            )
            .map(|statuses| {
                statuses
                    .into_iter()
                    .map(|status| CheckContext {
                        name: status.context,
                        state: CheckState::from_native(status.status.clone()),
                        native_state: status.status,
                        web_url: nonempty(status.target_url),
                    })
                    .collect()
            })
    }

    fn observe_actions(&self, project: &str, head_sha: &str) -> Observation<Vec<CiFailure>> {
        observation(self.load_action_failures(project, head_sha), true)
    }

    fn load_action_failures(
        &self,
        project: &str,
        head_sha: &str,
    ) -> Result<Vec<CiFailure>, RemoteError> {
        let runs = self.load_action_runs(project, head_sha)?;
        self.load_action_failures_for_runs(project, runs)
    }

    fn load_action_runs(
        &self,
        project: &str,
        head_sha: &str,
    ) -> Result<Vec<ActionRunResponse>, RemoteError> {
        let pagination = self.pagination(RemoteOperation::LoadCiLogs)?;
        let page_size = pagination.limit;
        let mut runs = Vec::new();
        let mut observed_runs = 0_u64;
        for page in 1..=MAX_ACTION_PAGES {
            let response = self.client.get_json::<ActionRunsResponse>(
                RemoteOperation::LoadCiLogs,
                &format!("repos/{project}/actions/runs"),
                &[
                    ("head_sha", head_sha.to_string()),
                    ("limit", page_size.clone()),
                    ("page", page.to_string()),
                ],
            )?;
            let total_count = response.total_count.ok_or_else(|| {
                invalid_response(
                    RemoteOperation::LoadCiLogs,
                    "Forgejo Actions response omitted total_count",
                )
            })?;
            let count = response.workflow_runs.len() as u64;
            observed_runs = observed_runs.saturating_add(count);
            runs.extend(
                response
                    .workflow_runs
                    .into_iter()
                    .filter(|run| run.commit_sha == head_sha),
            );
            if observed_runs >= total_count {
                break;
            }
            if count == 0 {
                return Err(invalid_response(
                    RemoteOperation::LoadCiLogs,
                    "Forgejo Actions pagination ended before total_count",
                ));
            }
            if page == MAX_ACTION_PAGES {
                return Err(invalid_response(
                    RemoteOperation::LoadCiLogs,
                    "Forgejo Actions pagination exceeded the page limit",
                ));
            }
        }
        Ok(runs)
    }

    fn load_action_failures_for_runs(
        &self,
        project: &str,
        runs: Vec<ActionRunResponse>,
    ) -> Result<Vec<CiFailure>, RemoteError> {
        let pagination = self.pagination(RemoteOperation::LoadCiLogs)?;
        let mut failures = Vec::new();
        for run in runs.into_iter().filter(|run| failed_state(&run.status)) {
            let jobs = self.client.get_json_pages::<ActionJobResponse>(
                RemoteOperation::LoadCiLogs,
                &format!("repos/{project}/actions/runs/{}/jobs", run.id),
                &[
                    ("limit", pagination.limit.clone()),
                    ("page", "1".to_string()),
                ],
            )?;
            for job in jobs.into_iter().filter(|job| failed_state(&job.status)) {
                if failures.len() == MAX_FAILED_JOBS {
                    return Err(invalid_response(
                        RemoteOperation::LoadCiLogs,
                        "Forgejo failed job count exceeded the log request limit",
                    ));
                }
                let log = self.client.get_bytes(
                    RemoteOperation::LoadCiLogs,
                    &format!("repos/{project}/actions/jobs/{}/logs", job.id),
                    &[],
                );
                if let Err(remote_error) = &log
                    && remote_error.class() == RemoteErrorClass::Cancelled
                {
                    return Err(remote_error.clone());
                }
                failures.push(CiFailure {
                    pipeline: if run.workflow_id.is_empty() {
                        run.title.clone()
                    } else {
                        run.workflow_id.clone()
                    },
                    job: job.name,
                    native_conclusion: job.status,
                    web_url: nonempty(run.html_url.clone()),
                    native_run_id: run.id.to_string(),
                    // The failed job remains useful evidence when hosted logs are disabled,
                    // external, expired, or forbidden for the current credential.
                    log_tail: log
                        .as_deref()
                        .map(|bytes| utf8_tail(bytes, LOG_TAIL_BYTES))
                        .unwrap_or_default(),
                });
            }
        }
        Ok(failures)
    }

    fn verify_branch_head(
        &self,
        project: &str,
        branch: &str,
        expected_sha: &str,
    ) -> Result<(), RemoteError> {
        let response = self.client.get_json::<BranchResponse>(
            RemoteOperation::CreateChangeRequest,
            &format!("repos/{project}/branches/{}", path_segment(branch)),
            &[],
        )?;
        if response.commit.id != expected_sha {
            return Err(stale_head_error(RemoteOperation::CreateChangeRequest, None));
        }
        Ok(())
    }

    fn require_supported_mutations(&self, operation: RemoteOperation) -> Result<(), RemoteError> {
        let instance = self.discover_instance_for(operation)?;
        let major = forgejo_major(&instance.version).ok_or_else(|| {
            invalid_response(
                operation,
                "Forgejo returned a malformed version for mutation safety",
            )
        })?;
        if !(MINIMUM_MUTATION_MAJOR..=MAXIMUM_MUTATION_MAJOR).contains(&major) {
            return Err(unsupported(
                operation,
                "Forgejo mutations require a verified supported server version",
            ));
        }
        Ok(())
    }

    fn pagination(&self, operation: RemoteOperation) -> Result<Pagination, RemoteError> {
        let settings = self.discover_instance_for(operation)?.settings;
        let limit = settings
            .max_response_items
            .or(settings.default_paging_num)
            .unwrap_or(PAGE_SIZE);
        if limit == 0 {
            return Err(invalid_response(
                operation,
                "Forgejo reported an invalid zero page limit",
            ));
        }
        Ok(Pagination {
            limit: PAGE_SIZE.min(limit).to_string(),
        })
    }

    fn instance_cache_key(&self) -> String {
        self.profile.api_base.to_string()
    }

    fn cached_instance(&self) -> Option<ForgejoInstance> {
        let key = self.instance_cache_key();
        let mut cache = instance_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fresh = cache.get(&key).is_some_and(|instance| {
            SystemTime::now()
                .duration_since(instance.observed_at)
                .is_ok_and(|age| age <= INSTANCE_CACHE_TTL)
        });
        if fresh {
            cache.get(&key).cloned()
        } else {
            cache.remove(&key);
            None
        }
    }
}

struct Pagination {
    limit: String,
}

fn instance_cache() -> &'static Mutex<HashMap<String, ForgejoInstance>> {
    INSTANCE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn observation<T>(
    result: Result<Vec<T>, RemoteError>,
    not_found_is_unsupported: bool,
) -> Observation<Vec<T>> {
    match result {
        Ok(values) if values.is_empty() => Observation::EmptyKnown,
        Ok(values) => Observation::Known(values),
        Err(remote_error)
            if not_found_is_unsupported && remote_error.class() == RemoteErrorClass::NotFound =>
        {
            Observation::Unsupported
        }
        Err(remote_error) if remote_error.class() == RemoteErrorClass::Authentication => {
            Observation::Unauthorized
        }
        Err(remote_error) if remote_error.class() == RemoteErrorClass::Authorization => {
            Observation::Unauthorized
        }
        Err(remote_error) => Observation::Failed(remote_error),
    }
}

fn checks_observation(
    statuses: Result<Vec<CheckContext>, RemoteError>,
    actions: Result<&[ActionRunResponse], &RemoteError>,
) -> Observation<Vec<CheckContext>> {
    if let Err(error) = &statuses
        && error.class() == RemoteErrorClass::Cancelled
    {
        return Observation::Failed(error.clone());
    }
    if let Err(error) = &actions
        && error.class() == RemoteErrorClass::Cancelled
    {
        return Observation::Failed((**error).clone());
    }
    let mut checks = Vec::new();
    let mut successful_sources = 0;
    let mut unsupported_sources = 0;
    let mut source_error = None;
    match statuses {
        Ok(statuses) => {
            successful_sources += 1;
            checks.extend(statuses);
        }
        Err(remote_error) if check_source_is_unsupported(&remote_error) => {
            unsupported_sources += 1;
        }
        Err(remote_error) => source_error = Some(remote_error),
    }
    match actions {
        Ok(runs) => {
            successful_sources += 1;
            checks.extend(runs.iter().map(action_check));
        }
        Err(remote_error) if check_source_is_unsupported(remote_error) => {
            unsupported_sources += 1;
        }
        Err(remote_error) => {
            source_error.get_or_insert_with(|| (*remote_error).clone());
        }
    }
    if let Some(error) = source_error {
        if checks.is_empty() {
            Observation::Failed(error)
        } else {
            Observation::Stale {
                value: checks,
                error: Some(error),
            }
        }
    } else if successful_sources > 0 {
        known_vec(checks)
    } else if unsupported_sources > 0 {
        Observation::Unsupported
    } else {
        Observation::NotLoaded
    }
}

fn check_source_is_unsupported(error: &RemoteError) -> bool {
    matches!(
        error.class(),
        RemoteErrorClass::NotFound | RemoteErrorClass::Unsupported
    )
}

fn known_vec<T>(values: Vec<T>) -> Observation<Vec<T>> {
    if values.is_empty() {
        Observation::EmptyKnown
    } else {
        Observation::Known(values)
    }
}

fn actions_enabled(pull: &PullRequestResponse) -> Option<bool> {
    pull.base
        .as_ref()
        .and_then(|branch| branch.repository.as_ref())
        .and_then(|repository| repository.has_actions)
}

fn action_check(run: &ActionRunResponse) -> CheckContext {
    let name = nonempty(run.workflow_id.clone())
        .or_else(|| nonempty(run.title.clone()))
        .unwrap_or_else(|| format!("Forgejo Actions run {}", run.id));
    CheckContext {
        name,
        state: forgejo_action_state(&run.status),
        native_state: run.status.clone(),
        web_url: nonempty(run.html_url.clone()),
    }
}

fn forgejo_action_state(native: &str) -> CheckState {
    match native.trim().to_ascii_lowercase().as_str() {
        "waiting" | "requested" => CheckState::Pending,
        _ => CheckState::from_native(native.to_string()),
    }
}

fn summarize_check_observation(checks: &Observation<Vec<CheckContext>>) -> CheckState {
    let checks = match checks {
        Observation::Known(checks) => checks,
        Observation::EmptyKnown | Observation::AuthoritativelyAbsent => {
            return CheckState::Skipped;
        }
        Observation::Stale { value, .. } => {
            let state = summarize_checks(value);
            return if state == CheckState::Failed {
                CheckState::Failed
            } else {
                CheckState::Unknown("incomplete_observation".to_string())
            };
        }
        _ => return CheckState::Unknown("not_observed".to_string()),
    };
    summarize_checks(checks)
}

fn summarize_checks(checks: &[CheckContext]) -> CheckState {
    if checks.is_empty() {
        return CheckState::Unknown("no_checks".to_string());
    }
    let states = checks.iter().map(|check| &check.state).collect::<Vec<_>>();
    if states
        .iter()
        .any(|state| matches!(state, CheckState::Failed))
    {
        CheckState::Failed
    } else if let Some(CheckState::Unknown(native)) = states
        .iter()
        .find(|state| matches!(state, CheckState::Unknown(_)))
        .copied()
    {
        CheckState::Unknown(native.clone())
    } else if states
        .iter()
        .any(|state| matches!(state, CheckState::Pending))
    {
        CheckState::Pending
    } else if states
        .iter()
        .all(|state| matches!(state, CheckState::Passed | CheckState::Skipped))
    {
        CheckState::Passed
    } else if states
        .iter()
        .all(|state| matches!(state, CheckState::Cancelled))
    {
        CheckState::Cancelled
    } else {
        CheckState::Mixed
    }
}

fn review_decision_from_requested_reviewers(
    requested_reviewers: &Option<Vec<UserResponse>>,
) -> ReviewDecision {
    match requested_reviewers {
        Some(reviewers) if !reviewers.is_empty() => ReviewDecision::ReviewRequired,
        Some(_) => ReviewDecision::Unknown("no_reviews".to_string()),
        None => ReviewDecision::Unknown("requested_reviewers_not_reported".to_string()),
    }
}

fn summarize_reviews(reviews: &[ReviewResponse], requested: ReviewDecision) -> ReviewDecision {
    let decisions = reviews.iter().map(map_review).map(|review| review.decision);
    let decisions = decisions.collect::<Vec<_>>();
    if decisions
        .iter()
        .any(|decision| matches!(decision, ReviewDecision::ChangesRequested))
    {
        ReviewDecision::ChangesRequested
    } else if matches!(requested, ReviewDecision::ReviewRequired) {
        ReviewDecision::ReviewRequired
    } else if decisions
        .iter()
        .any(|decision| matches!(decision, ReviewDecision::Approved))
    {
        ReviewDecision::Approved
    } else if decisions
        .iter()
        .any(|decision| matches!(decision, ReviewDecision::Unknown(native) if native == "stale" || native == "review_freshness_not_reported"))
    {
        ReviewDecision::Unknown("stale_or_unverified".to_string())
    } else if decisions
        .iter()
        .any(|decision| matches!(decision, ReviewDecision::Pending))
    {
        ReviewDecision::Pending
    } else {
        requested
    }
}

fn map_comment(comment: CommentResponse) -> Comment {
    Comment {
        native_id: comment.id.to_string(),
        author: comment.user.map(|user| user.login).unwrap_or_default(),
        body: comment.body,
        created_at: nonempty(comment.created_at),
        path: None,
        line: None,
    }
}

fn map_review(review: &ReviewResponse) -> Review {
    Review {
        native_id: review.id.to_string(),
        author: review
            .user
            .as_ref()
            .map(|user| user.login.clone())
            .unwrap_or_default(),
        decision: if review.dismissed == Some(true) {
            ReviewDecision::Dismissed
        } else if review.stale == Some(true) {
            ReviewDecision::Unknown("stale".to_string())
        } else if review.dismissed == Some(false) && review.stale == Some(false) {
            ReviewDecision::from_native(review.state.clone())
        } else {
            match ReviewDecision::from_native(review.state.clone()) {
                ReviewDecision::Approved | ReviewDecision::ChangesRequested => {
                    ReviewDecision::Unknown("review_freshness_not_reported".to_string())
                }
                decision => decision,
            }
        },
        body: review.body.clone(),
        submitted_at: nonempty(review.submitted_at.clone()),
    }
}

fn forgejo_review_evidence(review: &ReviewResponse) -> Vec<String> {
    let mut evidence = vec![review.state.clone()];
    if let Some(dismissed) = review.dismissed {
        evidence.push(format!("dismissed={dismissed}"));
    }
    if let Some(stale) = review.stale {
        evidence.push(format!("stale={stale}"));
    }
    evidence
}

fn ensure_details_association(
    observed: &ChangeRequest,
    expected: &ChangeRequest,
) -> Result<(), RemoteError> {
    if observed.id == expected.id
        && observed.head_sha == expected.head_sha
        && observed.source_repository == expected.source_repository
        && observed.target_repository == expected.target_repository
        && observed.source_branch == expected.source_branch
        && observed.target_branch == expected.target_branch
    {
        Ok(())
    } else {
        Err(error(
            RemoteOperation::ObserveChangeRequest,
            RemoteErrorClass::StaleHead,
            Retryability::NotRetryable,
            "Forgejo pull request association changed while details were loaded",
        )
        .with_retry_hint(RetryHint::RefreshObservation))
    }
}

fn failed_state(state: &str) -> bool {
    matches!(
        state.to_ascii_lowercase().as_str(),
        "failure" | "failed" | "error"
    )
}

fn utf8_tail(bytes: &[u8], limit: usize) -> String {
    let start = bytes.len().saturating_sub(limit);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

fn nonempty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn forgejo_major(version: &str) -> Option<u64> {
    let mut components = version.trim().splitn(3, '.');
    let major = components.next()?.parse().ok()?;
    components.next()?.parse::<u64>().ok()?;
    let patch = components.next()?;
    let patch_digits = patch.bytes().take_while(u8::is_ascii_digit).count();
    if patch_digits == 0 || patch[..patch_digits].parse::<u64>().is_err() {
        return None;
    }
    let suffix = &patch[patch_digits..];
    if !suffix.is_empty()
        && (!matches!(suffix.as_bytes()[0], b'-' | b'+')
            || suffix.len() == 1
            || !suffix[1..]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+')))
    {
        return None;
    }
    Some(major)
}

fn path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

fn error(
    operation: RemoteOperation,
    class: RemoteErrorClass,
    retryability: Retryability,
    message: &str,
) -> RemoteError {
    RemoteError::new(
        ProviderKind::Forgejo,
        operation,
        class,
        retryability,
        message,
    )
}

fn validation_error(operation: RemoteOperation, message: &str) -> RemoteError {
    error(
        operation,
        RemoteErrorClass::Validation,
        Retryability::NotRetryable,
        message,
    )
}

fn invalid_response(operation: RemoteOperation, message: &str) -> RemoteError {
    error(
        operation,
        RemoteErrorClass::InvalidResponse,
        Retryability::NotRetryable,
        message,
    )
}

fn stale_head_error(operation: RemoteOperation, status: Option<u16>) -> RemoteError {
    let mut remote_error = error(
        operation,
        RemoteErrorClass::StaleHead,
        Retryability::NotRetryable,
        "Forgejo change request head no longer matches the expected commit",
    )
    .with_retry_hint(RetryHint::RefreshObservation);
    if let Some(status) = status {
        remote_error = remote_error.with_status(status);
    }
    remote_error
}

fn unsupported(operation: RemoteOperation, message: &str) -> RemoteError {
    error(
        operation,
        RemoteErrorClass::Unsupported,
        Retryability::NotRetryable,
        message,
    )
}

#[derive(Deserialize)]
struct VersionResponse {
    #[serde(default)]
    version: String,
}

#[derive(Deserialize)]
struct PullRequestResponse {
    id: Option<u64>,
    number: Option<u64>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    merged: bool,
    mergeable: Option<bool>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    updated_at: String,
    user: Option<UserResponse>,
    requested_reviewers: Option<Vec<UserResponse>>,
    head: Option<PullBranchResponse>,
    base: Option<PullBranchResponse>,
}

#[derive(Deserialize)]
struct PullBranchResponse {
    #[serde(default, rename = "ref")]
    branch_ref: String,
    #[serde(default)]
    sha: String,
    #[serde(rename = "repo")]
    repository: Option<RepositoryResponse>,
}

#[derive(Deserialize)]
struct RepositoryResponse {
    #[serde(default)]
    full_name: String,
    id: Option<u64>,
    has_actions: Option<bool>,
}

#[derive(Clone, Deserialize)]
struct UserResponse {
    #[serde(default)]
    login: String,
}

#[derive(Deserialize)]
struct CommentResponse {
    id: u64,
    #[serde(default)]
    body: String,
    #[serde(default)]
    created_at: String,
    user: Option<UserResponse>,
}

#[derive(Clone, Deserialize)]
struct ReviewResponse {
    id: u64,
    #[serde(default)]
    body: String,
    #[serde(default)]
    state: String,
    dismissed: Option<bool>,
    stale: Option<bool>,
    #[serde(default)]
    submitted_at: String,
    user: Option<UserResponse>,
}

#[derive(Deserialize)]
struct ReviewCommentResponse {
    id: u64,
    #[serde(default)]
    body: String,
    #[serde(default)]
    created_at: String,
    user: Option<UserResponse>,
    resolver: Option<UserResponse>,
    #[serde(default)]
    path: String,
    line: Option<u64>,
}

#[derive(Deserialize)]
struct ChangedFileResponse {
    #[serde(default)]
    filename: String,
}

#[derive(Deserialize)]
struct CommitStatusResponse {
    #[serde(default)]
    context: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    target_url: String,
}

#[derive(Deserialize)]
struct BranchProtectionResponse {
    enable_status_check: Option<bool>,
    status_check_contexts: Option<Vec<String>>,
    required_approvals: Option<u32>,
    block_on_outdated_branch: Option<bool>,
}

#[derive(Deserialize)]
struct ActionRunsResponse {
    total_count: Option<u64>,
    #[serde(default)]
    workflow_runs: Vec<ActionRunResponse>,
}

#[derive(Clone, Deserialize)]
struct ActionRunResponse {
    id: u64,
    #[serde(default)]
    commit_sha: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    workflow_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    html_url: String,
}

#[derive(Deserialize)]
struct ActionJobResponse {
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
}

#[derive(Deserialize)]
struct BranchResponse {
    commit: BranchCommitResponse,
}

#[derive(Deserialize)]
struct BranchCommitResponse {
    #[serde(default)]
    id: String,
}

#[derive(Serialize)]
struct CreatePullRequestBody<'a> {
    base: &'a str,
    head: &'a str,
    title: &'a str,
    body: &'a str,
}

#[derive(Serialize)]
struct MergePullRequestBody<'a> {
    #[serde(rename = "Do")]
    operation: &'a str,
    head_commit_id: &'a str,
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Instant;

    use super::*;
    use crate::remote::{HostIdentity, RemoteBase, RemoteDiscovery, WebScheme};

    struct TestServer {
        address: String,
        requests: Arc<Mutex<Vec<String>>>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn start(responses: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&requests);
            let thread = thread::spawn(move || {
                let mut responses = VecDeque::from(responses);
                while let Some(response) = responses.pop_front() {
                    let (mut stream, _) = listener.accept().unwrap();
                    captured.lock().unwrap().push(read_request(&mut stream));
                    stream.write_all(response.as_bytes()).unwrap();
                }
            });
            Self {
                address,
                requests,
                thread: Some(thread),
            }
        }

        fn profile(&self, credential: Option<&str>) -> HostProfile {
            let host = HostIdentity::parse(&self.address).unwrap();
            let base = RemoteBase::new(WebScheme::Http, host.clone(), "api/v1").unwrap();
            let mut profile = HostProfile::new(host, ProviderKind::Forgejo)
                .unwrap()
                .with_http_allowed(true)
                .with_bases(base.clone(), base);
            if let Some(credential) = credential {
                profile = profile.with_credential_environment(credential).unwrap();
            }
            profile
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }

    fn read_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            let count = stream.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end + 4]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if bytes.len() >= end + 4 + content_length {
                    break;
                }
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    fn response(status: &str, headers: &[(&str, &str)], body: &str) -> String {
        let mut response = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
        for (name, value) in headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str("Connection: close\r\n\r\n");
        response.push_str(body);
        response
    }

    #[tokio::test]
    async fn discovers_codeberg_shaped_version_and_settings() {
        let server = TestServer::start(vec![
            response(
                "200 OK",
                &[],
                include_str!("../../tests/fixtures/remote/forgejo/version-codeberg.json"),
            ),
            response(
                "200 OK",
                &[],
                include_str!("../../tests/fixtures/remote/forgejo/settings-codeberg.json"),
            ),
        ]);
        let adapter = ForgejoAdapter::new(server.profile(None)).unwrap();
        let instance = adapter.discover_instance().unwrap();
        assert_eq!(instance.version, "16.0.0-dev+gitea-1.22.0");
        assert_eq!(instance.settings.max_response_items, Some(50));
        let requests = server.requests();
        assert!(requests[0].starts_with("GET /api/v1/version HTTP/1.1"));
        assert!(requests[1].starts_with("GET /api/v1/settings/api HTTP/1.1"));
    }

    #[tokio::test]
    async fn caches_discovery_across_adapters_and_uses_the_discovered_page_limit() {
        let server = TestServer::start(vec![
            response("200 OK", &[], r#"{"version":"11.0.0"}"#),
            response("200 OK", &[], r#"{"max_response_items":7}"#),
            response("200 OK", &[], "[]"),
            response("200 OK", &[], "[]"),
        ]);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();

        let first = ForgejoAdapter::new(profile.clone()).unwrap();
        first.list_change_requests(&repository).unwrap();
        let discovered = first.discover_instance().unwrap();
        assert!(discovered.observed_at <= SystemTime::now());
        assert_eq!(first.capabilities().guarded_merge, SupportLevel::Supported);

        let second = ForgejoAdapter::new(profile).unwrap();
        second.list_change_requests(&repository).unwrap();
        let requests = server.requests();
        assert_eq!(requests.len(), 4);
        assert!(requests[2].contains("state=open"));
        assert!(requests[2].contains("limit=7"));
        assert!(requests[3].contains("state=open"));
        assert!(requests[3].contains("limit=7"));
    }

    #[tokio::test]
    async fn supported_forgejo_major_version_fixtures_remain_forward_compatible() {
        for (fixture, expected_major) in [
            (
                include_str!("../../tests/fixtures/remote/forgejo/version-9.json"),
                9,
            ),
            (
                include_str!("../../tests/fixtures/remote/forgejo/version-current.json"),
                11,
            ),
            (
                include_str!("../../tests/fixtures/remote/forgejo/version-codeberg.json"),
                16,
            ),
        ] {
            let response: VersionResponse = serde_json::from_str(fixture).unwrap();
            assert_eq!(forgejo_major(&response.version), Some(expected_major));
        }
        assert_eq!(forgejo_major("not-a-version"), None);
        assert_eq!(forgejo_major("9"), None);
    }

    #[tokio::test]
    async fn maps_fork_source_target_and_exact_head_from_fixture() {
        let fixture = include_str!("../../tests/fixtures/remote/forgejo/pull-fork.json");
        let server = TestServer::start(vec![
            response(
                "200 OK",
                &[],
                include_str!("../../tests/fixtures/remote/forgejo/version-current.json"),
            ),
            response(
                "200 OK",
                &[],
                include_str!("../../tests/fixtures/remote/forgejo/settings-codeberg.json"),
            ),
            response("200 OK", &[], &format!("[{fixture}]")),
            response("200 OK", &[], "[]"),
            response("200 OK", &[], "[]"),
            response("200 OK", &[], r#"{"total_count":0,"workflow_runs":[]}"#),
        ]);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();
        let adapter = ForgejoAdapter::new(profile.clone()).unwrap();
        let summaries = adapter.list_change_requests(&repository).unwrap();
        let change = &summaries[0].change_request;
        assert_eq!(change.head_sha, "abc123");
        assert_eq!(change.source_branch, "topic");
        assert_eq!(change.target_branch, "main");
        assert_eq!(
            change.source_repository.project_path(),
            "contributor/widget"
        );
        assert_eq!(change.target_repository, repository);
        assert_eq!(change.source_repository.host(), &profile.host);
    }

    #[tokio::test]
    async fn list_enriches_only_a_bounded_number_of_open_requests() {
        let pulls = (0..=MAX_LIST_SUMMARY_ENRICHMENTS)
            .map(|index| {
                let number = index + 1;
                let state = if index == 0 { "closed" } else { "open" };
                format!(
                    r#"{{"id":{number},"number":{number},"title":"Change {number}","state":"{state}","merged":false,"mergeable":true,"requested_reviewers":[],"head":{{"ref":"topic-{number}","sha":"sha-{number}","repo":{{"full_name":"acme/widget"}}}},"base":{{"ref":"main","sha":"base","repo":{{"full_name":"acme/widget"}}}}}}"#
                )
            })
            .chain(std::iter::once(
                r#"{"id":99,"number":99,"title":"Extra open change","state":"open","merged":false,"mergeable":true,"requested_reviewers":[],"head":{"ref":"topic-99","sha":"sha-99","repo":{"full_name":"acme/widget"}},"base":{"ref":"main","sha":"base","repo":{"full_name":"acme/widget"}}}"#.to_string(),
            ))
            .collect::<Vec<_>>();
        let mut responses = vec![
            response("200 OK", &[], r#"{"version":"11.0.0"}"#),
            response("200 OK", &[], r#"{"max_response_items":50}"#),
            response("200 OK", &[], &format!("[{}]", pulls.join(","))),
        ];
        for _ in 0..MAX_LIST_SUMMARY_ENRICHMENTS {
            responses.push(response("200 OK", &[], "[]"));
            responses.push(response(
                "200 OK",
                &[],
                r#"[{"context":"external","status":"success"}]"#,
            ));
            responses.push(response(
                "200 OK",
                &[],
                r#"{"total_count":0,"workflow_runs":[]}"#,
            ));
        }
        let server = TestServer::start(responses);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();

        let summaries = ForgejoAdapter::new(profile)
            .unwrap()
            .list_change_requests(&repository)
            .unwrap();

        assert_eq!(summaries.len(), MAX_LIST_SUMMARY_ENRICHMENTS + 2);
        assert_eq!(
            summaries[0].check_state,
            CheckState::Unknown("not_observed".to_string())
        );
        assert!(
            summaries[1..=MAX_LIST_SUMMARY_ENRICHMENTS]
                .iter()
                .all(|summary| summary.check_state == CheckState::Passed)
        );
        assert_eq!(
            summaries[MAX_LIST_SUMMARY_ENRICHMENTS + 1].check_state,
            CheckState::Unknown("not_observed".to_string())
        );
        assert_eq!(
            server.requests().len(),
            3 + MAX_LIST_SUMMARY_ENRICHMENTS * 3
        );
    }

    #[tokio::test]
    async fn sends_configured_token_only_as_a_redacted_header() {
        const ENVIRONMENT: &str = "PRISM_FORGEJO_TEST_TOKEN_HEADER";
        const SECRET: &str = "super-secret-token";
        // Each test uses a unique variable; changing process environment is otherwise isolated.
        unsafe { std::env::set_var(ENVIRONMENT, SECRET) };
        let server = TestServer::start(vec![response("401 Unauthorized", &[], SECRET)]);
        let adapter = ForgejoAdapter::new(server.profile(Some(ENVIRONMENT))).unwrap();
        let error = adapter.discover_instance().unwrap_err();
        unsafe { std::env::remove_var(ENVIRONMENT) };
        let request = &server.requests()[0];
        assert!(
            request
                .to_ascii_lowercase()
                .contains(&format!("authorization: token {SECRET}"))
        );
        assert!(!request.lines().next().unwrap().contains(SECRET));
        assert!(!error.to_string().contains(SECRET));
        assert!(!format!("{error:?}").contains(SECRET));
    }

    #[tokio::test]
    async fn rejects_oversized_and_invalid_json_responses() {
        let oversized = "x".repeat(65);
        let server = TestServer::start(vec![response("200 OK", &[], &oversized)]);
        let adapter = ForgejoAdapter::with_transport_options(
            server.profile(None),
            Duration::from_secs(2),
            64,
        )
        .unwrap();
        let oversized_error = adapter.discover_instance().unwrap_err();
        assert_eq!(oversized_error.class(), RemoteErrorClass::InvalidResponse);

        let chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n41\r\n{}\r\n0\r\n\r\n",
            "x".repeat(65)
        );
        let server = TestServer::start(vec![chunked]);
        let adapter = ForgejoAdapter::with_transport_options(
            server.profile(None),
            Duration::from_secs(2),
            64,
        )
        .unwrap();
        assert_eq!(
            adapter.discover_instance().unwrap_err().class(),
            RemoteErrorClass::InvalidResponse
        );

        let server = TestServer::start(vec![response("200 OK", &[], "not-json")]);
        let adapter = ForgejoAdapter::with_transport_options(
            server.profile(None),
            Duration::from_secs(2),
            64,
        )
        .unwrap();
        let invalid_error = adapter.discover_instance().unwrap_err();
        assert_eq!(invalid_error.class(), RemoteErrorClass::InvalidResponse);
        assert_eq!(
            invalid_error.safe_message(),
            "Forgejo returned invalid JSON"
        );
    }

    #[tokio::test]
    async fn follows_same_origin_pagination_and_rejects_cross_origin_links() {
        let server = TestServer::start(vec![
            response("200 OK", &[], r#"{"version":"11.0.0"}"#),
            response("200 OK", &[], r#"{"max_response_items":50}"#),
            response(
                "200 OK",
                &[(
                    "Link",
                    "</api/v1/repos/acme/widget/pulls?page=2>; rel=\"next\"",
                )],
                "[]",
            ),
            response("200 OK", &[], "[]"),
            response(
                "200 OK",
                &[("Link", "<https://attacker.invalid/steal>; rel=\"next\"")],
                "[]",
            ),
        ]);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();
        let adapter = ForgejoAdapter::new(profile).unwrap();
        assert!(
            adapter
                .list_change_requests(&repository)
                .unwrap()
                .is_empty()
        );
        let error = adapter.list_change_requests(&repository).unwrap_err();
        assert_eq!(error.class(), RemoteErrorClass::InvalidResponse);
        assert_eq!(server.requests().len(), 5);
    }

    #[tokio::test]
    async fn rejects_same_origin_links_outside_the_api_and_incomplete_totals() {
        let server = TestServer::start(vec![
            response("200 OK", &[], r#"{"version":"11.0.0"}"#),
            response("200 OK", &[], r#"{"max_response_items":50}"#),
            response(
                "200 OK",
                &[("Link", "</session/credential-target>; rel=\"next\"")],
                "[]",
            ),
        ]);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();
        let adapter = ForgejoAdapter::new(profile).unwrap();
        assert_eq!(
            adapter
                .list_change_requests(&repository)
                .unwrap_err()
                .class(),
            RemoteErrorClass::InvalidResponse
        );
        assert_eq!(server.requests().len(), 3);

        let server = TestServer::start(vec![
            response("200 OK", &[], r#"{"version":"11.0.0"}"#),
            response("200 OK", &[], r#"{"max_response_items":50}"#),
            response("200 OK", &[("X-Total-Count", "2")], "[]"),
        ]);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();
        let adapter = ForgejoAdapter::new(profile).unwrap();
        assert_eq!(
            adapter
                .list_change_requests(&repository)
                .unwrap_err()
                .class(),
            RemoteErrorClass::InvalidResponse
        );
    }

    #[tokio::test]
    async fn status_page_two_failure_is_an_observation_failure() {
        let server = TestServer::start(vec![
            response("200 OK", &[], r#"{"version":"11.0.0"}"#),
            response("200 OK", &[], r#"{"max_response_items":50}"#),
            response(
                "200 OK",
                &[(
                    "Link",
                    "</api/v1/repos/acme/widget/commits/abc123/statuses?limit=50&page=2>; rel=\"next\"",
                )],
                r#"[{"context":"build","status":"success","target_url":""}]"#,
            ),
            response("503 Service Unavailable", &[], ""),
            response("404 Not Found", &[], ""),
        ]);
        let adapter = ForgejoAdapter::new(server.profile(None)).unwrap();
        let Observation::Failed(error) = adapter.observe_checks("acme/widget", "abc123") else {
            panic!("page two failure must not produce authoritative page-one statuses");
        };
        assert_eq!(error.class(), RemoteErrorClass::Provider);
        let requests = server.requests();
        assert_eq!(requests.len(), 5);
        assert!(requests[2].starts_with(
            "GET /api/v1/repos/acme/widget/commits/abc123/statuses?limit=50&page=1 HTTP/1.1"
        ));
        assert!(requests[3].starts_with(
            "GET /api/v1/repos/acme/widget/commits/abc123/statuses?limit=50&page=2 HTTP/1.1"
        ));
    }

    #[tokio::test]
    async fn classifies_retry_after_and_statuses() {
        let server = TestServer::start(vec![
            response("429 Too Many Requests", &[("Retry-After", "17")], ""),
            response("503 Service Unavailable", &[], ""),
            response("404 Not Found", &[], ""),
        ]);
        let adapter = ForgejoAdapter::new(server.profile(None)).unwrap();
        let rate_limit = adapter.discover_instance().unwrap_err();
        assert_eq!(rate_limit.class(), RemoteErrorClass::RateLimited);
        assert_eq!(
            rate_limit.retry_hint(),
            Some(RetryHint::After(Duration::from_secs(17)))
        );
        let unavailable = adapter.discover_instance().unwrap_err();
        assert_eq!(unavailable.class(), RemoteErrorClass::Provider);
        assert_eq!(unavailable.retryability(), Retryability::Retryable);
        let missing = adapter.discover_instance().unwrap_err();
        assert_eq!(missing.class(), RemoteErrorClass::NotFound);
    }

    #[tokio::test]
    async fn create_fails_closed_when_discovery_fails_without_posting() {
        let server = TestServer::start(vec![response("503 Service Unavailable", &[], "")]);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();
        let adapter = ForgejoAdapter::new(profile).unwrap();
        let error = adapter
            .create_change_request(CreateChangeRequest {
                source_repository: repository.clone(),
                target_repository: repository,
                source_branch: "topic".to_string(),
                target_branch: "main".to_string(),
                expected_head_sha: "abc123".to_string(),
                title: "Change".to_string(),
                body: String::new(),
                draft: false,
            })
            .unwrap_err();
        assert_eq!(error.class(), RemoteErrorClass::Provider);
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests.iter().all(|request| !request.starts_with("POST ")));
    }

    #[tokio::test]
    async fn unverified_versions_block_create_before_posting() {
        for (version, expected_class) in [
            ("not-a-version", RemoteErrorClass::InvalidResponse),
            ("8.0.0", RemoteErrorClass::Unsupported),
            ("17.0.0", RemoteErrorClass::Unsupported),
        ] {
            let server = TestServer::start(vec![
                response("200 OK", &[], &format!(r#"{{"version":"{version}"}}"#)),
                response("200 OK", &[], "{}"),
            ]);
            let profile = server.profile(None);
            let repository =
                RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                    .unwrap();
            let adapter = ForgejoAdapter::new(profile).unwrap();
            let error = adapter
                .create_change_request(CreateChangeRequest {
                    source_repository: repository.clone(),
                    target_repository: repository,
                    source_branch: "topic".to_string(),
                    target_branch: "main".to_string(),
                    expected_head_sha: "abc123".to_string(),
                    title: "Change".to_string(),
                    body: String::new(),
                    draft: false,
                })
                .unwrap_err();
            assert_eq!(error.class(), expected_class);
            let requests = server.requests();
            assert_eq!(requests.len(), 2);
            assert!(requests.iter().all(|request| !request.starts_with("POST ")));
        }
    }

    #[tokio::test]
    async fn actions_pagination_uses_total_count_instead_of_requested_page_size() {
        let server = TestServer::start(vec![
            response("200 OK", &[], r#"{"version":"11.0.0"}"#),
            response("200 OK", &[], r#"{"max_response_items":50}"#),
            response(
                "200 OK",
                &[],
                r#"{"total_count":2,"workflow_runs":[{"id":1,"commit_sha":"abc123","status":"success"}]}"#,
            ),
            response(
                "200 OK",
                &[],
                r#"{"total_count":2,"workflow_runs":[{"id":2,"commit_sha":"abc123","status":"success"}]}"#,
            ),
        ]);
        let adapter = ForgejoAdapter::new(server.profile(None)).unwrap();

        assert!(
            adapter
                .load_action_failures("acme/widget", "abc123")
                .unwrap()
                .is_empty()
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 4);
        assert!(requests[2].contains("page=1"));
        assert!(requests[3].contains("page=2"));
    }

    #[tokio::test]
    async fn retains_status_and_action_states_in_check_evidence() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/remote/forgejo/statuses-and-actions.json"
        ))
        .unwrap();
        let server = TestServer::start(vec![
            response("200 OK", &[], r#"{"version":"11.0.0"}"#),
            response("200 OK", &[], r#"{"max_response_items":50}"#),
            response("200 OK", &[], &fixture["statuses"].to_string()),
            response("200 OK", &[], &fixture["actions"].to_string()),
        ]);
        let adapter = ForgejoAdapter::new(server.profile(None)).unwrap();
        let Observation::Known(checks) = adapter.observe_checks("acme/widget", "abc123") else {
            panic!("status and Actions evidence should remain available");
        };
        assert_eq!(checks.len(), 4);
        assert!(checks.iter().any(|check| check.state == CheckState::Passed));
        assert!(
            checks
                .iter()
                .any(|check| check.state == CheckState::Pending)
        );
        assert!(checks.iter().any(|check| {
            check.state == CheckState::Unknown("waiting_for_device".to_string())
                && check.native_state == "waiting_for_device"
        }));
        assert_eq!(
            summarize_check_observation(&Observation::Known(checks)),
            CheckState::Unknown("waiting_for_device".to_string())
        );
    }

    #[tokio::test]
    async fn combined_checks_preserve_values_as_stale_when_either_source_fails() {
        let status = CheckContext {
            name: "external".to_string(),
            state: CheckState::Passed,
            native_state: "success".to_string(),
            web_url: None,
        };
        let action = ActionRunResponse {
            id: 7,
            commit_sha: "abc123".to_string(),
            status: "success".to_string(),
            workflow_id: "tests.yaml".to_string(),
            title: "Tests".to_string(),
            html_url: String::new(),
        };
        let source_error = error(
            RemoteOperation::ObserveChecks,
            RemoteErrorClass::Provider,
            Retryability::Retryable,
            "source unavailable",
        );

        let Observation::Stale { value, error } =
            checks_observation(Ok(vec![status]), Err(&source_error))
        else {
            panic!("successful statuses plus failed Actions must be stale");
        };
        assert_eq!(value.len(), 1);
        assert_eq!(error.unwrap().class(), RemoteErrorClass::Provider);

        let actions = [action];
        let Observation::Stale { value, error } =
            checks_observation(Err(source_error), Ok(&actions))
        else {
            panic!("successful Actions plus failed statuses must be stale");
        };
        assert_eq!(value.len(), 1);
        assert_eq!(value[0].name, "tests.yaml");
        assert_eq!(error.unwrap().class(), RemoteErrorClass::Provider);
    }

    #[tokio::test]
    async fn stale_passing_checks_do_not_become_a_passing_summary() {
        let observation = Observation::Stale {
            value: vec![CheckContext {
                name: "external".to_string(),
                state: CheckState::Passed,
                native_state: "success".to_string(),
                web_url: None,
            }],
            error: Some(error(
                RemoteOperation::ObserveChecks,
                RemoteErrorClass::Provider,
                Retryability::Retryable,
                "Actions unavailable",
            )),
        };

        assert_eq!(
            summarize_check_observation(&observation),
            CheckState::Unknown("incomplete_observation".to_string())
        );
    }

    #[tokio::test]
    async fn unsupported_actions_do_not_poison_external_status_readiness() {
        let status = CheckContext {
            name: "woodpecker/pr".to_string(),
            state: CheckState::Passed,
            native_state: "success".to_string(),
            web_url: None,
        };
        let missing_actions = error(
            RemoteOperation::LoadCiLogs,
            RemoteErrorClass::NotFound,
            Retryability::NotRetryable,
            "Actions unsupported",
        );

        let observation = checks_observation(Ok(vec![status]), Err(&missing_actions));
        assert!(matches!(observation, Observation::Known(_)));
        assert_eq!(
            summarize_check_observation(&observation),
            CheckState::Passed
        );
    }

    #[tokio::test]
    async fn stale_or_unverifiable_approvals_do_not_become_summary_approval() {
        let mut reviews: Vec<ReviewResponse> = serde_json::from_str(include_str!(
            "../../tests/fixtures/remote/forgejo/reviews-stale.json"
        ))
        .unwrap();
        assert_eq!(
            summarize_reviews(&reviews, ReviewDecision::Unknown("no_reviews".to_string())),
            ReviewDecision::Unknown("stale_or_unverified".to_string())
        );

        let missing_freshness: ReviewResponse =
            serde_json::from_str(r#"{"id":9,"state":"APPROVED","body":"","submitted_at":""}"#)
                .unwrap();
        assert_eq!(
            map_review(&missing_freshness).decision,
            ReviewDecision::Unknown("review_freshness_not_reported".to_string())
        );

        reviews.push(
            serde_json::from_str(
                r#"{"id":10,"state":"APPROVED","dismissed":false,"stale":false,"user":{"login":"fresh-reviewer"}}"#,
            )
            .unwrap(),
        );
        assert_eq!(
            summarize_reviews(&reviews, ReviewDecision::Unknown("no_reviews".to_string())),
            ReviewDecision::Approved
        );
    }

    #[tokio::test]
    async fn folds_fresh_reviews_and_exact_head_checks_into_summary_facts() {
        let pull = r#"{"id":99,"number":7,"title":"Change","state":"open","merged":false,"mergeable":true,"requested_reviewers":[],"head":{"ref":"topic","sha":"abc123","repo":{"full_name":"acme/widget"}},"base":{"ref":"main","sha":"def456","repo":{"full_name":"acme/widget"}}}"#;
        let server = TestServer::start(vec![
            response("200 OK", &[], pull),
            response("200 OK", &[], r#"{"version":"11.0.0"}"#),
            response("200 OK", &[], r#"{"max_response_items":50}"#),
            response(
                "200 OK",
                &[],
                r#"[{"id":41,"state":"APPROVED","dismissed":false,"stale":false,"user":{"login":"reviewer"}}]"#,
            ),
            response(
                "200 OK",
                &[],
                r#"[{"context":"woodpecker/pr","status":"success"}]"#,
            ),
            response(
                "200 OK",
                &[],
                r#"{"total_count":1,"workflow_runs":[{"id":81,"commit_sha":"abc123","status":"success","workflow_id":"test.yaml"}]}"#,
            ),
        ]);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();
        let id = ChangeRequestId::new(
            repository,
            NativeChangeRequestId::new("99").unwrap(),
            Some(7),
        );
        let summary = ForgejoAdapter::new(profile)
            .unwrap()
            .change_request_summary(&id)
            .unwrap();
        assert_eq!(summary.review_decision, ReviewDecision::Approved);
        assert_eq!(summary.check_state, CheckState::Passed);
        assert_eq!(
            summary.native_state_evidence.lifecycle,
            ["open", "merged=false"]
        );
        assert!(
            summary
                .native_state_evidence
                .review
                .contains(&"APPROVED".to_string())
        );
        assert_eq!(summary.native_state_evidence.check, ["success"]);
    }

    #[tokio::test]
    async fn details_reject_a_source_transition_after_all_fact_groups_are_loaded() {
        let initial = r#"{"id":99,"number":7,"title":"Change","state":"open","merged":false,"mergeable":true,"head":{"ref":"topic","sha":"abc123","repo":{"full_name":"acme/widget"}},"base":{"ref":"main","sha":"def456","repo":{"full_name":"acme/widget","has_actions":false}}}"#;
        let changed = initial.replace(
            r#""full_name":"acme/widget"}},"base"#,
            r#""full_name":"contributor/widget"}},"base"#,
        );
        let server = TestServer::start(vec![
            response("200 OK", &[], initial),
            response("200 OK", &[], r#"{"version":"11.0.0"}"#),
            response("200 OK", &[], r#"{"max_response_items":50}"#),
            response("200 OK", &[], "[]"),
            response("200 OK", &[], "[]"),
            response("200 OK", &[], "[]"),
            response("200 OK", &[], "[]"),
            response("200 OK", &[], &changed),
        ]);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();
        let id = ChangeRequestId::new(
            repository.clone(),
            NativeChangeRequestId::new("99").unwrap(),
            Some(7),
        );
        let expected = ChangeRequest {
            id,
            source_repository: repository.clone(),
            target_repository: repository,
            source_branch: "topic".to_string(),
            target_branch: "main".to_string(),
            head_sha: "abc123".to_string(),
        };

        let error = ForgejoAdapter::new(profile)
            .unwrap()
            .change_request_details(&expected)
            .unwrap_err();

        assert_eq!(error.class(), RemoteErrorClass::StaleHead);
        let requests = server.requests();
        assert_eq!(requests.len(), 8);
        assert!(
            requests
                .last()
                .unwrap()
                .starts_with("GET /api/v1/repos/acme/widget/pulls/7 HTTP/1.1")
        );
    }

    #[tokio::test]
    async fn unavailable_action_logs_do_not_erase_failed_job_evidence() {
        let server = TestServer::start(vec![
            response("200 OK", &[], r#"{"version":"11.0.0"}"#),
            response("200 OK", &[], r#"{"max_response_items":50}"#),
            response(
                "200 OK",
                &[],
                r#"{"total_count":1,"workflow_runs":[{"id":81,"commit_sha":"abc123","status":"failure","workflow_id":"test.yaml","title":"Tests"}]}"#,
            ),
            response(
                "200 OK",
                &[],
                r#"[{"id":91,"name":"unit","status":"failure"}]"#,
            ),
            response("404 Not Found", &[], "logs disabled"),
        ]);
        let adapter = ForgejoAdapter::new(server.profile(None)).unwrap();
        let Observation::Known(failures) = adapter.observe_actions("acme/widget", "abc123") else {
            panic!("failed job evidence should survive unavailable logs");
        };
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].job, "unit");
        assert!(failures[0].log_tail.is_empty());
    }

    #[tokio::test]
    async fn disabled_actions_do_not_hide_external_statuses_or_trigger_log_requests() {
        let server = TestServer::start(vec![
            response(
                "200 OK",
                &[],
                include_str!("../../tests/fixtures/remote/forgejo/pull-actions-disabled.json"),
            ),
            response("200 OK", &[], r#"{"version":"11.0.0"}"#),
            response("200 OK", &[], r#"{"max_response_items":50}"#),
            response("200 OK", &[], "[]"),
            response("200 OK", &[], "[]"),
            response("200 OK", &[], "[]"),
            response(
                "200 OK",
                &[],
                r#"[{"context":"woodpecker/pr","status":"success"}]"#,
            ),
            response(
                "200 OK",
                &[],
                include_str!("../../tests/fixtures/remote/forgejo/pull-actions-disabled.json"),
            ),
        ]);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();
        let id = ChangeRequestId::new(
            repository.clone(),
            NativeChangeRequestId::new("99").unwrap(),
            Some(7),
        );
        let expected = ChangeRequest {
            id,
            source_repository: repository.clone(),
            target_repository: repository,
            source_branch: "topic".to_string(),
            target_branch: "main".to_string(),
            head_sha: "abc123".to_string(),
        };
        let details = ForgejoAdapter::new(profile)
            .unwrap()
            .change_request_details(&expected)
            .unwrap();
        let Observation::Known(checks) = details.checks else {
            panic!("external status should remain authoritative");
        };
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "woodpecker/pr");
        assert_eq!(details.ci_failures, Observation::Unsupported);
        assert!(
            server
                .requests()
                .iter()
                .all(|request| !request.contains("/actions/"))
        );
    }

    #[tokio::test]
    async fn disabled_actions_and_fresh_empty_statuses_are_authoritative_no_ci() {
        let server = TestServer::start(vec![
            response(
                "200 OK",
                &[],
                include_str!("../../tests/fixtures/remote/forgejo/pull-actions-disabled.json"),
            ),
            response("200 OK", &[], r#"{"version":"11.0.0"}"#),
            response("200 OK", &[], r#"{"max_response_items":50}"#),
            response("200 OK", &[], "[]"),
            response("200 OK", &[], "[]"),
        ]);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();
        let id = ChangeRequestId::new(
            repository,
            NativeChangeRequestId::new("99").unwrap(),
            Some(7),
        );

        let summary = ForgejoAdapter::new(profile)
            .unwrap()
            .change_request_summary(&id)
            .unwrap();

        assert_eq!(summary.check_state, CheckState::Skipped);
        assert!(summary.native_state_evidence.check.is_empty());
        assert!(
            server
                .requests()
                .iter()
                .all(|request| !request.contains("/actions/"))
        );
    }

    #[tokio::test]
    async fn missing_branch_protection_safety_fields_remain_unknown() {
        let server = TestServer::start(vec![response(
            "200 OK",
            &[],
            include_str!(
                "../../tests/fixtures/remote/forgejo/branch-protection-missing-safety.json"
            ),
        )]);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();
        let policy = ForgejoAdapter::new(profile)
            .unwrap()
            .repository_policy(&repository, "main")
            .unwrap();
        assert_eq!(policy.facts.required_checks, Observation::NotLoaded);
        assert_eq!(policy.facts.required_approvals, Observation::NotLoaded);
        assert_eq!(
            policy.facts.source_must_be_up_to_date,
            Observation::NotLoaded
        );
        assert_eq!(
            policy.facts.conversations_must_be_resolved,
            Observation::Known(false)
        );

        let protection: BranchProtectionResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/remote/forgejo/branch-protection.json"
        ))
        .unwrap();
        assert_eq!(protection.enable_status_check, Some(true));
        assert_eq!(protection.required_approvals, Some(2));
        assert_eq!(protection.block_on_outdated_branch, Some(true));
    }

    #[tokio::test]
    async fn supported_discovery_permits_create_flow() {
        let pull = r#"{"id":99,"number":7,"title":"Change","state":"open","merged":false,"mergeable":true,"head":{"ref":"topic","sha":"abc123","repo":{"full_name":"acme/widget"}},"base":{"ref":"main","sha":"def456","repo":{"full_name":"acme/widget"}}}"#;
        let server = TestServer::start(vec![
            response(
                "200 OK",
                &[],
                include_str!("../../tests/fixtures/remote/forgejo/version-current.json"),
            ),
            response("200 OK", &[], "{}"),
            response("200 OK", &[], r#"{"commit":{"id":"abc123"}}"#),
            response("200 OK", &[], pull),
            response("200 OK", &[], pull),
        ]);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();
        let adapter = ForgejoAdapter::new(profile).unwrap();
        let summary = adapter
            .create_change_request(CreateChangeRequest {
                source_repository: repository.clone(),
                target_repository: repository,
                source_branch: "topic".to_string(),
                target_branch: "main".to_string(),
                expected_head_sha: "abc123".to_string(),
                title: "Change".to_string(),
                body: String::new(),
                draft: false,
            })
            .unwrap();
        assert_eq!(summary.change_request.head_sha, "abc123");
        let requests = server.requests();
        assert_eq!(requests.len(), 5);
        assert!(requests[0].starts_with("GET /api/v1/version HTTP/1.1"));
        assert!(requests[1].starts_with("GET /api/v1/settings/api HTTP/1.1"));
        assert!(requests[3].starts_with("POST /api/v1/repos/acme/widget/pulls HTTP/1.1"));
    }

    #[tokio::test]
    async fn guarded_merge_rejects_old_version_without_posting() {
        let server = TestServer::start(vec![
            response("200 OK", &[], r#"{"version":"8.0.0"}"#),
            response("200 OK", &[], "{}"),
        ]);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();
        let id = ChangeRequestId::new(
            repository.clone(),
            NativeChangeRequestId::new("99").unwrap(),
            Some(7),
        );
        let adapter = ForgejoAdapter::new(profile).unwrap();
        let error = adapter
            .merge_change_request(GuardedMerge {
                id,
                target_repository: repository,
                target_branch: "main".to_string(),
                expected_source_sha: "abc123".to_string(),
                method: MergeMethod::Squash,
                native_guard: None,
            })
            .unwrap_err();
        assert_eq!(error.class(), RemoteErrorClass::Unsupported);
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| !request.starts_with("POST ")));
    }

    #[tokio::test]
    async fn supported_discovery_permits_guarded_merge_flow() {
        let pull = r#"{"id":99,"number":7,"title":"Change","state":"open","merged":false,"mergeable":true,"head":{"ref":"topic","sha":"abc123","repo":{"full_name":"acme/widget"}},"base":{"ref":"main","sha":"def456","repo":{"full_name":"acme/widget"}}}"#;
        let merged = r#"{"id":99,"number":7,"title":"Change","state":"closed","merged":true,"mergeable":true,"head":{"ref":"topic","sha":"abc123","repo":{"full_name":"acme/widget"}},"base":{"ref":"main","sha":"def456","repo":{"full_name":"acme/widget"}}}"#;
        let server = TestServer::start(vec![
            response(
                "200 OK",
                &[],
                include_str!("../../tests/fixtures/remote/forgejo/version-codeberg.json"),
            ),
            response("200 OK", &[], "{}"),
            response("200 OK", &[], pull),
            response("200 OK", &[], ""),
            response("200 OK", &[], merged),
        ]);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();
        let id = ChangeRequestId::new(
            repository.clone(),
            NativeChangeRequestId::new("99").unwrap(),
            Some(7),
        );
        let adapter = ForgejoAdapter::new(profile).unwrap();
        let summary = adapter
            .merge_change_request(GuardedMerge {
                id,
                target_repository: repository,
                target_branch: "main".to_string(),
                expected_source_sha: "abc123".to_string(),
                method: MergeMethod::Squash,
                native_guard: None,
            })
            .unwrap();
        assert_eq!(summary.outcome, super::super::MergeMutationOutcome::Merged);
        assert_eq!(summary.summary.lifecycle, LifecycleState::Merged);
        let requests = server.requests();
        assert_eq!(requests.len(), 5);
        assert!(requests[0].starts_with("GET /api/v1/version HTTP/1.1"));
        assert!(requests[1].starts_with("GET /api/v1/settings/api HTTP/1.1"));
        assert!(requests[3].contains(r#"{"Do":"squash","head_commit_id":"abc123"}"#));
        assert!(requests[4].starts_with("GET /api/v1/repos/acme/widget/pulls/7 HTTP/1.1"));
    }

    #[tokio::test]
    async fn closed_unmerged_post_response_without_queue_evidence_is_uncertain() {
        let pull = r#"{"id":99,"number":7,"title":"Change","state":"open","merged":false,"mergeable":true,"head":{"ref":"topic","sha":"abc123","repo":{"full_name":"acme/widget"}},"base":{"ref":"main","sha":"def456","repo":{"full_name":"acme/widget"}}}"#;
        let closed = pull.replace(r#""state":"open""#, r#""state":"closed""#);
        let server = TestServer::start(vec![
            response("200 OK", &[], r#"{"version":"11.0.0"}"#),
            response("200 OK", &[], "{}"),
            response("200 OK", &[], pull),
            response("200 OK", &[], ""),
            response("200 OK", &[], &closed),
        ]);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();
        let id = ChangeRequestId::new(
            repository.clone(),
            NativeChangeRequestId::new("99").unwrap(),
            Some(7),
        );

        let result = ForgejoAdapter::new(profile)
            .unwrap()
            .merge_change_request(GuardedMerge {
                id,
                target_repository: repository,
                target_branch: "main".to_string(),
                expected_source_sha: "abc123".to_string(),
                method: MergeMethod::Squash,
                native_guard: None,
            })
            .unwrap();

        assert_eq!(
            result.outcome,
            super::super::MergeMutationOutcome::Uncertain
        );
        assert_eq!(result.summary.lifecycle, LifecycleState::Closed);
    }

    #[tokio::test]
    async fn merge_conflict_is_stale_only_when_reobservation_finds_a_changed_head() {
        let pull = r#"{"id":99,"number":7,"title":"Change","state":"open","merged":false,"mergeable":true,"head":{"ref":"topic","sha":"abc123","repo":{"full_name":"acme/widget"}},"base":{"ref":"main","sha":"def456","repo":{"full_name":"acme/widget"}}}"#;
        let changed = pull.replace("abc123", "new-head");
        for (current, expected_class) in [
            (pull.to_string(), RemoteErrorClass::Conflict),
            (changed, RemoteErrorClass::StaleHead),
        ] {
            let server = TestServer::start(vec![
                response("200 OK", &[], r#"{"version":"11.0.0"}"#),
                response("200 OK", &[], "{}"),
                response("200 OK", &[], pull),
                response("409 Conflict", &[], "merge conflict"),
                response("200 OK", &[], &current),
            ]);
            let profile = server.profile(None);
            let repository =
                RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                    .unwrap();
            let id = ChangeRequestId::new(
                repository.clone(),
                NativeChangeRequestId::new("99").unwrap(),
                Some(7),
            );
            let error = ForgejoAdapter::new(profile)
                .unwrap()
                .merge_change_request(GuardedMerge {
                    id,
                    target_repository: repository,
                    target_branch: "main".to_string(),
                    expected_source_sha: "abc123".to_string(),
                    method: MergeMethod::Squash,
                    native_guard: None,
                })
                .unwrap_err();
            assert_eq!(error.class(), expected_class);
        }
    }

    #[tokio::test]
    async fn unsupported_merge_method_response_is_not_misclassified_as_stale() {
        let pull = r#"{"id":99,"number":7,"title":"Change","state":"open","merged":false,"mergeable":true,"head":{"ref":"topic","sha":"abc123","repo":{"full_name":"acme/widget"}},"base":{"ref":"main","sha":"def456","repo":{"full_name":"acme/widget"}}}"#;
        let server = TestServer::start(vec![
            response("200 OK", &[], r#"{"version":"11.0.0"}"#),
            response("200 OK", &[], "{}"),
            response("200 OK", &[], pull),
            response("405 Method Not Allowed", &[], ""),
        ]);
        let profile = server.profile(None);
        let repository =
            RemoteRepositoryId::new(ProviderKind::Forgejo, profile.host.clone(), "acme/widget")
                .unwrap();
        let id = ChangeRequestId::new(
            repository.clone(),
            NativeChangeRequestId::new("99").unwrap(),
            Some(7),
        );
        let error = ForgejoAdapter::new(profile)
            .unwrap()
            .merge_change_request(GuardedMerge {
                id,
                target_repository: repository,
                target_branch: "main".to_string(),
                expected_source_sha: "abc123".to_string(),
                method: MergeMethod::Squash,
                native_guard: None,
            })
            .unwrap_err();
        assert_eq!(error.class(), RemoteErrorClass::Unsupported);
    }

    #[tokio::test]
    async fn cancellation_waiter_lifetime_is_shared_and_ends_with_last_adapter_clone() {
        let server = TestServer::start(Vec::new());
        let token = crate::process::CancellationToken::new();
        let waiter = crate::process::with_cancellation(token, async {
            let adapter = ForgejoAdapter::new(server.profile(None)).unwrap();
            let waiter = Arc::downgrade(&adapter._cancellation_waiter);
            let clone = adapter.clone();
            drop(adapter);
            assert!(waiter.upgrade().is_some());
            drop(clone);
            waiter
        })
        .await;

        tokio::task::yield_now().await;
        assert!(waiter.upgrade().is_none());
    }

    #[tokio::test]
    async fn cancellation_is_best_effort_between_requests() {
        let server = TestServer::start(Vec::new());
        let adapter = ForgejoAdapter::new(server.profile(None)).unwrap();
        adapter.cancel();
        let error = adapter.discover_instance().unwrap_err();
        assert_eq!(error.class(), RemoteErrorClass::Cancelled);
    }

    #[tokio::test]
    async fn cancellation_is_observed_while_reading_a_response_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (sent, received) = mpsc::channel();
        let server_thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{")
                .unwrap();
            sent.send(()).unwrap();
            thread::sleep(Duration::from_millis(75));
            stream.write_all(b"\"version\":\"11.0.0\"}").unwrap();
        });
        let host = HostIdentity::parse(&address).unwrap();
        let base = RemoteBase::new(WebScheme::Http, host.clone(), "api/v1").unwrap();
        let profile = HostProfile::new(host, ProviderKind::Forgejo)
            .unwrap()
            .with_http_allowed(true)
            .with_bases(base.clone(), base);
        let adapter = Arc::new(ForgejoAdapter::new(profile).unwrap());
        let worker_adapter = Arc::clone(&adapter);
        let worker = thread::spawn(move || worker_adapter.discover_instance());
        received.recv_timeout(Duration::from_secs(1)).unwrap();
        adapter.cancel();
        let error = worker.join().unwrap().unwrap_err();
        server_thread.join().unwrap();
        assert_eq!(error.class(), RemoteErrorClass::Cancelled);
    }

    #[tokio::test]
    async fn stalled_transport_is_bounded_by_the_request_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server_thread = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(150));
        });
        let host = HostIdentity::parse(&address).unwrap();
        let base = RemoteBase::new(WebScheme::Http, host.clone(), "api/v1").unwrap();
        let profile = HostProfile::new(host, ProviderKind::Forgejo)
            .unwrap()
            .with_http_allowed(true)
            .with_bases(base.clone(), base);
        let adapter = ForgejoAdapter::with_transport_options(
            profile,
            Duration::from_millis(25),
            DEFAULT_RESPONSE_LIMIT,
        )
        .unwrap();
        let started = Instant::now();
        let error = adapter.discover_instance().unwrap_err();
        server_thread.join().unwrap();
        assert_eq!(error.class(), RemoteErrorClass::Timeout);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn unsupported_operations_are_explicit() {
        let server = TestServer::start(Vec::new());
        let adapter = ForgejoAdapter::new(server.profile(None)).unwrap();
        let static_capabilities = Capabilities::for_provider(ProviderKind::Forgejo);
        assert_eq!(
            adapter.observe_merge_queue().unwrap_err().class(),
            RemoteErrorClass::Unsupported
        );
        assert_eq!(
            adapter.capabilities().resolve_review_thread,
            SupportLevel::Unsupported
        );
        assert_eq!(
            adapter.capabilities().merge_queue,
            SupportLevel::Unsupported
        );
        assert_eq!(
            adapter.capabilities().create_change_request,
            SupportLevel::Conditional
        );
        assert_eq!(
            adapter.capabilities().guarded_merge,
            SupportLevel::Conditional
        );
        assert_eq!(
            static_capabilities.create_change_request,
            SupportLevel::Conditional
        );
        assert_eq!(static_capabilities.guarded_merge, SupportLevel::Conditional);
        assert_eq!(adapter.capabilities().ci_logs, SupportLevel::Conditional);
        assert_eq!(static_capabilities.ci_logs, SupportLevel::Conditional);
        assert_eq!(
            static_capabilities.resolve_review_thread,
            SupportLevel::Unsupported
        );
        assert_eq!(static_capabilities.merge_queue, SupportLevel::Unsupported);
    }

    #[tokio::test]
    #[ignore = "requires scripts/remote-compatibility.sh forgejo"]
    async fn pinned_local_forgejo_adapter_compatibility() {
        assert!(
            matches!(std::env::var("PRISM_REMOTE_COMPATIBILITY"), Ok(value) if value == "1"),
            "unsupported fixture: run this test only through scripts/remote-compatibility.sh forgejo"
        );
        let base_url = std::env::var("PRISM_FORGEJO_COMPAT_URL")
            .expect("unsupported fixture: PRISM_FORGEJO_COMPAT_URL is required");
        std::env::var("PRISM_FORGEJO_COMPAT_TOKEN")
            .expect("unsupported fixture: PRISM_FORGEJO_COMPAT_TOKEN is required");
        let parsed = url::Url::parse(&base_url).expect("compatibility URL");
        let host = HostIdentity::new(
            parsed.host_str().expect("compatibility host"),
            parsed.port(),
        )
        .unwrap();
        let profile = HostProfile::new(host.clone(), ProviderKind::Forgejo)
            .unwrap()
            .with_http_allowed(true)
            .with_bases(
                RemoteBase::new(WebScheme::Http, host.clone(), "").unwrap(),
                RemoteBase::new(WebScheme::Http, host.clone(), "api/v1").unwrap(),
            )
            .with_credential_environment("PRISM_FORGEJO_COMPAT_TOKEN")
            .unwrap();
        let discovery = RemoteDiscovery::new([profile.clone()]).unwrap();
        let discovered = discovery
            .discover(&format!("{base_url}/prism/compat-target.git"))
            .unwrap();
        let target = discovered.repository.id;
        assert_eq!(target.provider(), ProviderKind::Forgejo);
        assert_eq!(target.project_path(), "prism/compat-target");

        let adapter = ForgejoAdapter::new(profile).unwrap();
        let instance = adapter.discover_instance().unwrap();
        assert!(instance.version.starts_with("11."));
        assert_eq!(
            adapter.capabilities().guarded_merge,
            SupportLevel::Supported
        );

        let summaries = adapter.list_change_requests(&target).unwrap();
        let same = summaries
            .iter()
            .find(|summary| summary.title == "compat same-project seeded")
            .expect("same-project fixture must be listed");
        assert_eq!(same.change_request.source_repository, target);
        assert_eq!(same.change_request.target_repository, target);
        let fork = summaries
            .iter()
            .find(|summary| summary.title == "compat fork seeded")
            .expect("fork fixture must be listed");
        assert_eq!(
            fork.change_request.source_repository.project_path(),
            "contributor/compat-target"
        );
        assert_eq!(fork.change_request.target_repository, target);

        let fetched = adapter
            .fetch_change_request(&fork.change_request.id)
            .unwrap();
        assert_eq!(
            fetched.source_repository,
            fork.change_request.source_repository
        );
        assert_eq!(fetched.expected_head_sha, fork.change_request.head_sha);

        let details = adapter
            .change_request_details(&same.change_request)
            .unwrap();
        assert!(details.association.as_ref().is_some_and(|association| {
            association.matches(&same.change_request.id, &same.change_request.head_sha)
        }));
        let comments = details
            .comments
            .known()
            .expect("Forgejo comments endpoint must be supported");
        assert!(
            comments
                .iter()
                .any(|comment| comment.body == "compat seeded comment")
        );
        let reviews = details
            .reviews
            .known()
            .expect("unsupported fixture: Forgejo reviews API returned no usable observation");
        assert!(
            reviews
                .iter()
                .any(|review| review.body == "compat seeded review")
        );
        let changed_files = details
            .changed_files
            .known()
            .expect("Forgejo changed-files endpoint must be supported");
        assert!(changed_files.iter().any(|path| path == "compat.txt"));
        let checks = details
            .checks
            .known()
            .expect("Forgejo status endpoint must return evidence");
        assert!(
            checks.iter().any(|check| {
                check.name == "compat/status" && check.state == CheckState::Passed
            })
        );
        assert_eq!(details.ci_failures, Observation::Unsupported);

        let policy = adapter.repository_policy(&target, "main").unwrap();
        assert_eq!(policy.repository, Some(target.clone()));
        assert_eq!(policy.facts.required_checks, Observation::Known(Vec::new()));
        assert_eq!(policy.facts.required_approvals, Observation::Known(0));
        assert_eq!(
            policy.facts.source_must_be_up_to_date,
            Observation::Known(false)
        );

        let source =
            RemoteRepositoryId::new(ProviderKind::Forgejo, host, "contributor/compat-target")
                .unwrap();
        let create_request = |expected_head_sha: &str| CreateChangeRequest {
            source_repository: source.clone(),
            target_repository: target.clone(),
            source_branch: "adapter-create".to_string(),
            target_branch: "main".to_string(),
            expected_head_sha: expected_head_sha.to_string(),
            title: "compat adapter-created".to_string(),
            body: "created by the real Forgejo adapter".to_string(),
            draft: false,
        };
        let stale = adapter
            .create_change_request(create_request("0000000000000000000000000000000000000000"))
            .unwrap_err();
        assert_eq!(stale.class(), RemoteErrorClass::StaleHead);
        let branch: BranchResponse = adapter
            .client
            .get_json(
                RemoteOperation::CreateChangeRequest,
                "repos/contributor/compat-target/branches/adapter-create",
                &[],
            )
            .unwrap();
        let created = adapter
            .create_change_request(create_request(&branch.commit.id))
            .unwrap();
        assert_eq!(
            created.change_request.source_repository.project_path(),
            "contributor/compat-target"
        );
        assert_eq!(created.change_request.target_repository, target);
        assert_eq!(created.change_request.head_sha, branch.commit.id);

        let merge_request = |expected_source_sha: &str| GuardedMerge {
            id: created.change_request.id.clone(),
            target_repository: target.clone(),
            target_branch: "main".to_string(),
            expected_source_sha: expected_source_sha.to_string(),
            method: MergeMethod::Merge,
            native_guard: None,
        };
        let stale = adapter
            .merge_change_request(merge_request("0000000000000000000000000000000000000000"))
            .unwrap_err();
        assert_eq!(stale.class(), RemoteErrorClass::StaleHead);
        let merged = adapter
            .merge_change_request(merge_request(&created.change_request.head_sha))
            .unwrap();
        assert_eq!(merged.outcome, super::super::MergeMutationOutcome::Merged);
        assert_eq!(merged.summary.lifecycle, LifecycleState::Merged);

        let unsupported = adapter
            .resolve_review_thread(ResolveReviewThread {
                id: same.change_request.id.clone(),
                thread_id: NativeReviewThreadId::new("unsupported-by-forgejo").unwrap(),
                expected_head_sha: same.change_request.head_sha.clone(),
            })
            .unwrap_err();
        assert_eq!(unsupported.class(), RemoteErrorClass::Unsupported);
    }
}
