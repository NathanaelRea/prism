use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::config::Config;
use crate::process::{
    ProcessDescriptor, ProcessOutput, ProcessPolicy, run_output_allow_failure_named,
};

use super::{
    Capabilities, ChangeRequest, ChangeRequestDetails, ChangeRequestId, ChangeRequestSummary,
    CheckContext, CheckState, CiFailure, Comment, CreateChangeRequest, FetchChangeRequest,
    GuardedMerge, LifecycleState, MergeMethod, MergeMutationResult, MergeabilityState,
    NativeChangeRequestId, NativeReviewThreadId, NativeStateEvidence, Observation, PolicyFacts,
    ProviderKind, QueueState, RemoteError, RemoteErrorClass, RemoteOperation, RemoteRepositoryId,
    RepositoryPolicy, ResolveReviewThread, RetryHint, Retryability, Review, ReviewDecision,
    ReviewThread, SupportLevel,
};

const PAGE_SIZE: usize = 50;
const MAX_PAGES: usize = 200;
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_TRACE_BYTES: usize = 32 * 1024;
const MAX_FAILED_TRACES: usize = 12;
const MAX_CHANGE_REQUEST_PIPELINES: usize = 20;

/// GitLab transport and normalization behind `glab`'s configured credentials.
pub(super) struct GitLabAdapter {
    glab_path: String,
    repository: RemoteRepositoryId,
    api_base_override: Option<String>,
    merge_method: crate::config::MergeMethod,
}

impl GitLabAdapter {
    pub(super) fn new(
        config: &Config,
        repository: RemoteRepositoryId,
    ) -> Result<Self, RemoteError> {
        if repository.provider() != ProviderKind::GitLab {
            return Err(remote_error(
                RemoteOperation::DiscoverRepository,
                RemoteErrorClass::Configuration,
                Retryability::NotRetryable,
                "GitLab adapter requires a GitLab repository",
            ));
        }
        Ok(Self {
            glab_path: config.tool("glab"),
            api_base_override: config.remote_api_override(repository.host()),
            merge_method: config.merge_method,
            repository,
        })
    }

    #[cfg(test)]
    fn with_glab_path(glab_path: &str, repository: RemoteRepositoryId) -> Self {
        Self {
            glab_path: glab_path.to_string(),
            repository,
            api_base_override: None,
            merge_method: crate::config::MergeMethod::Squash,
        }
    }

    pub(super) fn capabilities(&self) -> Capabilities {
        let mut capabilities = Capabilities {
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
            merge_queue: SupportLevel::Conditional,
        };
        if self.merge_method == crate::config::MergeMethod::Rebase {
            capabilities.guarded_merge = SupportLevel::Unsupported;
            capabilities.guarded_merge_reason =
                Some("GitLab adapter does not support rebase merges".to_string());
        }
        capabilities
    }

    pub(super) fn list_change_requests(&self) -> Result<Vec<ChangeRequestSummary>, RemoteError> {
        let endpoint = format!(
            "projects/{}/merge_requests?scope=all&state=all&order_by=updated_at&sort=desc&with_merge_status_recheck=true",
            encode_path_segment(self.repository.project_path())
        );
        let page =
            self.paginated::<GitLabMergeRequest>(RemoteOperation::ListChangeRequests, &endpoint)?;
        if let Some(error) = page.partial_error {
            return Err(error);
        }

        let mut projects = BTreeMap::new();
        let mut native_ids = BTreeSet::new();
        let mut summaries = Vec::with_capacity(page.items.len());
        for merge_request in page.items {
            if !native_ids.insert(merge_request.id) {
                return Err(invalid_response(
                    RemoteOperation::ListChangeRequests,
                    "GitLab pagination returned a duplicate global merge request ID",
                ));
            }
            let target_id = required_project_id(
                merge_request.target_project_id,
                "target_project_id",
                RemoteOperation::ListChangeRequests,
            )?;
            let target = self.repository_for_project_id(target_id, &mut projects)?;
            if target != self.repository {
                return Err(invalid_response(
                    RemoteOperation::ListChangeRequests,
                    "GitLab project merge-request list returned a different target project",
                ));
            }
            let Some(source_id) = merge_request.source_project_id.filter(|id| *id > 0) else {
                return Err(incomplete_change_request_list(
                    "GitLab merge-request list includes an item without source project identity",
                    None,
                ));
            };
            let source =
                list_source_repository(source_id, target_id, target.clone(), |source_id| {
                    self.repository_for_project_id(source_id, &mut projects)
                })?;
            let (review, review_evidence) = self.review_decision(
                merge_request.iid,
                &merge_request,
                RemoteOperation::ListChangeRequests,
            );
            summaries.push(summary_from_merge_request(
                &self.repository,
                source,
                target,
                merge_request,
                review,
                review_evidence,
                RemoteOperation::ListChangeRequests,
            )?);
        }
        Ok(summaries)
    }

    pub(super) fn observe_change_request(
        &self,
        id: &ChangeRequestId,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        self.observe_change_request_for(id, RemoteOperation::ObserveChangeRequest)
    }

    fn observe_change_request_for(
        &self,
        id: &ChangeRequestId,
        operation: RemoteOperation,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        let merge_request = self.change_request_metadata(id, operation)?;
        self.summary_from_metadata(merge_request, operation)
    }

    fn change_request_metadata(
        &self,
        id: &ChangeRequestId,
        operation: RemoteOperation,
    ) -> Result<GitLabMergeRequest, RemoteError> {
        self.validate_change_request_id(id, operation)?;
        let iid = required_iid(id, operation)?;
        let merge_request: GitLabMergeRequest =
            self.get_json(operation, &self.merge_request_endpoint(iid))?;
        if merge_request.id.to_string() != id.native_id().as_str() {
            return Err(invalid_response(
                operation,
                "GitLab returned a different global merge request ID",
            ));
        }
        Ok(merge_request)
    }

    pub(super) fn fetch_metadata(
        &self,
        id: &ChangeRequestId,
    ) -> Result<FetchChangeRequest, RemoteError> {
        let summary = self.observe_change_request_for(id, RemoteOperation::FetchChangeRequest)?;
        Ok(FetchChangeRequest {
            id: summary.change_request.id,
            source_repository: summary.change_request.source_repository,
            source_branch: summary.change_request.source_branch,
            expected_head_sha: summary.change_request.head_sha,
        })
    }

    pub(super) fn change_request_details(
        &self,
        change_request: &ChangeRequest,
    ) -> Result<ChangeRequestDetails, RemoteError> {
        let metadata = self
            .change_request_metadata(&change_request.id, RemoteOperation::ObserveChangeRequest)?;
        let requested_reviewers = metadata.reviewers.clone();
        let observed =
            self.summary_from_metadata(metadata, RemoteOperation::ObserveChangeRequest)?;
        ensure_association(
            &observed.change_request,
            change_request,
            RemoteOperation::ObserveChangeRequest,
            "merge request association changed before details were loaded",
        )?;
        let change_request = &observed.change_request;
        let iid = required_iid(&change_request.id, RemoteOperation::ObserveChangeRequest)?;
        let association = Some(change_request.head_association());

        let notes = self.notes(iid);
        let discussions = self.discussions(iid);
        let reviews = self.reviews(iid, requested_reviewers);
        let changed_files = self.changed_files(iid);
        let (checks, ci_failures) = self.checks_and_failures(change_request, iid);

        let details = ChangeRequestDetails {
            association,
            comments: notes,
            reviews,
            review_threads: discussions,
            changed_files,
            checks,
            ci_failures,
        };

        let after = self.observe_change_request(&change_request.id)?;
        ensure_association(
            &after.change_request,
            change_request,
            RemoteOperation::ObserveChangeRequest,
            "merge request association changed while details were loaded",
        )?;
        Ok(details)
    }

    pub(super) fn repository_policy(
        &self,
        target_branch: &str,
    ) -> Result<RepositoryPolicy, RemoteError> {
        if target_branch.trim().is_empty() {
            return Err(remote_error(
                RemoteOperation::ObserveRepositoryPolicy,
                RemoteErrorClass::Validation,
                Retryability::NotRetryable,
                "target branch must not be empty",
            ));
        }
        let project: GitLabProject = match self.get_json(
            RemoteOperation::ObserveRepositoryPolicy,
            &format!(
                "projects/{}",
                encode_path_segment(self.repository.project_path())
            ),
        ) {
            Ok(project) => project,
            Err(error) => {
                return Ok(unavailable_policy(
                    self.repository.clone(),
                    target_branch,
                    error,
                ));
            }
        };
        if project.id == 0 {
            return Err(invalid_response(
                RemoteOperation::ObserveRepositoryPolicy,
                "GitLab project metadata is missing id",
            ));
        }

        let approval_rules = self.paginated::<GitLabApprovalRule>(
            RemoteOperation::ObserveRepositoryPolicy,
            &format!("projects/{}/approval_rules", project.id),
        );
        let external_checks = self.paginated::<GitLabExternalStatusCheck>(
            RemoteOperation::ObserveRepositoryPolicy,
            &format!("projects/{}/external_status_checks", project.id),
        );
        let protected_branches = self.paginated::<GitLabProtectedBranch>(
            RemoteOperation::ObserveRepositoryPolicy,
            &format!("projects/{}/protected_branches", project.id),
        );

        let (target_is_protected, protection_error) = match protected_branches {
            Ok(page) => (
                Some(
                    page.items
                        .iter()
                        .any(|branch| wildcard_matches(&branch.name, target_branch)),
                ),
                page.partial_error,
            ),
            Err(error) => (None, Some(error)),
        };
        let required_approvals = policy_approvals(
            approval_rules,
            target_branch,
            target_is_protected,
            protection_error,
            project.approvals_before_merge,
        );
        let required_checks = policy_checks(
            external_checks,
            target_branch,
            project.only_allow_merge_if_pipeline_succeeds,
        );
        let (source_must_be_up_to_date, queue_required) = project_merge_requirements(&project);

        Ok(RepositoryPolicy {
            repository: Some(self.repository.clone()),
            target_branch: target_branch.to_string(),
            facts: PolicyFacts {
                required_checks,
                required_approvals,
                conversations_must_be_resolved: option_fact(
                    project.only_allow_merge_if_all_discussions_are_resolved,
                ),
                source_must_be_up_to_date,
                queue_required,
            },
        })
    }

    pub(super) fn create_change_request(
        &self,
        request: &CreateChangeRequest,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        self.validate_target_repository(
            &request.target_repository,
            RemoteOperation::CreateChangeRequest,
        )?;
        validate_expected_sha(
            &request.expected_head_sha,
            RemoteOperation::CreateChangeRequest,
        )?;
        validate_mutation_text(
            &request.title,
            "title",
            RemoteOperation::CreateChangeRequest,
        )?;
        validate_mutation_text(&request.body, "body", RemoteOperation::CreateChangeRequest)?;
        if request.source_repository.provider() != ProviderKind::GitLab
            || request.source_repository.host() != self.repository.host()
        {
            return Err(remote_error(
                RemoteOperation::CreateChangeRequest,
                RemoteErrorClass::Validation,
                Retryability::NotRetryable,
                "source repository must be on the adapter's GitLab host",
            ));
        }

        let source: GitLabProject = self.get_json(
            RemoteOperation::CreateChangeRequest,
            &format!(
                "projects/{}",
                encode_path_segment(request.source_repository.project_path())
            ),
        )?;
        if source.id == 0 || source.path_with_namespace != request.source_repository.project_path()
        {
            return Err(invalid_response(
                RemoteOperation::CreateChangeRequest,
                "GitLab returned different source-project metadata",
            ));
        }
        let target = if request.source_repository == request.target_repository {
            source.clone()
        } else {
            let target: GitLabProject = self.get_json(
                RemoteOperation::CreateChangeRequest,
                &format!(
                    "projects/{}",
                    encode_path_segment(request.target_repository.project_path())
                ),
            )?;
            if target.id == 0
                || target.path_with_namespace != request.target_repository.project_path()
            {
                return Err(invalid_response(
                    RemoteOperation::CreateChangeRequest,
                    "GitLab returned different target-project metadata",
                ));
            }
            target
        };
        let source_branch: GitLabBranch = self.get_json(
            RemoteOperation::CreateChangeRequest,
            &format!(
                "projects/{}/repository/branches/{}",
                source.id,
                encode_path_segment(&request.source_branch)
            ),
        )?;
        ensure_head(
            &source_branch.commit.id,
            &request.expected_head_sha,
            RemoteOperation::CreateChangeRequest,
        )?;
        let title = if request.draft && !has_draft_prefix(&request.title) {
            format!("Draft: {}", request.title)
        } else {
            request.title.clone()
        };
        let (endpoint, fields) = create_merge_request_request(&source, &target, request, title);
        let created: GitLabMergeRequest = self.api_json(
            RemoteOperation::CreateChangeRequest,
            &endpoint,
            "POST",
            &fields,
        )?;
        if created.sha != request.expected_head_sha {
            return Err(stale_head(
                RemoteOperation::CreateChangeRequest,
                "created merge request does not reference the expected source SHA",
            ));
        }
        if created.source_project_id != Some(source.id)
            || created.target_project_id != Some(target.id)
            || created.source_branch != request.source_branch
            || created.target_branch != request.target_branch
        {
            return Err(invalid_response(
                RemoteOperation::CreateChangeRequest,
                "created merge request does not match the requested source and target",
            ));
        }
        let id = ChangeRequestId::new(
            self.repository.clone(),
            NativeChangeRequestId::new(created.id.to_string()).map_err(|error| {
                invalid_response(
                    RemoteOperation::CreateChangeRequest,
                    &format!("invalid merge request ID: {error}"),
                )
            })?,
            Some(created.iid),
        );
        self.observe_change_request_for(&id, RemoteOperation::CreateChangeRequest)
    }

