use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::http::HttpClient;
use super::{
    Capabilities, ChangeRequest, ChangeRequestDetails, ChangeRequestId, ChangeRequestSummary,
    CheckContext, CheckState, CiFailure, Comment, CreateChangeRequest, FetchChangeRequest,
    GuardedMerge, HostProfile, LifecycleState, MergeMethod, MergeabilityState,
    NativeChangeRequestId, NativeReviewThreadId, Observation, PolicyFacts, ProviderKind,
    QueueState, RemoteError, RemoteErrorClass, RemoteOperation, RemoteRepositoryId,
    RepositoryPolicy, ResolveReviewThread, RetryHint, Retryability, Review, ReviewDecision,
    ReviewThread, SupportLevel,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_RESPONSE_LIMIT: usize = 1024 * 1024;
const PAGE_SIZE: u32 = 50;
const MINIMUM_MUTATION_MAJOR: u64 = 9;
const MAXIMUM_MUTATION_MAJOR: u64 = 16;
const MAX_REVIEW_COMMENT_REQUESTS: usize = 100;
const MAX_FAILED_JOBS: usize = 32;
const MAX_ACTION_PAGES: u32 = 100;
const LOG_TAIL_BYTES: usize = 16 * 1024;

pub(crate) struct ForgejoAdapter {
    profile: HostProfile,
    client: HttpClient,
    cancelled: Arc<AtomicBool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ForgejoInstance {
    pub(crate) version: String,
    pub(crate) settings: ForgejoApiSettings,
    pub(crate) observed_at: SystemTime,
}

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
        let cancelled = crate::process::current_cancellation()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let client = HttpClient::new(&profile, timeout, response_limit, Arc::clone(&cancelled))?;
        Ok(Self {
            profile,
            client,
            cancelled,
        })
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub(crate) fn capabilities(&self) -> Capabilities {
        Capabilities {
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
            merge_queue: SupportLevel::Unsupported,
        }
    }

    pub(crate) fn discover_instance(&self) -> Result<ForgejoInstance, RemoteError> {
        self.discover_instance_for(RemoteOperation::DiscoverRepository)
    }

    fn discover_instance_for(
        &self,
        operation: RemoteOperation,
    ) -> Result<ForgejoInstance, RemoteError> {
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
        Ok(ForgejoInstance {
            version: version.version,
            settings,
            observed_at: SystemTime::now(),
        })
    }

    pub(crate) fn list_change_requests(
        &self,
        repository: &RemoteRepositoryId,
    ) -> Result<Vec<ChangeRequestSummary>, RemoteError> {
        let project = self.project(repository, RemoteOperation::ListChangeRequests)?;
        let pulls = self.client.get_json_pages::<PullRequestResponse>(
            RemoteOperation::ListChangeRequests,
            &format!("repos/{project}/pulls"),
            &[
                ("state", "all".to_string()),
                ("limit", PAGE_SIZE.to_string()),
                ("page", "1".to_string()),
            ],
        )?;
        pulls
            .into_iter()
            .map(|pull| self.summary(repository, pull, RemoteOperation::ListChangeRequests))
            .collect()
    }

    pub(crate) fn change_request_summary(
        &self,
        id: &ChangeRequestId,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        let (project, number) =
            self.change_request_route(id, RemoteOperation::ObserveChangeRequest)?;
        let pull = self.client.get_json::<PullRequestResponse>(
            RemoteOperation::ObserveChangeRequest,
            &format!("repos/{project}/pulls/{number}"),
            &[],
        )?;
        self.summary(id.repository(), pull, RemoteOperation::ObserveChangeRequest)
    }

    pub(crate) fn change_request_details(
        &self,
        id: &ChangeRequestId,
    ) -> Result<ChangeRequestDetails, RemoteError> {
        let summary = self.change_request_summary(id)?;
        let (project, number) =
            self.change_request_route(id, RemoteOperation::ObserveChangeRequest)?;
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
        let checks = self.observe_checks(&project, &head_sha);
        let ci_failures = self.observe_actions(&project, &head_sha);
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
                required_checks: Observation::Known(if protection.enable_status_check {
                    protection.status_check_contexts
                } else {
                    Vec::new()
                }),
                required_approvals: Observation::Known(protection.required_approvals),
                conversations_must_be_resolved: Observation::Known(false),
                source_must_be_up_to_date: Observation::Known(protection.block_on_outdated_branch),
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
        let summary = self.change_request_summary(id)?;
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
        self.change_request_summary(&created_summary.change_request.id)
    }

    pub(crate) fn merge_change_request(
        &self,
        request: GuardedMerge,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        if request.id.repository() != &request.target_repository {
            return Err(validation_error(
                RemoteOperation::MergeChangeRequest,
                "Forgejo merge target does not match the change request repository",
            ));
        }
        self.require_supported_mutations(RemoteOperation::MergeChangeRequest)?;
        let observed = self.change_request_summary(&request.id)?;
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
            if matches!(
                remote_error.class(),
                RemoteErrorClass::Conflict | RemoteErrorClass::Validation
            ) {
                return Err(stale_head_error(
                    RemoteOperation::MergeChangeRequest,
                    remote_error.status(),
                ));
            }
            return Err(remote_error);
        }
        self.change_request_summary(&request.id)
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
            review_decision: ReviewDecision::Unknown("not_observed".to_string()),
            mergeability,
            check_state: CheckState::Unknown("not_observed".to_string()),
            queue_state: QueueState::Unknown("unsupported".to_string()),
            draft: pull.draft,
            updated_at: nonempty(pull.updated_at),
        })
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
        observation(
            self.client
                .get_json_pages::<CommentResponse>(
                    RemoteOperation::ObserveChangeRequest,
                    &format!("repos/{project}/issues/{number}/comments"),
                    &[("limit", PAGE_SIZE.to_string()), ("page", "1".to_string())],
                )
                .map(|comments| comments.into_iter().map(map_comment).collect()),
            false,
        )
    }

    fn load_reviews(&self, project: &str, number: u64) -> Result<Vec<ReviewResponse>, RemoteError> {
        self.client.get_json_pages::<ReviewResponse>(
            RemoteOperation::ObserveChangeRequest,
            &format!("repos/{project}/pulls/{number}/reviews"),
            &[("limit", PAGE_SIZE.to_string()), ("page", "1".to_string())],
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
        for review in reviews {
            let comments = self.client.get_json_pages::<ReviewCommentResponse>(
                RemoteOperation::ObserveReviewThreads,
                &format!(
                    "repos/{project}/pulls/{number}/reviews/{}/comments",
                    review.id
                ),
                &[("limit", PAGE_SIZE.to_string()), ("page", "1".to_string())],
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
        observation(
            self.client
                .get_json_pages::<ChangedFileResponse>(
                    RemoteOperation::ObserveChangedFiles,
                    &format!("repos/{project}/pulls/{number}/files"),
                    &[("limit", PAGE_SIZE.to_string()), ("page", "1".to_string())],
                )
                .map(|files| files.into_iter().map(|file| file.filename).collect()),
            true,
        )
    }

    fn observe_checks(&self, project: &str, head_sha: &str) -> Observation<Vec<CheckContext>> {
        observation(
            self.client
                .get_json_pages::<CommitStatusResponse>(
                    RemoteOperation::ObserveChecks,
                    &format!(
                        "repos/{project}/commits/{}/statuses",
                        path_segment(head_sha)
                    ),
                    &[("limit", PAGE_SIZE.to_string()), ("page", "1".to_string())],
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
                }),
            true,
        )
    }

    fn observe_actions(&self, project: &str, head_sha: &str) -> Observation<Vec<CiFailure>> {
        observation(self.load_action_failures(project, head_sha), true)
    }

    fn load_action_failures(
        &self,
        project: &str,
        head_sha: &str,
    ) -> Result<Vec<CiFailure>, RemoteError> {
        let mut failed_runs = Vec::new();
        let mut observed_runs = 0_u64;
        for page in 1..=MAX_ACTION_PAGES {
            let response = self.client.get_json::<ActionRunsResponse>(
                RemoteOperation::LoadCiLogs,
                &format!("repos/{project}/actions/runs"),
                &[
                    ("head_sha", head_sha.to_string()),
                    ("limit", PAGE_SIZE.to_string()),
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
            failed_runs.extend(
                response
                    .workflow_runs
                    .into_iter()
                    .filter(|run| run.commit_sha == head_sha && failed_state(&run.status)),
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
        let mut failures = Vec::new();
        for run in failed_runs {
            let jobs = self.client.get_json_pages::<ActionJobResponse>(
                RemoteOperation::LoadCiLogs,
                &format!("repos/{project}/actions/runs/{}/jobs", run.id),
                &[("limit", PAGE_SIZE.to_string()), ("page", "1".to_string())],
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
                )?;
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
                    log_tail: utf8_tail(&log, LOG_TAIL_BYTES),
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
        decision: if review.dismissed {
            ReviewDecision::Dismissed
        } else {
            ReviewDecision::from_native(review.state.clone())
        },
        body: review.body.clone(),
        submitted_at: nonempty(review.submitted_at.clone()),
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
    #[serde(default)]
    dismissed: bool,
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
    #[serde(default)]
    enable_status_check: bool,
    #[serde(default)]
    status_check_contexts: Vec<String>,
    #[serde(default)]
    required_approvals: u32,
    #[serde(default)]
    block_on_outdated_branch: bool,
}

#[derive(Deserialize)]
struct ActionRunsResponse {
    total_count: Option<u64>,
    #[serde(default)]
    workflow_runs: Vec<ActionRunResponse>,
}

#[derive(Deserialize)]
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
    use std::sync::{Arc, Mutex};
    use std::thread;

    use super::*;
    use crate::remote::{HostIdentity, RemoteBase, WebScheme};

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

    #[test]
    fn discovers_codeberg_shaped_version_and_settings() {
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

    #[test]
    fn supported_forgejo_major_version_fixtures_remain_forward_compatible() {
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

    #[test]
    fn maps_fork_source_target_and_exact_head_from_fixture() {
        let fixture = include_str!("../../tests/fixtures/remote/forgejo/pull-fork.json");
        let server = TestServer::start(vec![response("200 OK", &[], &format!("[{fixture}]"))]);
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

    #[test]
    fn sends_configured_token_only_as_a_redacted_header() {
        const ENVIRONMENT: &str = "PRISM_FORGEJO_TEST_TOKEN_HEADER";
        const SECRET: &str = "super-secret-token";
        // Each test uses a unique variable; changing process environment is otherwise isolated.
        unsafe { std::env::set_var(ENVIRONMENT, SECRET) };
        let server = TestServer::start(vec![response("401 Unauthorized", &[], "denied")]);
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

    #[test]
    fn rejects_oversized_and_invalid_json_responses() {
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

    #[test]
    fn follows_same_origin_pagination_and_rejects_cross_origin_links() {
        let server = TestServer::start(vec![
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
        assert_eq!(server.requests().len(), 3);
    }

    #[test]
    fn status_page_two_failure_is_an_observation_failure() {
        let server = TestServer::start(vec![
            response(
                "200 OK",
                &[(
                    "Link",
                    "</api/v1/repos/acme/widget/commits/abc123/statuses?limit=50&page=2>; rel=\"next\"",
                )],
                r#"[{"context":"build","status":"success","target_url":""}]"#,
            ),
            response("503 Service Unavailable", &[], ""),
        ]);
        let adapter = ForgejoAdapter::new(server.profile(None)).unwrap();
        let Observation::Failed(error) = adapter.observe_checks("acme/widget", "abc123") else {
            panic!("page two failure must not produce authoritative page-one statuses");
        };
        assert_eq!(error.class(), RemoteErrorClass::Provider);
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with(
            "GET /api/v1/repos/acme/widget/commits/abc123/statuses?limit=50&page=1 HTTP/1.1"
        ));
        assert!(requests[1].starts_with(
            "GET /api/v1/repos/acme/widget/commits/abc123/statuses?limit=50&page=2 HTTP/1.1"
        ));
    }

    #[test]
    fn classifies_retry_after_and_statuses() {
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

    #[test]
    fn create_fails_closed_when_discovery_fails_without_posting() {
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

    #[test]
    fn unverified_versions_block_create_before_posting() {
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

    #[test]
    fn actions_pagination_uses_total_count_instead_of_requested_page_size() {
        let server = TestServer::start(vec![
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
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("page=1"));
        assert!(requests[1].contains("page=2"));
    }

    #[test]
    fn supported_discovery_permits_create_flow() {
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

    #[test]
    fn guarded_merge_rejects_old_version_without_posting() {
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

    #[test]
    fn supported_discovery_permits_guarded_merge_flow() {
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
        assert_eq!(summary.lifecycle, LifecycleState::Merged);
        let requests = server.requests();
        assert_eq!(requests.len(), 5);
        assert!(requests[0].starts_with("GET /api/v1/version HTTP/1.1"));
        assert!(requests[1].starts_with("GET /api/v1/settings/api HTTP/1.1"));
        assert!(requests[3].contains(r#"{"Do":"squash","head_commit_id":"abc123"}"#));
        assert!(requests[4].starts_with("GET /api/v1/repos/acme/widget/pulls/7 HTTP/1.1"));
    }

    #[test]
    fn cancellation_is_best_effort_between_requests() {
        let server = TestServer::start(Vec::new());
        let adapter = ForgejoAdapter::new(server.profile(None)).unwrap();
        adapter.cancel();
        let error = adapter.discover_instance().unwrap_err();
        assert_eq!(error.class(), RemoteErrorClass::Cancelled);
    }

    #[test]
    fn unsupported_operations_are_explicit() {
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
        assert_eq!(
            static_capabilities.resolve_review_thread,
            SupportLevel::Unsupported
        );
        assert_eq!(static_capabilities.merge_queue, SupportLevel::Unsupported);
    }
}