    pub(super) fn resolve_review_thread(
        &self,
        request: &ResolveReviewThread,
    ) -> Result<ChangeRequestDetails, RemoteError> {
        let summary =
            self.observe_change_request_for(&request.id, RemoteOperation::ResolveReviewThread)?;
        ensure_head(
            &summary.change_request.head_sha,
            &request.expected_head_sha,
            RemoteOperation::ResolveReviewThread,
        )?;
        let iid = required_iid(&request.id, RemoteOperation::ResolveReviewThread)?;
        let endpoint = format!(
            "{}/discussions/{}",
            self.merge_request_endpoint(iid),
            encode_path_segment(request.thread_id.as_str())
        );
        let discussion: GitLabDiscussion =
            self.get_json(RemoteOperation::ResolveReviewThread, &endpoint)?;
        let thread = discussion_to_thread(discussion)?;
        if thread.native_id != request.thread_id {
            return Err(invalid_response(
                RemoteOperation::ResolveReviewThread,
                "GitLab returned a different discussion ID",
            ));
        }
        if !thread.resolvable {
            return Err(remote_error(
                RemoteOperation::ResolveReviewThread,
                RemoteErrorClass::Validation,
                Retryability::NotRetryable,
                "GitLab discussion is not resolvable",
            ));
        }
        let immediately_before =
            self.observe_change_request_for(&request.id, RemoteOperation::ResolveReviewThread)?;
        ensure_head(
            &immediately_before.change_request.head_sha,
            &request.expected_head_sha,
            RemoteOperation::ResolveReviewThread,
        )?;
        let resolved: GitLabDiscussion = self.api_json(
            RemoteOperation::ResolveReviewThread,
            &endpoint,
            "PUT",
            &[("resolved", "true".to_string())],
        )?;
        if resolved.id != request.thread_id.as_str()
            || !resolved
                .notes
                .iter()
                .any(|note| note.resolved == Some(true))
        {
            return Err(remote_error(
                RemoteOperation::ResolveReviewThread,
                RemoteErrorClass::Provider,
                Retryability::Unknown,
                "GitLab did not confirm resolution of the requested discussion",
            ));
        }
        let observed =
            self.observe_change_request_for(&request.id, RemoteOperation::ResolveReviewThread)?;
        ensure_head(
            &observed.change_request.head_sha,
            &request.expected_head_sha,
            RemoteOperation::ResolveReviewThread,
        )?;
        self.change_request_details(&observed.change_request)
    }

    pub(super) fn merge_change_request(
        &self,
        request: &GuardedMerge,
    ) -> Result<MergeMutationResult, RemoteError> {
        if request.method == MergeMethod::Rebase {
            return Err(remote_error(
                RemoteOperation::MergeChangeRequest,
                RemoteErrorClass::Unsupported,
                Retryability::NotRetryable,
                "GitLab adapter does not support rebase merges",
            ));
        }
        self.validate_target_repository(
            &request.target_repository,
            RemoteOperation::MergeChangeRequest,
        )?;
        let before =
            self.observe_change_request_for(&request.id, RemoteOperation::MergeChangeRequest)?;
        if before.change_request.target_repository != request.target_repository
            || before.change_request.target_branch != request.target_branch
        {
            return Err(remote_error(
                RemoteOperation::MergeChangeRequest,
                RemoteErrorClass::Conflict,
                Retryability::NotRetryable,
                "merge request target changed since authorization",
            ));
        }
        ensure_head(
            &before.change_request.head_sha,
            &request.expected_source_sha,
            RemoteOperation::MergeChangeRequest,
        )?;
        let iid = required_iid(&request.id, RemoteOperation::MergeChangeRequest)?;
        let fields = merge_fields(request)?;
        let accepted: GitLabMergeRequest = self.api_json(
            RemoteOperation::MergeChangeRequest,
            &format!("{}/merge", self.merge_request_endpoint(iid)),
            "PUT",
            &fields,
        )?;
        if accepted.id.to_string() != request.id.native_id().as_str()
            || accepted.iid != iid
            || accepted.sha != request.expected_source_sha
        {
            return Err(invalid_response(
                RemoteOperation::MergeChangeRequest,
                "GitLab merge response does not match the guarded merge request",
            ));
        }
        let mut observed =
            self.observe_change_request_for(&request.id, RemoteOperation::MergeChangeRequest)?;
        if observed.change_request.target_repository != request.target_repository
            || observed.change_request.target_branch != request.target_branch
        {
            return Err(remote_error(
                RemoteOperation::MergeChangeRequest,
                RemoteErrorClass::Conflict,
                Retryability::NotRetryable,
                "merge request target changed during merge",
            ));
        }
        ensure_head(
            &observed.change_request.head_sha,
            &request.expected_source_sha,
            RemoteOperation::MergeChangeRequest,
        )?;
        if observed.lifecycle == LifecycleState::Merged {
            return Ok(merge_mutation_result(observed, &accepted));
        }
        let accepted_queue = queue_state(&accepted);
        if observed.queue_state == QueueState::NotQueued && accepted_queue != QueueState::NotQueued
        {
            observed.queue_state = accepted_queue;
            observed.native_state_evidence.queue = native_queue_evidence(&accepted);
        }
        Ok(merge_mutation_result(observed, &accepted))
    }

    fn summary_from_metadata(
        &self,
        merge_request: GitLabMergeRequest,
        operation: RemoteOperation,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        let source_id = required_project_id(
            merge_request.source_project_id,
            "source_project_id",
            operation,
        )?;
        let target_id = required_project_id(
            merge_request.target_project_id,
            "target_project_id",
            operation,
        )?;
        let source = self.project_repository(source_id, operation)?;
        let target = self.project_repository(target_id, operation)?;
        if target != self.repository {
            return Err(invalid_response(
                operation,
                "GitLab returned a merge request for a different target project",
            ));
        }
        let (review, review_evidence) =
            self.review_decision(merge_request.iid, &merge_request, operation);
        summary_from_merge_request(
            &self.repository,
            source,
            target,
            merge_request,
            review,
            review_evidence,
            operation,
        )
    }

    fn review_decision(
        &self,
        iid: u64,
        merge_request: &GitLabMergeRequest,
        operation: RemoteOperation,
    ) -> (ReviewDecision, Vec<String>) {
        if merge_request.detailed_merge_status == "requested_changes" {
            return (
                ReviewDecision::ChangesRequested,
                vec![merge_request.detailed_merge_status.clone()],
            );
        }
        let endpoint = format!("{}/approvals", self.merge_request_endpoint(iid));
        match self.get_json::<GitLabApprovals>(operation, &endpoint) {
            Ok(approvals) => {
                let evidence = vec![
                    format!("approved={}", approvals.approved),
                    format!("approvals_required={}", approvals.approvals_required),
                    format!("approvals_left={}", approvals.approvals_left),
                ];
                let decision = if approvals.approved {
                    ReviewDecision::Approved
                } else if approvals.approvals_required > 0 || approvals.approvals_left > 0 {
                    ReviewDecision::ReviewRequired
                } else {
                    ReviewDecision::Pending
                };
                (decision, evidence)
            }
            Err(error) => (
                ReviewDecision::Unknown(format!(
                    "approval_state_unavailable:{}",
                    error_class_label(error.class())
                )),
                Vec::new(),
            ),
        }
    }

    fn notes(&self, iid: u64) -> Observation<Vec<Comment>> {
        let endpoint = format!(
            "{}/notes?sort=asc&order_by=created_at",
            self.merge_request_endpoint(iid)
        );
        match self.paginated::<GitLabNote>(RemoteOperation::ObserveChangeRequest, &endpoint) {
            Ok(page) => page_observation(page, |note| {
                Ok(Comment {
                    native_id: note.id.to_string(),
                    author: note.author.username,
                    body: bounded_prefix(&note.body, MAX_BODY_BYTES),
                    created_at: note.created_at,
                    path: None,
                    line: None,
                })
            }),
            Err(error) => observation_error(error),
        }
    }

    fn discussions(&self, iid: u64) -> Observation<Vec<ReviewThread>> {
        let endpoint = format!("{}/discussions", self.merge_request_endpoint(iid));
        match self.paginated::<GitLabDiscussion>(RemoteOperation::ObserveReviewThreads, &endpoint) {
            Ok(mut page) => {
                page.items.retain(|discussion| {
                    !discussion.individual_note
                        && discussion
                            .notes
                            .iter()
                            .any(|note| note.position.is_some() || note.resolvable.unwrap_or(false))
                });
                page_observation(page, discussion_to_thread)
            }
            Err(error) => observation_error(error),
        }
    }

    fn reviews(&self, iid: u64, requested_reviewers: Vec<GitLabUser>) -> Observation<Vec<Review>> {
        let endpoint = format!("{}/approvals", self.merge_request_endpoint(iid));
        let mut requested = requested_reviews(requested_reviewers);
        match self.get_json::<GitLabApprovals>(RemoteOperation::ObserveChangeRequest, &endpoint) {
            Ok(approvals) => {
                for approval in approvals.approved_by {
                    requested.retain(|review| review.author != approval.user.username);
                    requested.push(Review {
                        native_id: reviewer_identity(&approval.user),
                        author: approval.user.username,
                        decision: ReviewDecision::Approved,
                        body: String::new(),
                        submitted_at: None,
                    });
                }
                requested.sort_by(|left, right| left.author.cmp(&right.author));
                requested.dedup_by(|left, right| left.author == right.author);
                values_observation(requested)
            }
            Err(error) => partial_observation(requested, Some(error)),
        }
    }

    fn changed_files(&self, iid: u64) -> Observation<Vec<String>> {
        let endpoint = format!("{}/diffs", self.merge_request_endpoint(iid));
        match self.paginated::<GitLabDiff>(RemoteOperation::ObserveChangedFiles, &endpoint) {
            Ok(page) => page_observation(page, |diff| {
                let path = if diff.new_path.is_empty() {
                    diff.old_path
                } else {
                    diff.new_path
                };
                if path.is_empty() {
                    Err(invalid_response(
                        RemoteOperation::ObserveChangedFiles,
                        "GitLab diff is missing both paths",
                    ))
                } else {
                    Ok(path)
                }
            }),
            Err(error) => observation_error(error),
        }
    }

    fn checks_and_failures(
        &self,
        change_request: &ChangeRequest,
        iid: u64,
    ) -> (Observation<Vec<CheckContext>>, Observation<Vec<CiFailure>>) {
        let pipeline_endpoint = format!("{}/pipelines", self.merge_request_endpoint(iid));
        let pipelines = match self
            .paginated::<GitLabPipeline>(RemoteOperation::ObserveChecks, &pipeline_endpoint)
        {
            Ok(page) => page,
            Err(error) => return (observation_error(error.clone()), observation_error(error)),
        };
        let mut ci_errors = pipelines
            .partial_error
            .clone()
            .into_iter()
            .map(|error| reclassify_error(error, RemoteOperation::LoadCiLogs))
            .collect::<Vec<_>>();
        let mut errors = pipelines.partial_error.into_iter().collect::<Vec<_>>();
        let mut pipelines = pipelines.items;
        if pipelines.len() > MAX_CHANGE_REQUEST_PIPELINES {
            pipelines.truncate(MAX_CHANGE_REQUEST_PIPELINES);
            errors.push(invalid_response(
                RemoteOperation::ObserveChecks,
                "GitLab returned more merge-request pipelines than the adapter limit",
            ));
            ci_errors.push(invalid_response(
                RemoteOperation::LoadCiLogs,
                "GitLab returned more merge-request pipelines than the adapter limit",
            ));
        }

        let pipeline_evidence = pipelines
            .into_iter()
            .map(|pipeline| {
                let evidence = pipeline_evidence(&pipeline, change_request, iid);
                (pipeline, evidence)
            })
            .filter(|(_, evidence)| evidence.include)
            .collect::<Vec<_>>();
        let has_authoritative_pipeline = pipeline_evidence
            .iter()
            .any(|(_, evidence)| evidence.authoritative);

        let mut checks = Vec::new();
        let mut failed_jobs = Vec::new();
        let mut authoritative_pipeline_states = Vec::new();
        for (pipeline, evidence) in pipeline_evidence {
            if let Some(error) = pipeline_association_error(
                RemoteOperation::ObserveChecks,
                &evidence,
                has_authoritative_pipeline,
            ) {
                errors.push(error);
                ci_errors.push(unassociated_pipeline_error(
                    RemoteOperation::LoadCiLogs,
                    &evidence.identity,
                ));
            }
            checks.push(CheckContext {
                name: evidence.identity.clone(),
                state: gitlab_check_state(&pipeline.status),
                native_state: pipeline.status.clone(),
                web_url: pipeline.web_url.clone(),
            });
            if evidence.authoritative {
                authoritative_pipeline_states.push(gitlab_check_state(&pipeline.status));
            } else {
                // Keep provenance visible, but never let merged-result, train, or
                // unknown jobs stand in for exact source-head checks and failures.
                continue;
            }
            let Some(project_id) = pipeline.project_id.filter(|id| *id > 0) else {
                errors.push(invalid_response(
                    RemoteOperation::ObserveChecks,
                    "GitLab pipeline is missing project_id",
                ));
                ci_errors.push(invalid_response(
                    RemoteOperation::LoadCiLogs,
                    "GitLab pipeline is missing project_id",
                ));
                continue;
            };
            let endpoint = format!("projects/{project_id}/pipelines/{}/jobs", pipeline.id);
            match self.paginated::<GitLabJob>(RemoteOperation::ObserveChecks, &endpoint) {
                Ok(page) => {
                    if let Some(error) = page.partial_error.clone() {
                        ci_errors.push(reclassify_error(error, RemoteOperation::LoadCiLogs));
                    }
                    errors.extend(page.partial_error);
                    for job in page.items {
                        checks.push(CheckContext {
                            name: job.name.clone(),
                            state: gitlab_check_state(&job.status),
                            native_state: job.status.clone(),
                            web_url: job.web_url.clone(),
                        });
                        if matches!(job.status.as_str(), "failed" | "failure" | "error") {
                            failed_jobs.push((project_id, evidence.identity.clone(), job));
                        }
                    }
                }
                Err(error) => {
                    ci_errors.push(reclassify_error(error.clone(), RemoteOperation::LoadCiLogs));
                    errors.push(error);
                }
            }
        }
        if !authoritative_pipeline_states.is_empty() {
            checks.push(CheckContext {
                name: "pipeline".to_string(),
                state: aggregate_pipeline_states(&authoritative_pipeline_states),
                native_state: "aggregate".to_string(),
                web_url: None,
            });
        }

        let status_endpoint = format!(
            "projects/{}/repository/commits/{}/statuses?order_by=id&sort=desc",
            encode_path_segment(change_request.source_repository.project_path()),
            encode_path_segment(&change_request.head_sha)
        );
        match self.paginated::<GitLabCommitStatus>(RemoteOperation::ObserveChecks, &status_endpoint)
        {
            Ok(page) => {
                errors.extend(page.partial_error);
                checks.extend(page.items.into_iter().map(|status| CheckContext {
                    name: status.name,
                    state: gitlab_check_state(&status.status),
                    native_state: status.status,
                    web_url: status.target_url,
                }));
            }
            Err(error) => errors.push(error),
        }

        let external_status_endpoint =
            format!("{}/status_checks", self.merge_request_endpoint(iid));
        match self.paginated::<GitLabMergeRequestStatusCheck>(
            RemoteOperation::ObserveChecks,
            &external_status_endpoint,
        ) {
            Ok(page) => {
                errors.extend(page.partial_error);
                checks.extend(page.items.into_iter().map(|status| CheckContext {
                    name: status.name,
                    state: gitlab_check_state(&status.status),
                    native_state: status.status,
                    web_url: status.external_url,
                }));
            }
            Err(error) => errors.push(error),
        }

        deduplicate_checks(&mut checks);
        let checks_observation = partial_observation(checks, errors.first().cloned());
        let ci_failures = self.failed_traces(failed_jobs, ci_errors);
        (checks_observation, ci_failures)
    }

    fn failed_traces(
        &self,
        mut jobs: Vec<(u64, String, GitLabJob)>,
        mut errors: Vec<RemoteError>,
    ) -> Observation<Vec<CiFailure>> {
        if jobs.len() > MAX_FAILED_TRACES {
            jobs.truncate(MAX_FAILED_TRACES);
            errors.push(invalid_response(
                RemoteOperation::LoadCiLogs,
                "GitLab returned more failed jobs than the trace limit",
            ));
        }
        let mut failures = Vec::new();
        for (project_id, pipeline, job) in jobs {
            let endpoint = format!("projects/{project_id}/jobs/{}/trace", job.id);
            match self.api_text(RemoteOperation::LoadCiLogs, &endpoint, "GET", &[]) {
                Ok(trace) => failures.push(CiFailure {
                    pipeline,
                    job: job.name,
                    native_conclusion: job.status,
                    web_url: job.web_url,
                    native_run_id: job.id.to_string(),
                    log_tail: bounded_tail(&trace, MAX_TRACE_BYTES),
                }),
                Err(error) => errors.push(error),
            }
        }
        partial_observation(failures, errors.into_iter().next())
    }

    fn project_repository(
        &self,
        project_id: u64,
        operation: RemoteOperation,
    ) -> Result<RemoteRepositoryId, RemoteError> {
        let project: GitLabProject = self.get_json(operation, &format!("projects/{project_id}"))?;
        repository_from_project(self.repository.host().clone(), project, operation)
    }

    fn repository_for_project_id(
        &self,
        project_id: u64,
        projects: &mut BTreeMap<u64, RemoteRepositoryId>,
    ) -> Result<RemoteRepositoryId, RemoteError> {
        if let Some(repository) = projects.get(&project_id) {
            return Ok(repository.clone());
        }
        let repository =
            self.project_repository(project_id, RemoteOperation::ListChangeRequests)?;
        projects.insert(project_id, repository.clone());
        Ok(repository)
    }

    fn validate_change_request_id(
        &self,
        id: &ChangeRequestId,
        operation: RemoteOperation,
    ) -> Result<(), RemoteError> {
        self.validate_target_repository(id.repository(), operation)
    }

    fn validate_target_repository(
        &self,
        repository: &RemoteRepositoryId,
        operation: RemoteOperation,
    ) -> Result<(), RemoteError> {
        if repository != &self.repository {
            return Err(remote_error(
                operation,
                RemoteErrorClass::Validation,
                Retryability::NotRetryable,
                "operation targets a different GitLab repository",
            ));
        }
        Ok(())
    }

    fn merge_request_endpoint(&self, iid: u64) -> String {
        format!(
            "projects/{}/merge_requests/{iid}",
            encode_path_segment(self.repository.project_path())
        )
    }

    fn get_json<T: DeserializeOwned>(
        &self,
        operation: RemoteOperation,
        endpoint: &str,
    ) -> Result<T, RemoteError> {
        self.api_json(operation, endpoint, "GET", &[])
    }

    fn api_json<T: DeserializeOwned>(
        &self,
        operation: RemoteOperation,
        endpoint: &str,
        method: &str,
        fields: &[(&str, String)],
    ) -> Result<T, RemoteError> {
        let raw = self.api_text(operation, endpoint, method, fields)?;
        serde_json::from_str(&raw).map_err(|error| {
            invalid_response(operation, &format!("malformed GitLab response: {error}"))
        })
    }

    fn api_text(
        &self,
        operation: RemoteOperation,
        endpoint: &str,
        method: &str,
        fields: &[(&str, String)],
    ) -> Result<String, RemoteError> {
        let args = api_args(
            &self.repository.host().to_string(),
            self.api_base_override.as_deref(),
            endpoint,
            method,
            fields,
        );
        self.run_api(operation, &args)
    }

    fn api_page<T: DeserializeOwned>(
        &self,
        operation: RemoteOperation,
        endpoint: &str,
    ) -> Result<GitLabApiPage<T>, RemoteError> {
        let mut args = api_args(
            &self.repository.host().to_string(),
            self.api_base_override.as_deref(),
            endpoint,
            "GET",
            &[],
        );
        args.push("--include".to_string());
        let raw = self.run_api(operation, &args)?;
        parse_api_page(&raw, operation)
    }

    fn run_api(&self, operation: RemoteOperation, args: &[String]) -> Result<String, RemoteError> {
        let output = run_output_allow_failure_named(
            Command::new(&self.glab_path).args(args),
            ProcessPolicy::NetworkQuery,
            descriptor_for(operation),
        )
        .map_err(|message| classify_process_error(operation, &message))?;
        classify_output(operation, output)
    }

    fn paginated<T: DeserializeOwned>(
        &self,
        operation: RemoteOperation,
        endpoint: &str,
    ) -> Result<Paginated<T>, RemoteError> {
        collect_pages(
            |page| {
                let separator = if endpoint.contains('?') { '&' } else { '?' };
                let endpoint = format!("{endpoint}{separator}per_page={PAGE_SIZE}&page={page}");
                self.api_page::<T>(operation, &endpoint)
            },
            operation,
        )
    }
}

#[derive(Debug)]
struct Paginated<T> {
    items: Vec<T>,
    partial_error: Option<RemoteError>,
}

#[derive(Debug)]
struct GitLabApiPage<T> {
    items: Vec<T>,
    pagination: GitLabPagination,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GitLabPagination {
    page: usize,
    per_page: usize,
    next_page: Option<usize>,
    total: Option<usize>,
    total_pages: Option<usize>,
}

fn collect_pages<T>(
    mut fetch: impl FnMut(usize) -> Result<GitLabApiPage<T>, RemoteError>,
    operation: RemoteOperation,
) -> Result<Paginated<T>, RemoteError> {
    let mut items = Vec::new();
    let mut page = 1;
    let mut expected_per_page = None;
    let mut expected_total: Option<Option<usize>> = None;
    let mut expected_total_pages: Option<Option<usize>> = None;
    for _ in 0..MAX_PAGES {
        let response = match fetch(page) {
            Ok(response) => response,
            Err(error) if items.is_empty() => return Err(error),
            Err(error) => {
                return Ok(Paginated {
                    items,
                    partial_error: Some(error),
                });
            }
        };
        let pagination = response.pagination;
        let invalid = pagination.page != page
            || pagination.per_page == 0
            || pagination.per_page > PAGE_SIZE
            || response.items.len() > pagination.per_page
            || response.items.is_empty() && pagination.next_page.is_some()
            || pagination.next_page.is_some_and(|next| next != page + 1)
            || expected_per_page.is_some_and(|expected| expected != pagination.per_page)
            || expected_total.is_some_and(|expected| expected != pagination.total)
            || expected_total_pages.is_some_and(|expected| expected != pagination.total_pages)
            || !pagination_totals_are_valid(pagination);
        if invalid {
            return pagination_failure(
                items,
                operation,
                "GitLab returned malformed or discontinuous pagination metadata",
            );
        }
        expected_per_page.get_or_insert(pagination.per_page);
        expected_total.get_or_insert(pagination.total);
        expected_total_pages.get_or_insert(pagination.total_pages);
        items.extend(response.items);
        if pagination.total.is_some_and(|total| items.len() > total) {
            return pagination_failure(
                items,
                operation,
                "GitLab pagination returned more items than its declared total",
            );
        }
        if let Some(next_page) = pagination.next_page {
            if pagination.total.is_some_and(|total| items.len() >= total) {
                return pagination_failure(
                    items,
                    operation,
                    "GitLab pagination continued after reaching its declared total",
                );
            }
            page = next_page;
        } else if pagination.total.is_some_and(|total| items.len() != total) {
            return pagination_failure(
                items,
                operation,
                "GitLab pagination ended before reaching its declared total",
            );
        } else {
            return Ok(Paginated {
                items,
                partial_error: None,
            });
        }
    }
    Ok(Paginated {
        items,
        partial_error: Some(invalid_response(
            operation,
            "GitLab pagination reached the maximum page limit",
        )),
    })
}

fn pagination_totals_are_valid(pagination: GitLabPagination) -> bool {
    match (pagination.total, pagination.total_pages) {
        (Some(0), Some(total_pages)) => {
            matches!(total_pages, 0 | 1) && pagination.page == 1 && pagination.next_page.is_none()
        }
        (Some(total), Some(total_pages)) => {
            total_pages == total.div_ceil(pagination.per_page)
                && pagination.page <= total_pages
                && pagination.next_page.is_some() == (pagination.page < total_pages)
        }
        (None, None) => true,
        _ => false,
    }
}

fn pagination_failure<T>(
    items: Vec<T>,
    operation: RemoteOperation,
    message: &str,
) -> Result<Paginated<T>, RemoteError> {
    let error = invalid_response(operation, message);
    if items.is_empty() {
        Err(error)
    } else {
        Ok(Paginated {
            items,
            partial_error: Some(error),
        })
    }
}

fn parse_api_page<T: DeserializeOwned>(
    raw: &str,
    operation: RemoteOperation,
) -> Result<GitLabApiPage<T>, RemoteError> {
    let (headers, body) = raw
        .split_once("\r\n\r\n")
        .or_else(|| raw.split_once("\n\n"))
        .ok_or_else(|| {
            invalid_response(
                operation,
                "GitLab paginated response is missing HTTP headers",
            )
        })?;
    let mut lines = headers.lines();
    let status = lines.next().unwrap_or_default().trim_end_matches('\r');
    let valid_status = status.starts_with("HTTP/")
        && status
            .split_ascii_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u16>().ok())
            .is_some_and(|status| (200..300).contains(&status));
    if !valid_status {
        return Err(invalid_response(
            operation,
            "GitLab paginated response has a malformed HTTP status line",
        ));
    }
    let headers = lines
        .map(|line| line.trim_end_matches('\r'))
        .map(|line| {
            line.split_once(':').ok_or_else(|| {
                invalid_response(
                    operation,
                    "GitLab paginated response has a malformed HTTP header",
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let page = required_pagination_number(&headers, "X-Page", operation)?;
    let per_page = required_pagination_number(&headers, "X-Per-Page", operation)?;
    let next_page = optional_pagination_number(&headers, "X-Next-Page", true, operation)?;
    let total = optional_pagination_number(&headers, "X-Total", false, operation)?;
    let total_pages = optional_pagination_number(&headers, "X-Total-Pages", false, operation)?;
    if total.is_some() != total_pages.is_some() {
        return Err(invalid_response(
            operation,
            "GitLab paginated response has incomplete total metadata",
        ));
    }
    let items = serde_json::from_str(body.trim()).map_err(|error| {
        invalid_response(
            operation,
            &format!("malformed GitLab paginated response: {error}"),
        )
    })?;
    Ok(GitLabApiPage {
        items,
        pagination: GitLabPagination {
            page,
            per_page,
            next_page,
            total,
            total_pages,
        },
    })
}

fn required_pagination_number(
    headers: &[(&str, &str)],
    name: &str,
    operation: RemoteOperation,
) -> Result<usize, RemoteError> {
    optional_pagination_number(headers, name, false, operation)?.ok_or_else(|| {
        invalid_response(
            operation,
            &format!("GitLab paginated response is missing {name}"),
        )
    })
}

fn optional_pagination_number(
    headers: &[(&str, &str)],
    name: &str,
    required: bool,
    operation: RemoteOperation,
) -> Result<Option<usize>, RemoteError> {
    let mut values = headers
        .iter()
        .filter(|(header, _)| header.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim());
    let value = values.next();
    if values.next().is_some() || required && value.is_none() {
        return Err(invalid_response(
            operation,
            &format!("GitLab paginated response has invalid {name} metadata"),
        ));
    }
    match value {
        Some("") => Ok(None),
        Some(value) => value.parse().map(Some).map_err(|_| {
            invalid_response(
                operation,
                &format!("GitLab paginated response has nonnumeric {name} metadata"),
            )
        }),
        None => Ok(None),
    }
}

fn summary_from_merge_request(
    repository: &RemoteRepositoryId,
    source_repository: RemoteRepositoryId,
    target_repository: RemoteRepositoryId,
    merge_request: GitLabMergeRequest,
    review_decision: ReviewDecision,
    review_evidence: Vec<String>,
    operation: RemoteOperation,
) -> Result<ChangeRequestSummary, RemoteError> {
    if merge_request.id == 0 || merge_request.iid == 0 || merge_request.sha.trim().is_empty() {
        return Err(invalid_response(
            operation,
            "GitLab merge request is missing id, iid, or source SHA",
        ));
    }
    let native_id = NativeChangeRequestId::new(merge_request.id.to_string()).map_err(|error| {
        invalid_response(operation, &format!("invalid merge request ID: {error}"))
    })?;
    let id = ChangeRequestId::new(repository.clone(), native_id, Some(merge_request.iid));
    let lifecycle = lifecycle(&merge_request);
    let mergeability = mergeability(&merge_request.detailed_merge_status);
    let check_state = head_pipeline_state(&merge_request);
    let queue_state = queue_state(&merge_request);
    let native_state_evidence = NativeStateEvidence {
        lifecycle: NativeStateEvidence::retain([merge_request.state.clone()]),
        review: NativeStateEvidence::retain(review_evidence),
        mergeability: NativeStateEvidence::retain([merge_request.detailed_merge_status.clone()]),
        check: NativeStateEvidence::retain(
            merge_request
                .head_pipeline
                .iter()
                .map(|pipeline| pipeline.status.clone()),
        ),
        queue: native_queue_evidence(&merge_request),
    };
    Ok(ChangeRequestSummary {
        change_request: ChangeRequest {
            id,
            source_repository,
            target_repository,
            source_branch: merge_request.source_branch,
            target_branch: merge_request.target_branch,
            head_sha: merge_request.sha,
        },
        title: merge_request.title,
        author: merge_request.author.username,
        body: bounded_prefix(
            &merge_request.description.unwrap_or_default(),
            MAX_BODY_BYTES,
        ),
        web_url: merge_request.web_url,
        lifecycle,
        review_decision,
        requested_reviewers: merge_request
            .reviewers
            .into_iter()
            .map(|reviewer| reviewer.username)
            .collect(),
        mergeability,
        check_state,
        queue_state,
        native_state_evidence,
        draft: merge_request.draft || merge_request.work_in_progress,
        updated_at: merge_request.updated_at,
    })
}

fn lifecycle(merge_request: &GitLabMergeRequest) -> LifecycleState {
    if merge_request.merged_at.is_some() || merge_request.state.eq_ignore_ascii_case("merged") {
        LifecycleState::Merged
    } else {
        LifecycleState::from_native(merge_request.state.clone())
    }
}

fn mergeability(native: &str) -> MergeabilityState {
    match native.trim().to_ascii_lowercase().as_str() {
        "mergeable" | "can_be_merged" => MergeabilityState::Mergeable,
        "conflict" | "conflicting" | "cannot_be_merged" => MergeabilityState::Conflicting,
        "need_rebase" => MergeabilityState::Behind,
        "approvals_syncing"
        | "blocked_status"
        | "ci_must_pass"
        | "ci_still_running"
        | "discussions_not_resolved"
        | "draft_status"
        | "external_status_checks"
        | "merge_request_blocked"
        | "not_approved"
        | "policies_denied"
        | "requested_changes"
        | "security_policy_violations"
        | "status_checks_must_pass" => MergeabilityState::Blocked,
        _ => MergeabilityState::Unknown(native.to_string()),
    }
}

fn gitlab_check_state(native: &str) -> CheckState {
    match native.trim().to_ascii_lowercase().as_str() {
        "created" | "waiting_for_resource" | "preparing" | "manual" | "scheduled" => {
            CheckState::Pending
        }
        _ => CheckState::from_native(native.to_string()),
    }
}

fn head_pipeline_state(merge_request: &GitLabMergeRequest) -> CheckState {
    match merge_request.head_pipeline.as_ref() {
        Some(pipeline) if pipeline.sha == merge_request.sha => gitlab_check_state(&pipeline.status),
        Some(_) => CheckState::Unknown("head_pipeline_sha_mismatch".to_string()),
        None => CheckState::Unknown("not_observed".to_string()),
    }
}

fn queue_state(merge_request: &GitLabMergeRequest) -> QueueState {
    if lifecycle(merge_request) == LifecycleState::Merged {
        QueueState::Complete
    } else if merge_request.merge_when_pipeline_succeeds
        || merge_request.auto_merge_enabled == Some(true)
    {
        QueueState::Queued
    } else {
        QueueState::NotQueued
    }
}

fn native_queue_evidence(merge_request: &GitLabMergeRequest) -> Vec<String> {
    NativeStateEvidence::retain([
        format!(
            "merge_when_pipeline_succeeds={}",
            merge_request.merge_when_pipeline_succeeds
        ),
        format!(
            "auto_merge_enabled={}",
            merge_request
                .auto_merge_enabled
                .map(|enabled| enabled.to_string())
                .unwrap_or_else(|| "null".to_string())
        ),
    ])
}

fn merge_mutation_result(
    summary: ChangeRequestSummary,
    accepted: &GitLabMergeRequest,
) -> MergeMutationResult {
    MergeMutationResult::from_summary(summary, accepted.state.clone())
}

fn discussion_to_thread(discussion: GitLabDiscussion) -> Result<ReviewThread, RemoteError> {
    let native_id = NativeReviewThreadId::new(discussion.id.clone()).map_err(|error| {
        invalid_response(
            RemoteOperation::ObserveReviewThreads,
            &format!("invalid discussion ID: {error}"),
        )
    })?;
    let resolvable = discussion
        .notes
        .iter()
        .any(|note| note.resolvable.unwrap_or(false));
    let resolved = resolvable
        && discussion
            .notes
            .iter()
            .filter(|note| note.resolvable.unwrap_or(false))
            .all(|note| note.resolved.unwrap_or(false));
    let comments = discussion
        .notes
        .into_iter()
        .map(|note| Comment {
            native_id: note.id.to_string(),
            author: note.author.username,
            body: bounded_prefix(&note.body, MAX_BODY_BYTES),
            created_at: note.created_at,
            path: note.position.as_ref().and_then(|position| {
                position
                    .new_path
                    .clone()
                    .filter(|path| !path.trim().is_empty())
                    .or_else(|| {
                        position
                            .old_path
                            .clone()
                            .filter(|path| !path.trim().is_empty())
                    })
            }),
            line: note
                .position
                .and_then(|position| position.new_line.or(position.old_line)),
        })
        .collect();
    Ok(ReviewThread {
        native_id,
        resolvable,
        resolved,
        comments,
    })
}

fn page_observation<T, U>(
    page: Paginated<T>,
    mut convert: impl FnMut(T) -> Result<U, RemoteError>,
) -> Observation<Vec<U>> {
    let mut values = Vec::with_capacity(page.items.len());
    let mut error = page.partial_error;
    for item in page.items {
        match convert(item) {
            Ok(value) => values.push(value),
            Err(item_error) => {
                error.get_or_insert(item_error);
            }
        }
    }
    partial_observation(values, error)
}

fn partial_observation<T>(values: Vec<T>, error: Option<RemoteError>) -> Observation<Vec<T>> {
    match (values.is_empty(), error) {
        (true, None) => Observation::EmptyKnown,
        (false, None) => Observation::Known(values),
        (false, Some(error)) => Observation::Stale {
            value: values,
            error: Some(error),
        },
        (true, Some(error)) => observation_error(error),
    }
}

fn values_observation<T>(values: Vec<T>) -> Observation<Vec<T>> {
    partial_observation(values, None)
}

fn observation_error<T>(error: RemoteError) -> Observation<T> {
    match error.class() {
        RemoteErrorClass::Authentication | RemoteErrorClass::Authorization => {
            Observation::Unauthorized
        }
        RemoteErrorClass::Unsupported => Observation::Unsupported,
        RemoteErrorClass::Configuration => Observation::Unconfigured,
        _ => Observation::Failed(error),
    }
}

fn option_fact<T>(value: Option<T>) -> Observation<T> {
    value.map_or(Observation::NotLoaded, Observation::Known)
}

fn project_merge_requirements(project: &GitLabProject) -> (Observation<bool>, Observation<bool>) {
    let source_must_be_up_to_date = match project.merge_method.as_deref() {
        Some("merge") => Observation::Known(false),
        Some("rebase_merge" | "ff") => Observation::Known(true),
        Some(_) | None => Observation::NotLoaded,
    };
    let queue_required = match (
        project.merge_trains_enabled,
        project.merge_train_enforcement.as_deref(),
        project.merge_trains_skip_train_allowed,
    ) {
        (Some(false), _, _)
        | (Some(true), Some("allow_bypass"), _)
        | (Some(true), None, Some(true)) => Observation::Known(false),
        (Some(true), Some("enforce_for_all_users" | "enforce_with_owner_override"), _) => {
            Observation::Known(true)
        }
        _ => Observation::NotLoaded,
    };

    (source_must_be_up_to_date, queue_required)
}

fn unavailable_policy(
    repository: RemoteRepositoryId,
    target_branch: &str,
    error: RemoteError,
) -> RepositoryPolicy {
    RepositoryPolicy {
        repository: Some(repository),
        target_branch: target_branch.to_string(),
        facts: PolicyFacts {
            required_checks: observation_error(error.clone()),
            required_approvals: observation_error(error.clone()),
            conversations_must_be_resolved: observation_error(error.clone()),
            source_must_be_up_to_date: observation_error(error.clone()),
            queue_required: observation_error(error),
        },
    }
}

fn policy_approvals(
    rules: Result<Paginated<GitLabApprovalRule>, RemoteError>,
    target_branch: &str,
    target_is_protected: Option<bool>,
    protection_error: Option<RemoteError>,
    fallback: Option<u32>,
) -> Observation<u32> {
    match rules {
        Ok(page) => {
            let needs_protection = page
                .items
                .iter()
                .any(|rule| rule.applies_to_all_protected_branches);
            let mut applicable = page.items.into_iter().filter(|rule| {
                rule.protected_branches
                    .iter()
                    .any(|branch| wildcard_matches(&branch.name, target_branch))
                    || rule.applies_to_all_protected_branches
                        && target_is_protected.unwrap_or(false)
                    || rule.protected_branches.is_empty() && !rule.applies_to_all_protected_branches
            });
            let required = applicable
                .by_ref()
                .map(|rule| rule.approvals_required)
                .max()
                .or(fallback)
                .unwrap_or(0);
            let error = page.partial_error.or_else(|| {
                (needs_protection && target_is_protected != Some(true))
                    .then_some(protection_error)
                    .flatten()
            });
            match error {
                Some(error) => Observation::Stale {
                    value: required,
                    error: Some(error),
                },
                None => Observation::Known(required),
            }
        }
        Err(error) => match fallback {
            Some(value) => Observation::Stale {
                value,
                error: Some(error),
            },
            None => observation_error(error),
        },
    }
}

fn policy_checks(
    checks: Result<Paginated<GitLabExternalStatusCheck>, RemoteError>,
    target_branch: &str,
    pipeline_required: Option<bool>,
) -> Observation<Vec<String>> {
    let mut values = Vec::new();
    if pipeline_required == Some(true) {
        values.push("pipeline".to_string());
    }
    match checks {
        Ok(page) => {
            values.extend(page.items.into_iter().filter_map(|check| {
                let applies = check.protected_branches.is_empty()
                    || check
                        .protected_branches
                        .iter()
                        .any(|branch| wildcard_matches(&branch.name, target_branch));
                applies.then_some(check.name)
            }));
            values.sort();
            values.dedup();
            partial_observation(values, page.partial_error)
        }
        Err(error) if pipeline_required.is_some() => Observation::Stale {
            value: values,
            error: Some(error),
        },
        Err(error) => observation_error(error),
    }
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    if pattern == value || pattern == "*" {
        return true;
    }
    let Some((prefix, suffix)) = pattern.split_once('*') else {
        return false;
    };
    value.starts_with(prefix) && value.ends_with(suffix)
}

fn repository_from_project(
    host: super::HostIdentity,
    project: GitLabProject,
    operation: RemoteOperation,
) -> Result<RemoteRepositoryId, RemoteError> {
    if project.id == 0 {
        return Err(invalid_response(
            operation,
            "GitLab project metadata is missing id",
        ));
    }
    RemoteRepositoryId::new(ProviderKind::GitLab, host, &project.path_with_namespace).map_err(
        |error| invalid_response(operation, &format!("invalid GitLab project path: {error}")),
    )
}

fn source_project_unavailable(error: &RemoteError) -> bool {
    matches!(
        error.class(),
        RemoteErrorClass::Authorization | RemoteErrorClass::NotFound
    )
}

fn list_source_repository(
    source_id: u64,
    target_id: u64,
    target: RemoteRepositoryId,
    resolve: impl FnOnce(u64) -> Result<RemoteRepositoryId, RemoteError>,
) -> Result<RemoteRepositoryId, RemoteError> {
    if source_id == target_id {
        return Ok(target);
    }
    match resolve(source_id) {
        Ok(source) => Ok(source),
        Err(error) if source_project_unavailable(&error) => Err(incomplete_change_request_list(
            "GitLab merge-request list is incomplete because a source project is hidden or deleted",
            Some(&error),
        )),
        Err(error) => Err(error),
    }
}

fn incomplete_change_request_list(message: &str, cause: Option<&RemoteError>) -> RemoteError {
    let mut error = invalid_response(RemoteOperation::ListChangeRequests, message);
    if let Some(status) = cause.and_then(RemoteError::status) {
        error = error.with_status(status);
    }
    if let Some(exit_code) = cause.and_then(RemoteError::exit_code) {
        error = error.with_exit_code(exit_code);
    }
    error
}

fn reviewer_identity(reviewer: &GitLabUser) -> String {
    if reviewer.id > 0 {
        reviewer.id.to_string()
    } else {
        format!("reviewer:{}", reviewer.username)
    }
}

fn requested_reviews(reviewers: Vec<GitLabUser>) -> Vec<Review> {
    reviewers
        .into_iter()
        .filter(|reviewer| !reviewer.username.trim().is_empty())
        .map(|reviewer| Review {
            native_id: reviewer_identity(&reviewer),
            author: reviewer.username,
            decision: ReviewDecision::Pending,
            body: String::new(),
            submitted_at: None,
        })
        .collect()
}

fn required_project_id(
    value: Option<u64>,
    field: &str,
    operation: RemoteOperation,
) -> Result<u64, RemoteError> {
    value.filter(|value| *value > 0).ok_or_else(|| {
        invalid_response(
            operation,
            &format!("GitLab merge request is missing {field}"),
        )
    })
}

fn required_iid(id: &ChangeRequestId, operation: RemoteOperation) -> Result<u64, RemoteError> {
    id.display_number().filter(|iid| *iid > 0).ok_or_else(|| {
        remote_error(
            operation,
            RemoteErrorClass::Validation,
            Retryability::NotRetryable,
            "GitLab operation requires the project-local merge request iid",
        )
    })
}

fn validate_expected_sha(sha: &str, operation: RemoteOperation) -> Result<(), RemoteError> {
    let valid = matches!(sha.len(), 40 | 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit());
    if valid {
        Ok(())
    } else {
        Err(remote_error(
            operation,
            RemoteErrorClass::Validation,
            Retryability::NotRetryable,
            "expected source SHA is invalid",
        ))
    }
}

fn ensure_head(
    observed: &str,
    expected: &str,
    operation: RemoteOperation,
) -> Result<(), RemoteError> {
    validate_expected_sha(expected, operation)?;
    if observed == expected {
        Ok(())
    } else {
        Err(stale_head(
            operation,
            "merge request source SHA changed since authorization",
        ))
    }
}

fn ensure_association(
    observed: &ChangeRequest,
    expected: &ChangeRequest,
    operation: RemoteOperation,
    message: &str,
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
        Err(stale_head(operation, message))
    }
}

fn validate_mutation_text(
    value: &str,
    name: &str,
    operation: RemoteOperation,
) -> Result<(), RemoteError> {
    if value.len() <= MAX_BODY_BYTES {
        Ok(())
    } else {
        Err(remote_error(
            operation,
            RemoteErrorClass::Validation,
            Retryability::NotRetryable,
            format!("{name} exceeds the adapter body limit"),
        ))
    }
}

fn has_draft_prefix(title: &str) -> bool {
    let title = title.trim_start().to_ascii_lowercase();
    title.starts_with("draft:") || title.starts_with("wip:")
}

fn create_merge_request_request(
    source: &GitLabProject,
    target: &GitLabProject,
    request: &CreateChangeRequest,
    title: String,
) -> (String, Vec<(&'static str, String)>) {
    let endpoint = format!("projects/{}/merge_requests", source.id);
    let mut fields = vec![
        ("source_branch", request.source_branch.clone()),
        ("target_branch", request.target_branch.clone()),
        ("title", title),
        ("description", request.body.clone()),
    ];
    if source.id != target.id {
        fields.push(("target_project_id", target.id.to_string()));
    }
    (endpoint, fields)
}

fn merge_fields(request: &GuardedMerge) -> Result<Vec<(&'static str, String)>, RemoteError> {
    validate_expected_sha(
        &request.expected_source_sha,
        RemoteOperation::MergeChangeRequest,
    )?;
    let squash = match request.method {
        MergeMethod::Merge => "false",
        MergeMethod::Squash => "true",
        MergeMethod::Rebase => {
            return Err(remote_error(
                RemoteOperation::MergeChangeRequest,
                RemoteErrorClass::Unsupported,
                Retryability::NotRetryable,
                "GitLab merge API cannot request rebase as a per-merge method",
            ));
        }
    };
    Ok(vec![
        ("sha", request.expected_source_sha.clone()),
        ("squash", squash.to_string()),
    ])
}

fn deduplicate_checks(checks: &mut Vec<CheckContext>) {
    let mut seen = BTreeSet::new();
    checks.retain(|check| {
        seen.insert((
            check.name.clone(),
            check.native_state.clone(),
            check.web_url.clone(),
        ))
    });
}

fn aggregate_pipeline_states(states: &[CheckState]) -> CheckState {
    if states
        .iter()
        .any(|state| matches!(state, CheckState::Failed | CheckState::Cancelled))
    {
        CheckState::Failed
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
    } else {
        CheckState::Mixed
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitLabPipelineKind {
    SourceBranchHead,
    DetachedMergeRequestHead,
    HistoricalSourceBranch,
    HistoricalDetachedMergeRequest,
    MergedResult,
    MergeTrain,
    Unknown,
}

impl GitLabPipelineKind {
    fn label(self) -> &'static str {
        match self {
            Self::SourceBranchHead => "source-branch-head",
            Self::DetachedMergeRequestHead => "detached-mr-head",
            Self::HistoricalSourceBranch => "historical-source-branch",
            Self::HistoricalDetachedMergeRequest => "historical-detached-mr",
            Self::MergedResult => "merged-result",
            Self::MergeTrain => "merge-train",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug)]
struct GitLabPipelineEvidence {
    identity: String,
    include: bool,
    authoritative: bool,
}

fn pipeline_evidence(
    pipeline: &GitLabPipeline,
    change_request: &ChangeRequest,
    iid: u64,
) -> GitLabPipelineEvidence {
    let detached_ref = format!("refs/merge-requests/{iid}/head");
    let merged_ref = format!("refs/merge-requests/{iid}/merge");
    let train_ref = format!("refs/merge-requests/{iid}/train");
    let ref_name = pipeline.ref_name.as_deref();
    let source = pipeline.source.as_deref();
    let exact_head = pipeline.sha == change_request.head_sha;
    let kind = match (ref_name, source, exact_head) {
        (Some(reference), Some(source), true)
            if reference == change_request.source_branch
                && is_source_branch_pipeline_source(source) =>
        {
            GitLabPipelineKind::SourceBranchHead
        }
        (Some(reference), Some("merge_request_event"), true) if reference == detached_ref => {
            GitLabPipelineKind::DetachedMergeRequestHead
        }
        (Some(reference), Some(source), false)
            if reference == change_request.source_branch
                && is_source_branch_pipeline_source(source) =>
        {
            GitLabPipelineKind::HistoricalSourceBranch
        }
        (Some(reference), Some("merge_request_event"), false) if reference == detached_ref => {
            GitLabPipelineKind::HistoricalDetachedMergeRequest
        }
        (Some(reference), Some("merge_request_event"), _) if reference == merged_ref => {
            GitLabPipelineKind::MergedResult
        }
        (Some(reference), Some("merge_request_event"), _) if reference == train_ref => {
            GitLabPipelineKind::MergeTrain
        }
        _ => GitLabPipelineKind::Unknown,
    };
    let include = !matches!(
        kind,
        GitLabPipelineKind::HistoricalSourceBranch
            | GitLabPipelineKind::HistoricalDetachedMergeRequest
    );
    let authoritative = matches!(
        kind,
        GitLabPipelineKind::SourceBranchHead | GitLabPipelineKind::DetachedMergeRequestHead
    );
    let identity = format!(
        "pipeline:{}:{}:ref={}:source={}",
        pipeline.id,
        kind.label(),
        provenance_component(ref_name),
        provenance_component(source)
    );
    GitLabPipelineEvidence {
        identity,
        include,
        authoritative,
    }
}

fn is_source_branch_pipeline_source(source: &str) -> bool {
    matches!(
        source,
        "push"
            | "web"
            | "trigger"
            | "schedule"
            | "api"
            | "pipeline"
            | "chat"
            | "webide"
            | "parent_pipeline"
    )
}

fn provenance_component(value: Option<&str>) -> String {
    value.map_or_else(|| "missing".to_string(), encode_path_segment)
}

fn unassociated_pipeline_error(operation: RemoteOperation, identity: &str) -> RemoteError {
    invalid_response(
        operation,
        &format!("GitLab pipeline is not authoritative head evidence: {identity}"),
    )
}

fn pipeline_association_error(
    operation: RemoteOperation,
    evidence: &GitLabPipelineEvidence,
    has_authoritative_pipeline: bool,
) -> Option<RemoteError> {
    (!evidence.authoritative && !has_authoritative_pipeline)
        .then(|| unassociated_pipeline_error(operation, &evidence.identity))
}

fn api_args(
    host: &str,
    api_base: Option<&str>,
    endpoint: &str,
    method: &str,
    fields: &[(&str, String)],
) -> Vec<String> {
    let endpoint = api_base.map_or_else(
        || endpoint.to_string(),
        |base| {
            format!(
                "{}/{}",
                base.trim_end_matches('/'),
                endpoint.trim_start_matches('/')
            )
        },
    );
    let mut args = vec![
        "api".to_string(),
        "--hostname".to_string(),
        // glab rejects ports here; its per-host `api_host` setting carries the port.
        host.split_once(':')
            .map_or(host, |(hostname, _)| hostname)
            .to_string(),
        endpoint,
        "--method".to_string(),
        method.to_string(),
    ];
    for (name, value) in fields {
        // Unlike --field, --raw-field never interprets an @-prefixed value as a filename.
        args.push("--raw-field".to_string());
        args.push(format!("{name}={value}"));
    }
    args
}

fn encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

fn bounded_prefix(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }
    let mut boundary = limit;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_string()
}

fn bounded_tail(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }
    let mut boundary = value.len() - limit;
    while !value.is_char_boundary(boundary) {
        boundary += 1;
    }
    value[boundary..].to_string()
}

fn classify_output(
    operation: RemoteOperation,
    output: ProcessOutput,
) -> Result<String, RemoteError> {
    let exit_code = output.status.code();
    if output.stdout_truncated {
        return Err(RemoteError::classified(
            ProviderKind::GitLab,
            operation,
            RemoteErrorClass::InvalidResponse,
            Retryability::Unknown,
            None,
            exit_code,
            None,
        ));
    }
    if output.status.success() {
        return Ok(output.stdout);
    }
    let diagnostic = format!("{}\n{}", output.stderr, output.stdout);
    Err(classify_diagnostic(operation, &diagnostic, exit_code))
}

fn classify_process_error(operation: RemoteOperation, message: &str) -> RemoteError {
    classify_diagnostic(operation, message, None)
}

fn classify_message(operation: RemoteOperation, message: &str) -> RemoteError {
    classify_diagnostic(operation, message, None)
}

fn classify_diagnostic(
    operation: RemoteOperation,
    message: &str,
    exit_code: Option<i32>,
) -> RemoteError {
    let lower = message.to_ascii_lowercase();
    let status = extract_status(&lower);
    let (class, retryability, hint) = if lower.contains("timed out") || lower.contains("timeout") {
        (
            RemoteErrorClass::Timeout,
            Retryability::Retryable,
            Some(RetryHint::Backoff),
        )
    } else if lower.contains("canceled") || lower.contains("cancelled") {
        (RemoteErrorClass::Cancelled, Retryability::Retryable, None)
    } else if status == Some(401)
        || lower.contains("authentication required")
        || lower.contains("not logged in")
        || lower.contains("invalid token")
    {
        (
            RemoteErrorClass::Authentication,
            Retryability::NotRetryable,
            Some(RetryHint::Reauthenticate),
        )
    } else if status == Some(403) || lower.contains("forbidden") {
        (
            RemoteErrorClass::Authorization,
            Retryability::NotRetryable,
            None,
        )
    } else if status == Some(404) || lower.contains("404 not found") {
        (RemoteErrorClass::NotFound, Retryability::NotRetryable, None)
    } else if status == Some(429)
        || lower.contains("rate limit")
        || lower.contains("too many requests")
    {
        (
            RemoteErrorClass::RateLimited,
            Retryability::Retryable,
            Some(RetryHint::Backoff),
        )
    } else if lower.contains("sha does not match")
        || lower.contains("head sha")
        || lower.contains("stale") && lower.contains("sha")
    {
        (
            RemoteErrorClass::StaleHead,
            Retryability::NotRetryable,
            Some(RetryHint::RefreshObservation),
        )
    } else if status == Some(409) || lower.contains("conflict") {
        (RemoteErrorClass::Conflict, Retryability::NotRetryable, None)
    } else if status == Some(400) || status == Some(422) || lower.contains("validation failed") {
        (
            RemoteErrorClass::Validation,
            Retryability::NotRetryable,
            None,
        )
    } else if status.is_some_and(|status| (500..=599).contains(&status)) {
        (
            RemoteErrorClass::Provider,
            Retryability::Retryable,
            Some(RetryHint::Backoff),
        )
    } else if lower.contains("no such file") || lower.contains("failed to start") {
        (
            RemoteErrorClass::Configuration,
            Retryability::NotRetryable,
            None,
        )
    } else if lower.contains("connection")
        || lower.contains("network")
        || lower.contains("dns")
        || lower.contains("tls")
    {
        (
            RemoteErrorClass::Transport,
            Retryability::Retryable,
            Some(RetryHint::Backoff),
        )
    } else {
        (RemoteErrorClass::Provider, Retryability::Unknown, None)
    };
    RemoteError::classified(
        ProviderKind::GitLab,
        operation,
        class,
        retryability,
        status,
        exit_code,
        hint,
    )
}

fn extract_status(message: &str) -> Option<u16> {
    message
        .split(|character: char| !character.is_ascii_digit())
        .find_map(|word| {
            (word.len() == 3)
                .then(|| word.parse::<u16>().ok())
                .flatten()
                .filter(|status| (400..=599).contains(status))
        })
}

fn error_class_label(class: RemoteErrorClass) -> &'static str {
    class.label()
}

fn remote_error(
    operation: RemoteOperation,
    class: RemoteErrorClass,
    retryability: Retryability,
    message: impl AsRef<str>,
) -> RemoteError {
    RemoteError::new(
        ProviderKind::GitLab,
        operation,
        class,
        retryability,
        message,
    )
}

fn reclassify_error(error: RemoteError, operation: RemoteOperation) -> RemoteError {
    RemoteError::classified(
        ProviderKind::GitLab,
        operation,
        error.class(),
        error.retryability(),
        error.status(),
        error.exit_code(),
        error.retry_hint(),
    )
}

fn invalid_response(operation: RemoteOperation, message: &str) -> RemoteError {
    remote_error(
        operation,
        RemoteErrorClass::InvalidResponse,
        Retryability::Unknown,
        message,
    )
}

fn stale_head(operation: RemoteOperation, message: &str) -> RemoteError {
    remote_error(
        operation,
        RemoteErrorClass::StaleHead,
        Retryability::NotRetryable,
        message,
    )
    .with_retry_hint(RetryHint::RefreshObservation)
}

fn descriptor_for(operation: RemoteOperation) -> ProcessDescriptor {
    ProcessDescriptor::new(match operation {
        RemoteOperation::ListChangeRequests => "glab.mr.list",
        RemoteOperation::ObserveChangeRequest => "glab.mr.view",
        RemoteOperation::ObserveReviewThreads => "glab.mr.discussions",
        RemoteOperation::ResolveReviewThread => "glab.mr.discussion.resolve",
        RemoteOperation::ObserveChecks => "glab.mr.pipelines",
        RemoteOperation::LoadCiLogs => "glab.job.trace",
        RemoteOperation::ObserveChangedFiles => "glab.mr.diffs",
        RemoteOperation::ObserveRepositoryPolicy => "glab.repository.policy",
        RemoteOperation::FetchChangeRequest => "glab.mr.fetch_metadata",
        RemoteOperation::CreateChangeRequest => "glab.mr.create",
        RemoteOperation::MergeChangeRequest => "glab.mr.merge",
        RemoteOperation::ObserveMergeQueue => "glab.mr.merge_train",
        RemoteOperation::DiscoverRepository => "glab.repository.metadata",
    })
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabMergeRequest {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    iid: u64,
    #[serde(default)]
    title: String,
    description: Option<String>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    work_in_progress: bool,
    web_url: Option<String>,
    #[serde(default)]
    detailed_merge_status: String,
    #[serde(default)]
    source_branch: String,
    #[serde(default)]
    target_branch: String,
    #[serde(default)]
    sha: String,
    source_project_id: Option<u64>,
    target_project_id: Option<u64>,
    #[serde(default)]
    author: GitLabUser,
    #[serde(default)]
    reviewers: Vec<GitLabUser>,
    updated_at: Option<String>,
    merged_at: Option<String>,
    head_pipeline: Option<GitLabPipeline>,
    #[serde(default)]
    merge_when_pipeline_succeeds: bool,
    auto_merge_enabled: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabProject {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    path_with_namespace: String,
    only_allow_merge_if_pipeline_succeeds: Option<bool>,
    only_allow_merge_if_all_discussions_are_resolved: Option<bool>,
    approvals_before_merge: Option<u32>,
    merge_method: Option<String>,
    merge_trains_enabled: Option<bool>,
    merge_train_enforcement: Option<String>,
    merge_trains_skip_train_allowed: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabBranch {
    #[serde(default)]
    commit: GitLabCommit,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabCommit {
    #[serde(default)]
    id: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabUser {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    username: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabApprovals {
    #[serde(default)]
    approved: bool,
    #[serde(default)]
    approvals_required: u32,
    #[serde(default)]
    approvals_left: u32,
    #[serde(default)]
    approved_by: Vec<GitLabApproval>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabApproval {
    #[serde(default)]
    user: GitLabUser,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabNote {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    body: String,
    #[serde(default)]
    author: GitLabUser,
    created_at: Option<String>,
    resolvable: Option<bool>,
    resolved: Option<bool>,
    position: Option<GitLabPosition>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabPosition {
    new_path: Option<String>,
    old_path: Option<String>,
    new_line: Option<u64>,
    old_line: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabDiscussion {
    #[serde(default)]
    id: String,
    #[serde(default)]
    notes: Vec<GitLabNote>,
    #[serde(default)]
    individual_note: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabDiff {
    #[serde(default)]
    old_path: String,
    #[serde(default)]
    new_path: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabPipeline {
    #[serde(default)]
    id: u64,
    project_id: Option<u64>,
    #[serde(default)]
    sha: String,
    #[serde(default)]
    status: String,
    web_url: Option<String>,
    #[serde(rename = "ref")]
    ref_name: Option<String>,
    source: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabJob {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    web_url: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabCommitStatus {
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    target_url: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabMergeRequestStatusCheck {
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    external_url: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabApprovalRule {
    #[serde(default)]
    approvals_required: u32,
    #[serde(default)]
    applies_to_all_protected_branches: bool,
    #[serde(default)]
    protected_branches: Vec<GitLabBranchReference>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabExternalStatusCheck {
    #[serde(default)]
    name: String,
    #[serde(default)]
    protected_branches: Vec<GitLabBranchReference>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabProtectedBranch {
    #[serde(default)]
    name: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GitLabBranchReference {
    #[serde(default)]
    name: String,
}

#[cfg(test)]
mod tests {
    use super::super::{
        HostIdentity, HostProfile, NativeMergeGuard, RemoteBase, RemoteDiscovery, WebScheme,
    };
    use super::*;

    #[test]
    fn implemented_ci_logs_and_guarded_merge_are_supported_capabilities() {
        let adapter = GitLabAdapter::with_glab_path("glab", repository("group/project"));
        let capabilities = adapter.capabilities();

        assert_eq!(capabilities.ci_logs, SupportLevel::Supported);
        assert_eq!(capabilities.guarded_merge, SupportLevel::Supported);
    }

    #[test]
    fn configured_rebase_disables_guarded_merge_with_shared_reason() {
        let mut adapter = GitLabAdapter::with_glab_path("glab", repository("group/project"));
        adapter.merge_method = crate::config::MergeMethod::Rebase;

        let capabilities = adapter.capabilities();

        assert_eq!(capabilities.guarded_merge, SupportLevel::Unsupported);
        assert_eq!(
            capabilities.guarded_merge_reason.as_deref(),
            Some("GitLab adapter does not support rebase merges")
        );

        for method in [
            crate::config::MergeMethod::Merge,
            crate::config::MergeMethod::Squash,
        ] {
            adapter.merge_method = method;
            assert_eq!(
                adapter.capabilities().guarded_merge,
                SupportLevel::Supported
            );
            assert!(adapter.capabilities().guarded_merge_reason.is_none());
        }
    }

    fn repository(path: &str) -> RemoteRepositoryId {
        RemoteRepositoryId::new(
            ProviderKind::GitLab,
            HostIdentity::new("git.example.com", None).unwrap(),
            path,
        )
        .unwrap()
    }

    fn merge_request(json: &str) -> GitLabMergeRequest {
        serde_json::from_str(json).unwrap()
    }

    fn change_request() -> ChangeRequest {
        ChangeRequest {
            id: ChangeRequestId::new(
                repository("group/project"),
                NativeChangeRequestId::new("1001").unwrap(),
                Some(7),
            ),
            source_repository: repository("group/project"),
            target_repository: repository("group/project"),
            source_branch: "feature/topic".to_string(),
            target_branch: "main".to_string(),
            head_sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
        }
    }

    fn create_request(source: &str, target: &str) -> CreateChangeRequest {
        CreateChangeRequest {
            source_repository: repository(source),
            target_repository: repository(target),
            source_branch: "feature/topic".to_string(),
            target_branch: "main".to_string(),
            expected_head_sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
            title: "Title".to_string(),
            body: "Body".to_string(),
            draft: false,
        }
    }

    fn pipeline(json: &str) -> GitLabPipeline {
        serde_json::from_str(json).unwrap()
    }

    fn api_page<T>(
        items: Vec<T>,
        page: usize,
        per_page: usize,
        next_page: Option<usize>,
        total: Option<usize>,
    ) -> GitLabApiPage<T> {
        GitLabApiPage {
            items,
            pagination: GitLabPagination {
                page,
                per_page,
                next_page,
                total,
                total_pages: total.map(|total| total.div_ceil(per_page).max(1)),
            },
        }
    }

    #[test]
    fn nested_fork_paths_keep_global_id_and_iid() {
        let target = repository("platform/services/widget");
        let source = repository("contributors/alice/widget");
        let mr = merge_request(
            r#"{
                "id": 987654, "iid": 42, "title": "Fork", "description": "body",
                "state": "opened", "draft": false, "source_branch": "feature",
                "target_branch": "main", "sha": "0123456789abcdef0123456789abcdef01234567",
                "source_project_id": 8, "target_project_id": 9,
                "author": {"id": 1, "username": "alice"},
                "detailed_merge_status": "can_be_merged"
            }"#,
        );
        let summary = summary_from_merge_request(
            &target,
            source.clone(),
            target.clone(),
            mr,
            ReviewDecision::Approved,
            Vec::new(),
            RemoteOperation::ListChangeRequests,
        )
        .unwrap();

        assert_eq!(summary.change_request.id.native_id().as_str(), "987654");
        assert_eq!(summary.change_request.id.display_number(), Some(42));
        assert_eq!(summary.change_request.source_repository, source);
        assert_eq!(summary.change_request.target_repository, target);
        assert_eq!(
            summary.change_request.head_sha,
            "0123456789abcdef0123456789abcdef01234567"
        );
        assert_eq!(summary.native_state_evidence.lifecycle, ["opened"]);
        assert_eq!(
            summary.native_state_evidence.mergeability,
            ["can_be_merged"]
        );
        assert_eq!(summary.native_state_evidence.review, Vec::<String>::new());
    }

    #[test]
    fn same_project_and_fork_fixtures_preserve_source_target_identity() {
        let same = merge_request(include_str!(
            "../../tests/fixtures/remote/gitlab/mr-same-project.json"
        ));
        let fork = merge_request(include_str!(
            "../../tests/fixtures/remote/gitlab/mr-fork.json"
        ));

        assert_eq!(same.source_project_id, same.target_project_id);
        assert_ne!(fork.source_project_id, fork.target_project_id);
        assert_eq!(fork.iid, 42);
        assert_eq!(fork.id, 2002);
    }

    #[test]
    fn create_uses_source_project_endpoint_and_target_id_only_for_forks() {
        let source = GitLabProject {
            id: 10,
            path_with_namespace: "contributors/alice/widget".to_string(),
            ..GitLabProject::default()
        };
        let target = GitLabProject {
            id: 20,
            path_with_namespace: "platform/widget".to_string(),
            ..GitLabProject::default()
        };
        let fork_request = create_request("contributors/alice/widget", "platform/widget");
        let (endpoint, fields) =
            create_merge_request_request(&source, &target, &fork_request, "Title".to_string());

        assert_eq!(endpoint, "projects/10/merge_requests");
        assert!(fields.contains(&("target_project_id", "20".to_string())));
        assert!(!fields.iter().any(|(name, _)| *name == "source_project_id"));

        let same_request = create_request("platform/widget", "platform/widget");
        let (endpoint, fields) =
            create_merge_request_request(&target, &target, &same_request, "Title".to_string());
        assert_eq!(endpoint, "projects/20/merge_requests");
        assert!(!fields.iter().any(|(name, _)| *name == "target_project_id"));
    }

    #[test]
    fn requested_reviewers_are_represented_as_pending_reviews() {
        let merge_request = merge_request(include_str!(
            "../../tests/fixtures/remote/gitlab/mr-same-project.json"
        ));
        let reviews = requested_reviews(merge_request.reviewers);

        assert_eq!(reviews.len(), 2);
        assert!(
            reviews
                .iter()
                .all(|review| review.decision == ReviewDecision::Pending)
        );
        assert_eq!(reviews[0].author, "bob");
        assert_eq!(reviews[0].native_id, "2");
    }

    #[test]
    fn merged_and_closed_are_distinct() {
        let mut merged = merge_request(r#"{"state":"closed","merged_at":"2026-01-01T00:00:00Z"}"#);
        let closed = merge_request(r#"{"state":"closed","merged_at":null}"#);

        assert_eq!(lifecycle(&merged), LifecycleState::Merged);
        assert_eq!(lifecycle(&closed), LifecycleState::Closed);
        merged.state = "merged".to_string();
        merged.merged_at = None;
        assert_eq!(lifecycle(&merged), LifecycleState::Merged);
    }

    #[test]
    fn unknown_native_states_are_preserved() {
        assert_eq!(
            mergeability("new_future_status"),
            MergeabilityState::Unknown("new_future_status".to_string())
        );
        assert_eq!(
            gitlab_check_state("future_ci_state"),
            CheckState::Unknown("future_ci_state".to_string())
        );
    }

    #[test]
    fn project_merge_method_is_the_only_authoritative_strict_update_evidence() {
        for (merge_method, expected) in [("merge", false), ("rebase_merge", true), ("ff", true)] {
            let project = GitLabProject {
                merge_method: Some(merge_method.to_string()),
                ..GitLabProject::default()
            };
            let (up_to_date, queue) = project_merge_requirements(&project);

            assert_eq!(up_to_date, Observation::Known(expected));
            assert_eq!(queue, Observation::NotLoaded);
        }

        for merge_method in [None, Some("future_method".to_string())] {
            let project = GitLabProject {
                merge_method,
                ..GitLabProject::default()
            };
            let (up_to_date, queue) = project_merge_requirements(&project);

            assert_eq!(up_to_date, Observation::NotLoaded);
            assert_eq!(queue, Observation::NotLoaded);
        }
    }

    #[test]
    fn queue_requirement_needs_recognized_merge_train_enforcement() {
        for (enabled, enforcement, expected) in [
            (false, "allow_bypass", false),
            (true, "allow_bypass", false),
            (true, "enforce_for_all_users", true),
            (true, "enforce_with_owner_override", true),
        ] {
            let project = GitLabProject {
                merge_method: Some("merge".to_string()),
                merge_trains_enabled: Some(enabled),
                merge_train_enforcement: Some(enforcement.to_string()),
                ..GitLabProject::default()
            };
            let (_, queue) = project_merge_requirements(&project);

            assert_eq!(queue, Observation::Known(expected));
        }

        for (enabled, enforcement) in [
            (None, None),
            (Some(true), None),
            (None, Some("enforce_for_all_users".to_string())),
            (Some(true), Some("future_enforcement".to_string())),
        ] {
            let project = GitLabProject {
                merge_method: Some("merge".to_string()),
                merge_trains_enabled: enabled,
                merge_train_enforcement: enforcement,
                ..GitLabProject::default()
            };
            let (_, queue) = project_merge_requirements(&project);

            assert_eq!(queue, Observation::NotLoaded);
        }

        let project = GitLabProject {
            merge_method: Some("merge".to_string()),
            merge_trains_enabled: Some(false),
            ..GitLabProject::default()
        };
        assert_eq!(
            project_merge_requirements(&project).1,
            Observation::Known(false)
        );
        let project = GitLabProject {
            merge_method: Some("merge".to_string()),
            merge_trains_enabled: Some(true),
            merge_trains_skip_train_allowed: Some(true),
            ..GitLabProject::default()
        };
        assert_eq!(
            project_merge_requirements(&project).1,
            Observation::Known(false)
        );
    }

    #[test]
    fn required_pipeline_aggregate_passes_only_when_every_pipeline_passes() {
        assert_eq!(
            aggregate_pipeline_states(&[CheckState::Passed, CheckState::Skipped]),
            CheckState::Passed
        );
        assert_eq!(
            aggregate_pipeline_states(&[CheckState::Passed, CheckState::Pending]),
            CheckState::Pending
        );
        assert_eq!(
            aggregate_pipeline_states(&[CheckState::Passed, CheckState::Failed]),
            CheckState::Failed
        );
        assert_eq!(
            aggregate_pipeline_states(&[
                CheckState::Passed,
                CheckState::Unknown("future".to_string()),
            ]),
            CheckState::Mixed
        );
    }

    #[test]
    fn embedded_head_pipeline_must_match_merge_request_sha() {
        let exact = merge_request(include_str!(
            "../../tests/fixtures/remote/gitlab/mr-same-project.json"
        ));
        let stale = merge_request(include_str!(
            "../../tests/fixtures/remote/gitlab/mr-mismatched-head-pipeline.json"
        ));

        assert_eq!(head_pipeline_state(&exact), CheckState::Passed);
        assert_eq!(
            head_pipeline_state(&stale),
            CheckState::Unknown("head_pipeline_sha_mismatch".to_string())
        );

        let target = repository("group/project");
        let summary = summary_from_merge_request(
            &target,
            target.clone(),
            target.clone(),
            stale,
            ReviewDecision::Approved,
            vec!["approved=true".to_string()],
            RemoteOperation::ListChangeRequests,
        )
        .unwrap();
        assert_eq!(
            summary.check_state,
            CheckState::Unknown("head_pipeline_sha_mismatch".to_string())
        );
        assert_eq!(summary.native_state_evidence.check, ["success"]);
        assert_eq!(summary.native_state_evidence.review, ["approved=true"]);
    }

    #[test]
    fn source_branch_head_pipeline_is_authoritative_and_labeled() {
        let pipeline = pipeline(
            r#"{"id":11,"sha":"0123456789abcdef0123456789abcdef01234567",
                 "status":"success","ref":"feature/topic","source":"push"}"#,
        );

        let evidence = pipeline_evidence(&pipeline, &change_request(), 7);

        assert!(evidence.include);
        assert!(evidence.authoritative);
        assert_eq!(
            evidence.identity,
            "pipeline:11:source-branch-head:ref=feature%2Ftopic:source=push"
        );
    }

    #[test]
    fn detached_merge_request_head_pipeline_is_authoritative_and_distinct() {
        let pipeline = pipeline(
            r#"{"id":12,"sha":"0123456789abcdef0123456789abcdef01234567",
                 "status":"success","ref":"refs/merge-requests/7/head",
                 "source":"merge_request_event"}"#,
        );

        let evidence = pipeline_evidence(&pipeline, &change_request(), 7);

        assert!(evidence.include);
        assert!(evidence.authoritative);
        assert_eq!(
            evidence.identity,
            "pipeline:12:detached-mr-head:ref=refs%2Fmerge-requests%2F7%2Fhead:source=merge_request_event"
        );
    }

    #[test]
    fn merged_result_and_train_pipelines_are_retained_but_not_authoritative() {
        for (id, reference, kind) in [
            (13, "refs/merge-requests/7/merge", "merged-result"),
            (14, "refs/merge-requests/7/train", "merge-train"),
        ] {
            let pipeline = pipeline(&format!(
                r#"{{"id":{id},"sha":"fedcba9876543210fedcba9876543210fedcba98",
                     "status":"success","ref":"{reference}",
                     "source":"merge_request_event"}}"#
            ));

            let evidence = pipeline_evidence(&pipeline, &change_request(), 7);
            let observation = partial_observation(
                vec![CheckContext {
                    name: evidence.identity.clone(),
                    state: gitlab_check_state(&pipeline.status),
                    native_state: pipeline.status,
                    web_url: None,
                }],
                (!evidence.authoritative).then(|| {
                    unassociated_pipeline_error(RemoteOperation::ObserveChecks, &evidence.identity)
                }),
            );

            assert!(evidence.include);
            assert!(evidence.identity.contains(kind));
            assert!(matches!(observation, Observation::Stale { .. }));
        }
    }

    #[test]
    fn non_authoritative_pipeline_does_not_stale_coexisting_exact_head_evidence() {
        let pipelines: Vec<GitLabPipeline> = serde_json::from_str(include_str!(
            "../../tests/fixtures/remote/gitlab/pipelines-mixed.json"
        ))
        .unwrap();
        let evidence = pipelines
            .iter()
            .map(|pipeline| pipeline_evidence(pipeline, &change_request(), 7))
            .collect::<Vec<_>>();
        let has_authoritative = evidence.iter().any(|item| item.authoritative);

        assert!(has_authoritative);
        assert!(evidence.iter().any(|item| !item.authoritative));
        assert!(evidence.iter().all(|item| {
            pipeline_association_error(RemoteOperation::ObserveChecks, item, has_authoritative)
                .is_none()
        }));
    }

    #[test]
    fn unknown_or_missing_pipeline_provenance_fails_closed() {
        for json in [
            r#"{"id":15,"sha":"0123456789abcdef0123456789abcdef01234567",
                 "status":"success","ref":"feature/topic","source":"future_source"}"#,
            r#"{"id":16,"sha":"0123456789abcdef0123456789abcdef01234567",
                 "status":"success"}"#,
        ] {
            let pipeline = pipeline(json);
            let evidence = pipeline_evidence(&pipeline, &change_request(), 7);
            let observation = partial_observation(
                vec![CheckContext {
                    name: evidence.identity.clone(),
                    state: gitlab_check_state(&pipeline.status),
                    native_state: pipeline.status,
                    web_url: None,
                }],
                (!evidence.authoritative).then(|| {
                    unassociated_pipeline_error(RemoteOperation::ObserveChecks, &evidence.identity)
                }),
            );

            assert!(evidence.include);
            assert!(!evidence.authoritative);
            assert!(evidence.identity.contains(":unknown:"));
            assert!(matches!(observation, Observation::Stale { .. }));
        }
    }

    #[test]
    fn discussion_id_is_the_resolvable_identity() {
        let discussion: GitLabDiscussion = serde_json::from_str(
            r#"{
                "id":"discussion-token-1",
                "notes":[{"id":17,"body":"fix this","author":{"username":"reviewer"},
                          "resolvable":true,"resolved":false,
                          "position":{"new_path":"src/lib.rs","new_line":42}}]
            }"#,
        )
        .unwrap();
        let thread = discussion_to_thread(discussion).unwrap();

        assert_eq!(thread.native_id.as_str(), "discussion-token-1");
        assert_eq!(thread.comments[0].native_id, "17");
        assert_eq!(thread.comments[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(thread.comments[0].line, Some(42));
        assert!(thread.resolvable);
        assert!(!thread.resolved);
    }

    #[test]
    fn pagination_preserves_partial_items_as_stale() {
        let mut calls = 0;
        let page = collect_pages(
            |_| {
                calls += 1;
                if calls == 1 {
                    Ok(api_page(
                        vec![1_u64; PAGE_SIZE],
                        1,
                        PAGE_SIZE,
                        Some(2),
                        None,
                    ))
                } else {
                    Err(remote_error(
                        RemoteOperation::ObserveChangedFiles,
                        RemoteErrorClass::Transport,
                        Retryability::Retryable,
                        "network failed",
                    ))
                }
            },
            RemoteOperation::ObserveChangedFiles,
        )
        .unwrap();
        let observed = page_observation(page, Ok);

        assert!(matches!(observed, Observation::Stale { value, .. } if value.len() == PAGE_SIZE));
    }

    #[test]
    fn pagination_follows_server_page_cap_instead_of_requested_page_size() {
        let mut calls = 0;
        let page = collect_pages(
            |page| {
                calls += 1;
                let response = match page {
                    1 => "HTTP/2.0 200 OK\r\nX-Page: 1\r\nX-Per-Page: 2\r\nX-Next-Page: 2\r\nX-Total: 3\r\nX-Total-Pages: 2\r\n\r\n[1,2]",
                    2 => "HTTP/2.0 200 OK\r\nX-Page: 2\r\nX-Per-Page: 2\r\nX-Next-Page: \r\nX-Total: 3\r\nX-Total-Pages: 2\r\n\r\n[3]",
                    _ => unreachable!(),
                };
                parse_api_page::<u64>(response, RemoteOperation::ListChangeRequests)
            },
            RemoteOperation::ListChangeRequests,
        )
        .unwrap();

        assert_eq!(calls, 2);
        assert_eq!(page.items, vec![1, 2, 3]);
        assert!(page.partial_error.is_none());
    }

    #[test]
    fn pagination_propagates_first_page_errors_without_claiming_absence() {
        let result = collect_pages::<u64>(
            |_| {
                Err(remote_error(
                    RemoteOperation::ListChangeRequests,
                    RemoteErrorClass::RateLimited,
                    Retryability::Retryable,
                    "HTTP 429",
                ))
            },
            RemoteOperation::ListChangeRequests,
        );

        assert_eq!(result.unwrap_err().class(), RemoteErrorClass::RateLimited);
    }

    #[test]
    fn hidden_or_deleted_source_projects_cannot_be_skipped() {
        for (status, class) in [
            (403, RemoteErrorClass::Authorization),
            (404, RemoteErrorClass::NotFound),
        ] {
            let cause = classify_message(
                RemoteOperation::ListChangeRequests,
                &format!("HTTP {status}: source unavailable"),
            );
            assert_eq!(cause.class(), class);

            let error =
                list_source_repository(10, 20, repository("target/project"), |_| Err(cause))
                    .unwrap_err();
            assert_eq!(error.class(), RemoteErrorClass::InvalidResponse);
            assert_eq!(error.status(), Some(status));
            assert!(error.safe_message().contains("incomplete"));
        }
        let authentication = remote_error(
            RemoteOperation::ListChangeRequests,
            RemoteErrorClass::Authentication,
            Retryability::NotRetryable,
            "token unavailable",
        );
        assert!(!source_project_unavailable(&authentication));
    }

    #[test]
    fn paginated_response_requires_complete_consistent_gitlab_headers() {
        let missing_next = "HTTP/2.0 200 OK\r\nX-Page: 1\r\nX-Per-Page: 20\r\n\r\n[1]";
        let incomplete_totals = "HTTP/2.0 200 OK\r\nX-Page: 1\r\nX-Per-Page: 20\r\nX-Next-Page: \r\nX-Total: 1\r\n\r\n[1]";

        for response in [missing_next, incomplete_totals] {
            let error =
                parse_api_page::<u64>(response, RemoteOperation::ListChangeRequests).unwrap_err();
            assert_eq!(error.class(), RemoteErrorClass::InvalidResponse);
        }
    }

    #[test]
    fn malformed_later_page_is_reported_as_partial() {
        let page = collect_pages(
            |page| match page {
                1 => Ok(api_page(vec![1_u64, 2], 1, 2, Some(2), None)),
                2 => parse_api_page(
                    "HTTP/2.0 200 OK\r\nX-Page: 2\r\nX-Per-Page: 2\r\n\r\n[3]",
                    RemoteOperation::ObserveChangedFiles,
                ),
                _ => unreachable!(),
            },
            RemoteOperation::ObserveChangedFiles,
        )
        .unwrap();

        assert_eq!(page.items, vec![1, 2]);
        assert_eq!(
            page.partial_error.unwrap().class(),
            RemoteErrorClass::InvalidResponse
        );
    }

    #[test]
    fn pagination_rejects_totals_that_contradict_the_effective_page_cap() {
        let result = collect_pages(
            |_| {
                Ok(GitLabApiPage {
                    items: vec![1_u64, 2],
                    pagination: GitLabPagination {
                        page: 1,
                        per_page: 2,
                        next_page: Some(2),
                        total: Some(3),
                        total_pages: Some(3),
                    },
                })
            },
            RemoteOperation::ListChangeRequests,
        );

        assert_eq!(
            result.unwrap_err().class(),
            RemoteErrorClass::InvalidResponse
        );
    }

    #[test]
    fn pagination_rejects_a_declared_next_page_after_the_total() {
        let error = collect_pages(
            |_| Ok(api_page(vec![1_u64, 2], 1, 2, Some(2), Some(2))),
            RemoteOperation::ListChangeRequests,
        )
        .unwrap_err();

        assert_eq!(error.class(), RemoteErrorClass::InvalidResponse);
    }

    #[test]
    fn association_mutation_is_rejected_after_multi_call_details() {
        let expected = change_request();
        let mut changed = expected.clone();
        changed.head_sha = "fedcba9876543210fedcba9876543210fedcba98".to_string();

        let error = ensure_association(
            &changed,
            &expected,
            RemoteOperation::ObserveChangeRequest,
            "changed while loading",
        )
        .unwrap_err();
        assert_eq!(error.class(), RemoteErrorClass::StaleHead);
    }

    #[test]
    fn hidden_policy_endpoint_is_stale_or_failed_never_empty_known() {
        let hidden = remote_error(
            RemoteOperation::ObserveRepositoryPolicy,
            RemoteErrorClass::NotFound,
            Retryability::NotRetryable,
            "HTTP 404",
        );
        assert!(matches!(
            policy_checks(Err(hidden.clone()), "main", Some(false)),
            Observation::Stale { value, error: Some(_) } if value.is_empty()
        ));
        assert!(matches!(
            policy_checks(Err(hidden), "main", None),
            Observation::Failed(_)
        ));
    }

    #[test]
    fn queued_merge_is_pending_and_preserves_gitlab_state() {
        let mut queued = merge_request(include_str!(
            "../../tests/fixtures/remote/gitlab/mr-same-project.json"
        ));
        queued.merge_when_pipeline_succeeds = true;
        let native_state = queued.state.clone();
        let target = repository("group/project");
        let mut summary = summary_from_merge_request(
            &target,
            target.clone(),
            target.clone(),
            queued.clone(),
            ReviewDecision::Approved,
            Vec::new(),
            RemoteOperation::MergeChangeRequest,
        )
        .unwrap();
        summary.queue_state = QueueState::Queued;

        let outcome = merge_mutation_result(summary, &queued);

        assert_eq!(outcome.outcome, super::super::MergeMutationOutcome::Pending);
        assert_eq!(outcome.native_state, native_state);
        assert_eq!(outcome.summary.queue_state, QueueState::Queued);
    }

    #[test]
    fn unknown_post_merge_state_without_queue_confirmation_is_uncertain() {
        let accepted = merge_request(include_str!(
            "../../tests/fixtures/remote/gitlab/mr-same-project.json"
        ));
        let target = repository("group/project");
        let mut summary = summary_from_merge_request(
            &target,
            target.clone(),
            target.clone(),
            accepted.clone(),
            ReviewDecision::Approved,
            vec!["approved".to_string()],
            RemoteOperation::MergeChangeRequest,
        )
        .unwrap();
        summary.lifecycle = LifecycleState::Unknown("preparing".to_string());
        summary.queue_state = QueueState::NotQueued;
        summary.native_state_evidence.queue.clear();

        let result = merge_mutation_result(summary, &accepted);

        assert_eq!(
            result.outcome,
            super::super::MergeMutationOutcome::Uncertain
        );
    }

    #[test]
    fn merge_args_include_expected_sha_as_one_argument() {
        let id = ChangeRequestId::new(
            repository("group/project"),
            NativeChangeRequestId::new("1001").unwrap(),
            Some(7),
        );
        let request = GuardedMerge {
            id,
            target_repository: repository("group/project"),
            target_branch: "main".to_string(),
            expected_source_sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
            method: MergeMethod::Squash,
            native_guard: Some(NativeMergeGuard::new("guard").unwrap()),
        };
        let fields = merge_fields(&request).unwrap();
        let args = api_args(
            "git.example.com",
            None,
            "projects/group%2Fproject/merge_requests/7/merge",
            "PUT",
            &fields,
        );

        assert!(args.windows(2).any(|pair| {
            pair == [
                "--raw-field",
                "sha=0123456789abcdef0123456789abcdef01234567",
            ]
        }));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--raw-field", "squash=true"])
        );
    }

    #[test]
    fn api_args_select_host_and_encode_nested_project() {
        let endpoint = format!(
            "projects/{}/merge_requests",
            encode_path_segment("parent/subgroup/project")
        );
        let args = api_args("git.corp.example", None, &endpoint, "GET", &[]);

        assert_eq!(
            args,
            vec![
                "api",
                "--hostname",
                "git.corp.example",
                "projects/parent%2Fsubgroup%2Fproject/merge_requests",
                "--method",
                "GET",
            ]
        );
    }

    #[test]
    fn api_args_route_override_through_full_endpoint_and_canonical_credential_host() {
        let args = api_args(
            "git.corp.example",
            Some("https://api.corp.example/gitlab/api/v4"),
            "projects/group%2Fproject/merge_requests",
            "GET",
            &[],
        );

        assert!(
            args.contains(
                &"https://api.corp.example/gitlab/api/v4/projects/group%2Fproject/merge_requests"
                    .to_string()
            )
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--hostname", "git.corp.example"])
        );
    }

    #[test]
    fn api_args_select_the_glab_config_host_for_a_self_managed_port() {
        let host = HostIdentity::new("localhost", Some(8443)).unwrap();
        let args = api_args(
            &host.to_string(),
            None,
            "projects/group%2Fproject",
            "GET",
            &[],
        );

        assert_eq!(
            args.windows(2)
                .find(|pair| pair[0] == "--hostname")
                .unwrap(),
            ["--hostname", "localhost"]
        );
    }

    #[test]
    fn malformed_output_is_invalid_response() {
        let result = serde_json::from_str::<GitLabMergeRequest>("not-json").map_err(|error| {
            invalid_response(
                RemoteOperation::ObserveChangeRequest,
                &format!("malformed GitLab response: {error}"),
            )
        });

        assert_eq!(
            result.unwrap_err().class(),
            RemoteErrorClass::InvalidResponse
        );
    }

    #[test]
    fn adapter_keeps_configured_glab_program() {
        let adapter = GitLabAdapter::with_glab_path("/opt/tools/glab-custom", repository("g/p"));
        assert_eq!(adapter.glab_path, "/opt/tools/glab-custom");
    }

    #[test]
    fn error_classifier_distinguishes_auth_and_stale_head() {
        let auth = classify_message(RemoteOperation::ListChangeRequests, "HTTP 401 Unauthorized");
        let stale = classify_message(
            RemoteOperation::MergeChangeRequest,
            "HTTP 409: SHA does not match HEAD of source branch",
        );

        assert_eq!(auth.class(), RemoteErrorClass::Authentication);
        assert_eq!(auth.status(), Some(401));
        assert_eq!(stale.class(), RemoteErrorClass::StaleHead);
    }

    #[cfg(unix)]
    #[test]
    fn glab_failure_classifies_then_discards_all_untrusted_output() {
        let directory = std::env::temp_dir().join(format!(
            "prism-gitlab-safe-error-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let glab = directory.join("glab");
        crate::test_support::write_executable(
            &glab,
            r#"#!/bin/sh
printf '%s\n' 'https://attacker.example/collect?private_token=query-secret'
printf '%s\n' '{"message":"malicious response body"}'
printf '%s\n' 'HTTP 503 Service Unavailable' >&2
printf '%s\n' 'glpat-direct-secret' >&2
printf '%s\n' 'Authorization: Bearer bearer-header-secret' >&2
printf '%s\n' 'PRIVATE-TOKEN: glpat-private-header-secret' >&2
printf '%s\n' 'first malicious line' 'second malicious line' >&2
exit 17
"#,
        );
        let adapter = GitLabAdapter::with_glab_path(glab.to_str().unwrap(), repository("g/p"));

        let error = adapter
            .get_json::<GitLabProject>(RemoteOperation::ObserveRepositoryPolicy, "projects/g%2Fp")
            .unwrap_err();

        assert_eq!(error.class(), RemoteErrorClass::Provider);
        assert_eq!(error.retryability(), Retryability::Retryable);
        assert_eq!(error.status(), Some(503));
        assert_eq!(error.exit_code(), Some(17));
        assert_eq!(error.retry_hint(), Some(RetryHint::Backoff));
        assert_eq!(
            error.to_string(),
            "GitLab observe_repository_policy failed: provider; retry=retryable; status=503; exit=17; hint=backoff"
        );
        for untrusted in [
            "glpat-direct-secret",
            "bearer-header-secret",
            "glpat-private-header-secret",
            "query-secret",
            "Authorization",
            "PRIVATE-TOKEN",
            "https://attacker.example",
            "malicious response body",
            "first malicious line",
        ] {
            assert!(
                !error.to_string().contains(untrusted),
                "untrusted output survived: {untrusted}"
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[ignore = "requires scripts/remote-compatibility.sh gitlab"]
    fn pinned_local_gitlab_adapter_compatibility() {
        assert!(
            matches!(std::env::var("PRISM_REMOTE_COMPATIBILITY"), Ok(value) if value == "1"),
            "unsupported fixture: run this test only through scripts/remote-compatibility.sh gitlab"
        );
        let base_url = std::env::var("PRISM_GITLAB_COMPAT_URL")
            .expect("unsupported fixture: PRISM_GITLAB_COMPAT_URL is required");
        let glab = std::env::var("PRISM_GITLAB_COMPAT_GLAB")
            .expect("unsupported fixture: PRISM_GITLAB_COMPAT_GLAB is required");
        let parsed = url::Url::parse(&base_url).expect("compatibility URL");
        let host = HostIdentity::new(
            parsed.host_str().expect("compatibility host"),
            parsed.port(),
        )
        .unwrap();
        let profile = HostProfile::new(host.clone(), ProviderKind::GitLab)
            .unwrap()
            .with_http_allowed(true)
            .with_bases(
                RemoteBase::new(WebScheme::Http, host.clone(), "").unwrap(),
                RemoteBase::new(WebScheme::Http, host.clone(), "api/v4").unwrap(),
            );
        let discovery = RemoteDiscovery::new([profile]).unwrap();
        let discovered = discovery
            .discover(&format!("{base_url}/prism-target/compat-target.git"))
            .unwrap();
        let target = discovered.repository.id;
        assert_eq!(target.provider(), ProviderKind::GitLab);
        assert_eq!(target.project_path(), "prism-target/compat-target");

        let adapter = GitLabAdapter::with_glab_path(&glab, target.clone());
        let summaries = adapter.list_change_requests().unwrap();
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
            "prism-fork/compat-target"
        );
        assert_eq!(fork.change_request.target_repository, target);

        let fetched = adapter.fetch_metadata(&fork.change_request.id).unwrap();
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
            .expect("GitLab notes endpoint must be supported");
        assert!(
            comments
                .iter()
                .any(|comment| comment.body == "compat seeded note")
        );
        let threads = details
            .review_threads
            .known()
            .expect("unsupported fixture: GitLab discussions API returned no usable observation");
        let thread = threads
            .iter()
            .find(|thread| {
                thread
                    .comments
                    .iter()
                    .any(|comment| comment.body == "compat exact discussion")
            })
            .expect("unsupported fixture: GitLab did not expose the seeded exact discussion");
        assert!(
            thread.resolvable,
            "unsupported fixture: GitLab did not make the seeded discussion resolvable"
        );
        assert!(!thread.resolved);
        let checks = details
            .checks
            .known()
            .expect("GitLab commit status endpoint must return evidence");
        assert!(
            checks.iter().any(|check| {
                check.name == "compat/status" && check.state == CheckState::Passed
            })
        );

        let resolved = adapter
            .resolve_review_thread(&ResolveReviewThread {
                id: same.change_request.id.clone(),
                thread_id: thread.native_id.clone(),
                expected_head_sha: same.change_request.head_sha.clone(),
            })
            .unwrap();
        assert!(resolved.review_threads.known().is_some_and(|threads| {
            threads
                .iter()
                .any(|observed| observed.native_id == thread.native_id && observed.resolved)
        }));

        let policy = adapter.repository_policy("main").unwrap();
        assert_eq!(policy.repository, Some(target.clone()));
        assert_eq!(
            policy.facts.conversations_must_be_resolved,
            Observation::Known(true)
        );

        let create_request = |expected_head_sha: &str| CreateChangeRequest {
            source_repository: target.clone(),
            target_repository: target.clone(),
            source_branch: "adapter-create".to_string(),
            target_branch: "main".to_string(),
            expected_head_sha: expected_head_sha.to_string(),
            title: "compat adapter-created".to_string(),
            body: "created by the real GitLab adapter".to_string(),
            draft: false,
        };
        let stale = adapter
            .create_change_request(&create_request("0000000000000000000000000000000000000000"))
            .unwrap_err();
        assert_eq!(stale.class(), RemoteErrorClass::StaleHead);
        let branch: GitLabBranch = adapter
            .get_json(
                RemoteOperation::CreateChangeRequest,
                &format!(
                    "projects/{}/repository/branches/adapter-create",
                    encode_path_segment(target.project_path())
                ),
            )
            .unwrap();
        let created = adapter
            .create_change_request(&create_request(&branch.commit.id))
            .unwrap();
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
            .merge_change_request(&merge_request("0000000000000000000000000000000000000000"))
            .unwrap_err();
        assert_eq!(stale.class(), RemoteErrorClass::StaleHead);
        let merged = adapter
            .merge_change_request(&merge_request(&created.change_request.head_sha))
            .unwrap();
        assert_eq!(merged.outcome, super::super::MergeMutationOutcome::Merged);
        assert_eq!(merged.summary.lifecycle, LifecycleState::Merged);
    }
}
