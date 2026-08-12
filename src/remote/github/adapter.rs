use super::*;

/// GitHub transport and normalization behind `gh`'s configured credentials.
pub(in crate::remote) struct GitHubAdapter<'a> {
    path: &'a std::path::Path,
    config: &'a Config,
    repository: RemoteRepositoryId,
}

impl<'a> GitHubAdapter<'a> {
    pub(in crate::remote) fn new(
        path: &'a std::path::Path,
        config: &'a Config,
        repository: RemoteRepositoryId,
    ) -> Result<Self, RemoteError> {
        if repository.provider() != ProviderKind::GitHub {
            return Err(github_error(
                RemoteOperation::DiscoverRepository,
                RemoteErrorClass::Configuration,
                Retryability::NotRetryable,
                "GitHub adapter requires a GitHub repository",
            ));
        }
        Ok(Self {
            path,
            config,
            repository,
        })
    }

    pub(in crate::remote) fn capabilities(&self) -> Capabilities {
        Capabilities::for_provider(ProviderKind::GitHub)
    }

    /// Lists issue-shaped records but deliberately excludes pull requests.
    /// `--slurp` preserves page boundaries so a truncated or malformed page
    /// fails the whole observation instead of looking like an empty result.
    pub(in crate::remote) async fn discover_issues(
        &self,
        state: &str,
    ) -> Result<Vec<ProviderItemObservation>, RemoteError> {
        let operation = RemoteOperation::DiscoverIssues;
        if !matches!(state, "open" | "closed" | "all") {
            return Err(github_error(
                operation,
                RemoteErrorClass::Validation,
                Retryability::NotRetryable,
                "GitHub issue state must be open, closed, or all",
            ));
        }
        let endpoint = github_repository_api_endpoint(
            &self.repository,
            &format!("issues?state={state}&per_page=100"),
        )
        .map_err(|error| github_invalid_response(operation, error))?;
        let endpoint = github_api_endpoint(self.config, self.repository.host(), &endpoint);
        let command = crate::process::Command::new(self.config.tool("gh"))
            .arg("api")
            .arg(endpoint)
            .arg("--hostname")
            .arg(self.repository.host().to_string())
            .arg("--paginate")
            .arg("--slurp")
            .args(["-H", "Accept: application/vnd.github+json"])
            .current_dir(self.path);
        let output = crate::process::run_output_allow_failure_named(
            command,
            crate::process::ProcessPolicy::NetworkQuery,
            crate::process::ProcessDescriptor::new("gh.api.issue.list"),
        )
        .await
        .map_err(|error| github_provider_error(operation, error))?;
        if !output.status.success() {
            return Err(github_provider_error(
                operation,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )
            .with_exit_code(output.status.code().unwrap_or(-1)));
        }
        if output.stdout_truncated {
            return Err(github_invalid_response(
                operation,
                "GitHub issue discovery response was truncated".to_string(),
            ));
        }
        let pages: Vec<Vec<GithubIssue>> =
            serde_json::from_slice(&output.stdout).map_err(|error| {
                github_invalid_response(operation, format!("parse GitHub issues: {error}"))
            })?;
        pages
            .into_iter()
            .flatten()
            .filter(|issue| issue.pull_request.is_none())
            .map(|issue| issue.normalize(self.repository.clone(), operation))
            .collect()
    }

    pub(in crate::remote) async fn observe_issue(
        &self,
        native_id: &str,
    ) -> Result<ProviderItemObservation, RemoteError> {
        let operation = RemoteOperation::DiscoverIssues;
        let number = native_id.parse::<u64>().map_err(|_| {
            github_error(
                operation,
                RemoteErrorClass::Validation,
                Retryability::NotRetryable,
                "GitHub Issue identity must be numeric",
            )
        })?;
        let endpoint =
            github_repository_api_endpoint(&self.repository, &format!("issues/{number}"))
                .map_err(|error| github_invalid_response(operation, error))?;
        let issue: GithubIssue = self
            .issue_api_json(operation, &endpoint, "GET", &[])
            .await?;
        if issue.pull_request.is_some() {
            return Err(github_error(
                operation,
                RemoteErrorClass::Validation,
                Retryability::NotRetryable,
                "provider item is a pull request, not an Issue",
            ));
        }
        issue.normalize(self.repository.clone(), operation)
    }

    pub(in crate::remote) async fn set_issue_labels(
        &self,
        native_id: &str,
        labels: &[String],
    ) -> Result<ProviderItemObservation, RemoteError> {
        let before = self.observe_issue(native_id).await?;
        let operation = RemoteOperation::MutateLabels;
        let endpoint =
            github_repository_api_endpoint(&self.repository, &format!("issues/{native_id}"))
                .map_err(|error| github_invalid_response(operation, error))?;
        let fields = labels
            .iter()
            .map(|label| format!("labels[]={label}"))
            .collect::<Vec<_>>();
        let _: serde_json::Value = self
            .issue_api_json(operation, &endpoint, "PATCH", &fields)
            .await?;
        let after = self.observe_issue(native_id).await?;
        if after.id != before.id {
            return Err(github_invalid_response(
                operation,
                "GitHub Issue identity changed during label mutation".into(),
            ));
        }
        Ok(after)
    }

    pub(in crate::remote) async fn set_issue_assignees(
        &self,
        native_id: &str,
        assignees: &[String],
    ) -> Result<ProviderItemObservation, RemoteError> {
        let operation = RemoteOperation::MutateAssignment;
        let endpoint =
            github_repository_api_endpoint(&self.repository, &format!("issues/{native_id}"))
                .map_err(|error| github_invalid_response(operation, error))?;
        let fields = assignees
            .iter()
            .map(|value| format!("assignees[]={value}"))
            .collect::<Vec<_>>();
        let _: serde_json::Value = self
            .issue_api_json(operation, &endpoint, "PATCH", &fields)
            .await?;
        self.observe_issue(native_id).await
    }

    pub(in crate::remote) async fn set_issue_lifecycle(
        &self,
        native_id: &str,
        lifecycle: &str,
    ) -> Result<ProviderItemObservation, RemoteError> {
        if !matches!(lifecycle, "open" | "closed") {
            return Err(github_error(
                RemoteOperation::MutateIssueLifecycle,
                RemoteErrorClass::Validation,
                Retryability::NotRetryable,
                "GitHub Issue lifecycle must be open or closed",
            ));
        }
        let operation = RemoteOperation::MutateIssueLifecycle;
        let endpoint =
            github_repository_api_endpoint(&self.repository, &format!("issues/{native_id}"))
                .map_err(|error| github_invalid_response(operation, error))?;
        let _: serde_json::Value = self
            .issue_api_json(
                operation,
                &endpoint,
                "PATCH",
                &[format!("state={lifecycle}")],
            )
            .await?;
        self.observe_issue(native_id).await
    }

    pub(in crate::remote) async fn issue_has_comment_marker(
        &self,
        native_id: &str,
        marker: &str,
    ) -> Result<bool, RemoteError> {
        let operation = RemoteOperation::CreateIssueComment;
        let endpoint = github_repository_api_endpoint(
            &self.repository,
            &format!("issues/{native_id}/comments?per_page=100"),
        )
        .map_err(|error| github_invalid_response(operation, error))?;
        let comments: Vec<GithubIssueComment> = self
            .issue_api_json(operation, &endpoint, "GET", &[])
            .await?;
        Ok(comments.iter().any(|comment| comment.body.contains(marker)))
    }

    pub(in crate::remote) async fn create_issue_comment(
        &self,
        native_id: &str,
        body: &str,
        marker: &str,
    ) -> Result<(), RemoteError> {
        let operation = RemoteOperation::CreateIssueComment;
        let endpoint = github_repository_api_endpoint(
            &self.repository,
            &format!("issues/{native_id}/comments"),
        )
        .map_err(|error| github_invalid_response(operation, error))?;
        let value = format!("body={}\n\n<!-- prism:{marker} -->", body.trim());
        let _: serde_json::Value = self
            .issue_api_json(operation, &endpoint, "POST", &[value])
            .await?;
        Ok(())
    }

    async fn issue_api_json<T: serde::de::DeserializeOwned>(
        &self,
        operation: RemoteOperation,
        endpoint: &str,
        method: &str,
        fields: &[String],
    ) -> Result<T, RemoteError> {
        let endpoint = github_api_endpoint(self.config, self.repository.host(), endpoint);
        let mut command = crate::process::Command::new(self.config.tool("gh"))
            .arg("api")
            .arg(endpoint)
            .arg("--hostname")
            .arg(self.repository.host().to_string())
            .args(["--method", method])
            .args(["-H", "Accept: application/vnd.github+json"])
            .current_dir(self.path);
        for field in fields {
            command = command.arg("-f").arg(field);
        }
        let output = crate::process::run_output_allow_failure_named(
            command,
            crate::process::ProcessPolicy::NetworkQuery,
            crate::process::ProcessDescriptor::new("gh.api.issue"),
        )
        .await
        .map_err(|error| github_provider_error(operation, error))?;
        if !output.status.success() {
            return Err(github_provider_error(
                operation,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )
            .with_exit_code(output.status.code().unwrap_or(-1)));
        }
        if output.stdout_truncated {
            return Err(github_invalid_response(
                operation,
                "GitHub Issue response was truncated".into(),
            ));
        }
        serde_json::from_slice(&output.stdout).map_err(|error| {
            github_invalid_response(operation, format!("parse GitHub Issue response: {error}"))
        })
    }

    pub(in crate::remote) async fn list_change_requests(
        &self,
        head_ref: Option<&str>,
    ) -> Result<Vec<ChangeRequestSummary>, RemoteError> {
        let summaries = match head_ref {
            Some(head_ref) => {
                fetch_open_pr_summaries_for_repository_head(
                    self.path,
                    self.config,
                    &self.repository,
                    head_ref,
                )
                .await
            }
            None => {
                fetch_pr_summary_index_for_repository(self.path, self.config, &self.repository)
                    .await
            }
        }
        .map_err(|error| github_provider_error(RemoteOperation::ListChangeRequests, error))?;
        summaries
            .into_iter()
            .map(|summary| {
                normalize_summary(
                    summary,
                    &self.repository,
                    RemoteOperation::ListChangeRequests,
                )
            })
            .collect()
    }

    pub(in crate::remote) async fn observe_change_request(
        &self,
        id: &ChangeRequestId,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        self.lookup_change_request_for(id, RemoteOperation::ObserveChangeRequest)
            .await?
            .ok_or_else(|| github_not_found(RemoteOperation::ObserveChangeRequest))
    }

    pub(in crate::remote) async fn lookup_change_request(
        &self,
        id: &ChangeRequestId,
    ) -> Result<Option<ChangeRequestSummary>, RemoteError> {
        self.lookup_change_request_for(id, RemoteOperation::ObserveChangeRequest)
            .await
    }

    async fn observe_change_request_for(
        &self,
        id: &ChangeRequestId,
        operation: RemoteOperation,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        self.lookup_change_request_for(id, operation)
            .await?
            .ok_or_else(|| github_not_found(operation))
    }

    async fn lookup_change_request_for(
        &self,
        id: &ChangeRequestId,
        operation: RemoteOperation,
    ) -> Result<Option<ChangeRequestSummary>, RemoteError> {
        self.validate_change_request_id(id, operation)?;
        let number = github_number(id, operation)?;
        let Some(summary) = fetch_pr_summary_for_repository_number(
            self.path,
            self.config,
            &self.repository,
            number,
        )
        .await
        .map_err(|error| github_provider_error(operation, error))?
        else {
            return Ok(None);
        };
        let summary = normalize_summary(summary, &self.repository, operation)?;
        if summary.change_request.id != *id {
            return Err(github_error(
                operation,
                RemoteErrorClass::InvalidResponse,
                Retryability::NotRetryable,
                "GitHub returned a different change request identity",
            ));
        }
        Ok(Some(summary))
    }

    pub(in crate::remote) async fn change_request_details(
        &self,
        change_request: &ChangeRequest,
    ) -> Result<ChangeRequestDetails, RemoteError> {
        self.change_request_details_for(change_request, RemoteOperation::ObserveChangeRequest)
            .await
    }

    async fn change_request_details_for(
        &self,
        change_request: &ChangeRequest,
        operation: RemoteOperation,
    ) -> Result<ChangeRequestDetails, RemoteError> {
        let observed = self
            .observe_change_request_for(&change_request.id, operation)
            .await?;
        ensure_association(
            &observed.change_request,
            change_request,
            operation,
            "pull request association changed before details were loaded",
        )?;
        let number = github_number(&change_request.id, operation)?;
        let details = fetch_pr_details_for_repository_number(
            self.path,
            self.config,
            &self.repository,
            number,
            &change_request.source_branch,
            &change_request.head_sha,
        )
        .await
        .map_err(|error| github_provider_error(operation, error))?;
        let after = self
            .observe_change_request_for(&change_request.id, operation)
            .await?;
        ensure_association(
            &after.change_request,
            change_request,
            operation,
            "pull request association changed while details were loaded",
        )?;
        Ok(normalize_details(change_request, details, operation))
    }

    pub(in crate::remote) async fn repository_policy(
        &self,
        target_branch: &str,
    ) -> Result<RepositoryPolicy, RemoteError> {
        let policy = fetch_repo_policy(self.path, self.config, &self.repository, target_branch)
            .await
            .map_err(|error| {
                github_provider_error(RemoteOperation::ObserveRepositoryPolicy, error)
            })?;
        let required_approvals = u32::try_from(policy.required_approvals).map_err(|_| {
            github_error(
                RemoteOperation::ObserveRepositoryPolicy,
                RemoteErrorClass::InvalidResponse,
                Retryability::NotRetryable,
                "GitHub required approval count exceeds the normalized model",
            )
        })?;
        Ok(RepositoryPolicy {
            repository: Some(self.repository.clone()),
            target_branch: target_branch.to_string(),
            facts: PolicyFacts {
                required_checks: Observation::Known(policy.required_checks),
                required_approvals: Observation::Known(required_approvals),
                conversations_must_be_resolved: Observation::Known(
                    policy.require_conversation_resolution,
                ),
                source_must_be_up_to_date: Observation::Known(policy.require_branch_up_to_date),
                queue_required: Observation::Known(policy.merge_queue_required),
            },
        })
    }

    pub(in crate::remote) async fn create_change_request(
        &self,
        request: &CreateChangeRequest,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        self.validate_repository(
            &request.target_repository,
            RemoteOperation::CreateChangeRequest,
        )?;
        if request.source_repository.provider() != ProviderKind::GitHub
            || request.source_repository.host() != self.repository.host()
        {
            return Err(github_error(
                RemoteOperation::CreateChangeRequest,
                RemoteErrorClass::Validation,
                Retryability::NotRetryable,
                "source repository must be on the adapter's GitHub host",
            ));
        }
        let source_head = if request.source_repository == request.target_repository {
            request.source_branch.clone()
        } else {
            let owner = request
                .source_repository
                .project_path()
                .split('/')
                .next()
                .ok_or_else(|| {
                    github_error(
                        RemoteOperation::CreateChangeRequest,
                        RemoteErrorClass::Validation,
                        Retryability::NotRetryable,
                        "GitHub source repository has no owner",
                    )
                })?;
            format!("{owner}:{}", request.source_branch)
        };
        run_create_pull_request(
            self.config,
            self.path,
            &request.body,
            Some(request.target_repository.project_path()),
            Some(&request.target_branch),
            Some(&source_head),
        )
        .await
        .map_err(|error| github_provider_error(RemoteOperation::CreateChangeRequest, error))?;
        let summaries = fetch_open_pr_summaries_for_repository_head(
            self.path,
            self.config,
            &self.repository,
            &request.source_branch,
        )
        .await
        .map_err(|error| github_provider_error(RemoteOperation::CreateChangeRequest, error))?;
        let summary = summaries
            .into_iter()
            .map(|summary| {
                normalize_summary(
                    summary,
                    &self.repository,
                    RemoteOperation::CreateChangeRequest,
                )
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find(|summary| {
                summary.change_request.source_repository == request.source_repository
                    && summary.change_request.target_repository == request.target_repository
                    && summary.change_request.source_branch == request.source_branch
                    && summary.change_request.target_branch == request.target_branch
                    && summary.change_request.head_sha == request.expected_head_sha
            })
            .ok_or_else(|| {
                github_error(
                    RemoteOperation::CreateChangeRequest,
                    RemoteErrorClass::InvalidResponse,
                    Retryability::Unknown,
                    "created pull request was not returned by GitHub",
                )
            })?;
        Ok(summary)
    }

    pub(in crate::remote) async fn merge_change_request(
        &self,
        request: &GuardedMerge,
    ) -> Result<MergeMutationResult, RemoteError> {
        self.validate_change_request_id(&request.id, RemoteOperation::MergeChangeRequest)?;
        self.validate_repository(
            &request.target_repository,
            RemoteOperation::MergeChangeRequest,
        )?;
        let number = github_number(&request.id, RemoteOperation::MergeChangeRequest)?;
        let before = self
            .observe_change_request_for(&request.id, RemoteOperation::MergeChangeRequest)
            .await?;
        request.validate_observation(&before).map_err(|error| {
            github_error(
                RemoteOperation::MergeChangeRequest,
                RemoteErrorClass::StaleHead,
                Retryability::NotRetryable,
                error.to_string(),
            )
        })?;
        let target_project = (before.change_request.source_repository != self.repository)
            .then_some(self.repository.project_path());
        merge_pull_request(
            self.config,
            self.path,
            number,
            &request.expected_source_sha,
            target_project,
        )
        .await
        .map_err(|error| github_provider_error(RemoteOperation::MergeChangeRequest, error))?;
        let summary = self
            .observe_change_request_for(&request.id, RemoteOperation::MergeChangeRequest)
            .await?;
        if summary.change_request.head_sha != request.expected_source_sha {
            return Err(github_error(
                RemoteOperation::MergeChangeRequest,
                RemoteErrorClass::StaleHead,
                Retryability::NotRetryable,
                "change request head changed during merge",
            ));
        }
        let native_state = summary
            .native_state_evidence
            .lifecycle
            .first()
            .cloned()
            .unwrap_or_else(|| lifecycle_label(&summary.lifecycle).to_string());
        Ok(MergeMutationResult::from_summary(summary, native_state))
    }

    pub(in crate::remote) async fn submit_review(
        &self,
        request: &SubmitReview,
    ) -> Result<(), RemoteError> {
        let operation = RemoteOperation::SubmitReview;
        self.validate_change_request_id(&request.id, operation)?;
        let number = github_number(&request.id, operation)?;
        let observed = self
            .observe_change_request_for(&request.id, operation)
            .await?;

        // Keep the authorization checks adjacent to the mutating command.
        self.validate_repository(&observed.change_request.target_repository, operation)?;
        if observed.change_request.head_sha != request.expected_head_sha {
            return Err(github_stale_head(
                operation,
                "pull request head changed since review submission was authorized",
            ));
        }

        let event = match request.kind {
            ReviewSubmissionKind::Approve => "APPROVE",
            ReviewSubmissionKind::Comment => "COMMENT",
            ReviewSubmissionKind::RequestChanges => "REQUEST_CHANGES",
        };
        let endpoint =
            github_repository_api_endpoint(&self.repository, &format!("pulls/{number}/reviews"))
                .map_err(|error| github_invalid_response(operation, error))?;
        let endpoint = github_api_endpoint(self.config, self.repository.host(), &endpoint);
        let command = crate::process::Command::new(self.config.tool("gh"))
            .arg("api")
            .arg(endpoint)
            .arg("--hostname")
            .arg(self.repository.host().to_string())
            .args(["--method", "POST"])
            .args(["-H", "Accept: application/vnd.github+json"])
            .arg("-f")
            .arg(format!("commit_id={}", request.expected_head_sha))
            .arg("-f")
            .arg(format!("event={event}"))
            .arg("-f")
            .arg(format!("body={}", request.body.trim()))
            .current_dir(self.path);
        let output = crate::process::run_output_allow_failure_named(
            command,
            crate::process::ProcessPolicy::NetworkQuery,
            crate::process::ProcessDescriptor::new("gh.api.pull-request-review.create"),
        )
        .await
        .map_err(|error| github_provider_error(operation, error))?;
        if output.status.success() {
            if output.stdout_truncated {
                return Err(github_invalid_response(
                    operation,
                    "GitHub create pull request review response was truncated".to_string(),
                ));
            }
            let response =
                serde_json::from_slice::<GithubCreatedReview>(&output.stdout).map_err(|error| {
                    github_invalid_response(
                        operation,
                        format!("parse GitHub create pull request review response: {error}"),
                    )
                })?;
            if response.commit_id != request.expected_head_sha {
                return Err(github_stale_head(
                    operation,
                    "GitHub created the review for a different pull request head",
                ));
            }
            return Ok(());
        }
        let message = if output.stderr.iter().all(u8::is_ascii_whitespace) {
            format!(
                "gh api create pull request review exited with {}",
                output.status
            )
        } else {
            String::from_utf8_lossy(&output.stderr).trim().to_string()
        };
        let error = github_provider_error(operation, message);
        Err(match output.status.code() {
            Some(exit_code) => error.with_exit_code(exit_code),
            None => error,
        })
    }

    pub(in crate::remote) async fn resolve_review_thread(
        &self,
        request: &ResolveReviewThread,
    ) -> Result<(), RemoteError> {
        let operation = RemoteOperation::ResolveReviewThread;
        self.validate_change_request_id(&request.id, operation)?;
        let observed = self
            .observe_change_request_for(&request.id, operation)
            .await?;
        ensure_expected_head(
            &observed.change_request.head_sha,
            &request.expected_head_sha,
        )?;
        let details = self
            .change_request_details_for(&observed.change_request, operation)
            .await?;
        if !details
            .association
            .as_ref()
            .is_some_and(|association| association.matches(&request.id, &request.expected_head_sha))
        {
            return Err(github_stale_head(
                operation,
                "pull request association changed while review threads were loaded",
            ));
        }
        let threads = authoritative_review_threads(details.review_threads, operation)?;
        let thread = threads
            .iter()
            .find(|thread| thread.native_id == request.thread_id)
            .ok_or_else(|| {
                github_error(
                    operation,
                    RemoteErrorClass::Validation,
                    Retryability::NotRetryable,
                    "review thread does not belong to the observed pull request",
                )
            })?;
        if !thread.resolvable || thread.resolved {
            return Err(github_error(
                operation,
                RemoteErrorClass::Validation,
                Retryability::NotRetryable,
                "review thread is not unresolved and resolvable",
            ));
        }
        let immediately_before = self
            .observe_change_request_for(&request.id, operation)
            .await?;
        ensure_association(
            &immediately_before.change_request,
            &observed.change_request,
            operation,
            "pull request association changed before review thread resolution",
        )?;
        ensure_expected_head(
            &immediately_before.change_request.head_sha,
            &request.expected_head_sha,
        )?;
        resolve_review_thread(
            self.path,
            self.config,
            self.repository.host(),
            request.thread_id.as_str(),
        )
        .await
        .map_err(|error| github_provider_error(operation, error))?;
        // The mutation response authoritatively confirms resolution. A successful
        // post-observation can still expose a concurrent head or identity change,
        // but an unavailable refresh must not report the completed mutation as failed.
        if let Ok(after) = self
            .observe_change_request_for(&request.id, operation)
            .await
        {
            ensure_association(
                &after.change_request,
                &observed.change_request,
                operation,
                "pull request association changed during review thread resolution",
            )?;
            ensure_expected_head(&after.change_request.head_sha, &request.expected_head_sha)?;
        }
        Ok(())
    }

    fn validate_change_request_id(
        &self,
        id: &ChangeRequestId,
        operation: RemoteOperation,
    ) -> Result<(), RemoteError> {
        self.validate_repository(id.repository(), operation)
    }

    fn validate_repository(
        &self,
        repository: &RemoteRepositoryId,
        operation: RemoteOperation,
    ) -> Result<(), RemoteError> {
        if repository != &self.repository {
            return Err(github_error(
                operation,
                RemoteErrorClass::Validation,
                Retryability::NotRetryable,
                "repository does not belong to this GitHub adapter",
            ));
        }
        Ok(())
    }
}

#[derive(serde::Deserialize)]
struct GithubCreatedReview {
    commit_id: String,
}

#[derive(serde::Deserialize)]
struct GithubIssueComment {
    body: String,
}

#[derive(serde::Deserialize)]
struct GithubIssue {
    number: u64,
    title: String,
    body: Option<String>,
    state: String,
    user: Option<GithubLogin>,
    author_association: Option<String>,
    #[serde(default)]
    labels: Vec<GithubLabel>,
    #[serde(default)]
    assignees: Vec<GithubLogin>,
    updated_at: Option<String>,
    pull_request: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct GithubLogin {
    login: String,
}

#[derive(serde::Deserialize)]
struct GithubLabel {
    id: serde_json::Value,
    name: String,
}

impl GithubIssue {
    fn normalize(
        self,
        repository: RemoteRepositoryId,
        operation: RemoteOperation,
    ) -> Result<ProviderItemObservation, RemoteError> {
        let id = ProviderItemId::new(repository, self.number.to_string(), ProviderItemKind::Issue)
            .map_err(|error| github_invalid_response(operation, error.to_string()))?;
        let labels = self
            .labels
            .into_iter()
            .map(|label| {
                let id = match label.id {
                    serde_json::Value::String(value) => value,
                    serde_json::Value::Number(value) => value.to_string(),
                    _ => String::new(),
                };
                if id.is_empty() {
                    Err(github_invalid_response(
                        operation,
                        "GitHub issue label has no authenticated identity".to_string(),
                    ))
                } else {
                    Ok((id, label.name))
                }
            })
            .collect::<Result<_, _>>()?;
        Ok(ProviderItemObservation {
            id,
            title: self.title,
            body: self.body.unwrap_or_default(),
            lifecycle: self.state,
            author: self
                .user
                .map_or_else(|| "ghost".to_string(), |user| user.login),
            author_relationship: self.author_association,
            labels,
            assignees: self.assignees.into_iter().map(|user| user.login).collect(),
            updated_at: self.updated_at,
        })
    }
}

pub(super) fn normalize_summary(
    summary: PrSummary,
    _expected_repository: &RemoteRepositoryId,
    operation: RemoteOperation,
) -> Result<ChangeRequestSummary, RemoteError> {
    let identity = summary.change_request_identity.as_ref().ok_or_else(|| {
        github_invalid_response(operation, "missing canonical identity".to_string())
    })?;
    let change_request = ChangeRequest {
        id: identity
            .change_request_id(Some(summary.number))
            .map_err(|error| github_invalid_response(operation, error.to_string()))?,
        source_repository: identity
            .source_repository()
            .map_err(|error| github_invalid_response(operation, error.to_string()))?,
        target_repository: identity
            .target_repository()
            .map_err(|error| github_invalid_response(operation, error.to_string()))?,
        source_branch: summary.head_ref,
        target_branch: summary.base_ref,
        head_sha: summary.head_sha,
    };
    let lifecycle = if summary.merged {
        LifecycleState::Merged
    } else {
        LifecycleState::from_native(summary.state.clone())
    };
    let native_mergeability = MergeabilityState::from_native(
        summary
            .native_state_evidence
            .mergeability
            .first()
            .cloned()
            .unwrap_or_else(|| summary.merge_state_status.clone()),
    );
    let merge_state = MergeabilityState::from_native(summary.merge_state_status.clone());
    let mergeability = match (&native_mergeability, &merge_state) {
        (MergeabilityState::Conflicting, _) | (_, MergeabilityState::Conflicting) => {
            MergeabilityState::Conflicting
        }
        (_, MergeabilityState::Behind) => MergeabilityState::Behind,
        _ => native_mergeability,
    };
    Ok(ChangeRequestSummary {
        change_request,
        title: summary.title,
        author: summary.author,
        body: summary.body,
        web_url: (!summary.url.trim().is_empty()).then_some(summary.url),
        lifecycle,
        review_decision: ReviewDecision::from_native(summary.review_decision),
        requested_reviewers: summary.requested_reviewers,
        mergeability,
        check_state: CheckState::from_native(summary.check_status),
        queue_state: QueueState::from_native(summary.queue_state),
        native_state_evidence: summary.native_state_evidence,
        comment_count: summary.comment_count,
        draft: summary.draft,
        updated_at: (!summary.updated_at.trim().is_empty()).then_some(summary.updated_at),
    })
}

fn normalize_details(
    change_request: &ChangeRequest,
    details: ProviderDetailsObservation,
    operation: RemoteOperation,
) -> ChangeRequestDetails {
    let mut review_threads = Vec::<ReviewThread>::new();
    let review_threads = details.review_comments.and_then(|comments| {
        for comment in comments {
            let thread_id = NativeReviewThreadId::new(if comment.thread_id.is_empty() {
                comment.id.clone()
            } else {
                comment.thread_id.clone()
            })
            .map_err(|error| format!("invalid GitHub review thread ID: {error}"))?;
            let normalized = Comment {
                native_id: comment.id,
                author: comment.author,
                body: comment.body,
                created_at: (!comment.created_at.is_empty()).then_some(comment.created_at),
                path: (!comment.path.is_empty()).then_some(comment.path),
                line: comment.line.parse().ok(),
            };
            if let Some(thread) = review_threads
                .iter_mut()
                .find(|thread| thread.native_id == thread_id)
            {
                thread.comments.push(normalized);
            } else {
                review_threads.push(ReviewThread {
                    native_id: thread_id,
                    resolvable: !comment.thread_id.is_empty(),
                    resolved: comment.resolved,
                    comments: vec![normalized],
                });
            }
        }
        Ok(review_threads)
    });
    let partial_error = (!details.partial_errors.is_empty())
        .then(|| github_provider_error(operation, details.partial_errors.join("; ")));
    ChangeRequestDetails {
        association: Some(change_request.head_association()),
        comments: normalized_observation(
            details.comments.map(|comments| {
                comments
                    .into_iter()
                    .map(|comment| Comment {
                        native_id: comment.id,
                        author: comment.author,
                        body: comment.body,
                        created_at: (!comment.created_at.is_empty()).then_some(comment.created_at),
                        path: None,
                        line: None,
                    })
                    .collect()
            }),
            partial_error.clone(),
            operation,
        ),
        reviews: normalized_observation(
            details.reviews.map(|reviews| {
                reviews
                    .into_iter()
                    .map(|review| Review {
                        native_id: review.id,
                        author: review.author,
                        decision: ReviewDecision::from_native(review.state),
                        body: review.body,
                        submitted_at: (!review.submitted_at.is_empty())
                            .then_some(review.submitted_at),
                    })
                    .collect()
            }),
            None,
            operation,
        ),
        review_threads: normalized_observation(review_threads, None, operation),
        changed_files: normalized_observation(details.files, None, operation),
        checks: normalized_observation(
            details.check_contexts.map(|checks| {
                checks
                    .into_iter()
                    .map(|check| CheckContext {
                        name: check.name,
                        state: match check.state {
                            PrCheckState::Pending => CheckState::Pending,
                            PrCheckState::Success => CheckState::Passed,
                            PrCheckState::Failed => CheckState::Failed,
                            PrCheckState::Mixed => CheckState::Mixed,
                            PrCheckState::Unknown => CheckState::Unknown("unknown".to_string()),
                        },
                        native_state: check.state.label().to_string(),
                        web_url: None,
                    })
                    .collect()
            }),
            None,
            operation,
        ),
        ci_failures: normalized_observation(
            details.ci_failures.map(|failures| {
                failures
                    .into_iter()
                    .map(|failure| crate::remote::CiFailure {
                        pipeline: failure.workflow,
                        job: failure.name,
                        native_conclusion: failure.conclusion,
                        web_url: (!failure.url.is_empty()).then_some(failure.url),
                        native_run_id: failure.run_id,
                        log_tail: failure.log_tail,
                    })
                    .collect()
            }),
            None,
            operation,
        ),
    }
}

fn normalized_observation<T>(
    result: Result<T, String>,
    partial_error: Option<RemoteError>,
    operation: RemoteOperation,
) -> Observation<T> {
    match result {
        Ok(value) => match partial_error {
            Some(error) => Observation::Stale {
                value,
                error: Some(error),
            },
            None => Observation::Known(value),
        },
        Err(error) => Observation::Failed(github_provider_error(operation, error)),
    }
}

fn authoritative_review_threads(
    observation: Observation<Vec<ReviewThread>>,
    operation: RemoteOperation,
) -> Result<Vec<ReviewThread>, RemoteError> {
    match observation {
        Observation::Known(threads) => Ok(threads),
        Observation::EmptyKnown | Observation::AuthoritativelyAbsent => Ok(Vec::new()),
        Observation::Stale {
            error: Some(error), ..
        }
        | Observation::Failed(error) => Err(reclassify_error(error, operation)),
        Observation::Stale { error: None, .. }
        | Observation::NotLoaded
        | Observation::Unsupported
        | Observation::Unconfigured
        | Observation::Unauthorized => Err(github_error(
            operation,
            RemoteErrorClass::InvalidResponse,
            Retryability::NotRetryable,
            "GitHub review threads were not authoritatively observed",
        )),
    }
}

fn ensure_association(
    observed: &ChangeRequest,
    expected: &ChangeRequest,
    operation: RemoteOperation,
    message: &str,
) -> Result<(), RemoteError> {
    if observed == expected {
        Ok(())
    } else {
        Err(github_stale_head(operation, message))
    }
}

fn ensure_expected_head(observed: &str, expected: &str) -> Result<(), RemoteError> {
    if observed == expected {
        Ok(())
    } else {
        Err(github_stale_head(
            RemoteOperation::ResolveReviewThread,
            "pull request head changed since review thread resolution was authorized",
        ))
    }
}

fn reclassify_error(error: RemoteError, operation: RemoteOperation) -> RemoteError {
    RemoteError::classified(
        ProviderKind::GitHub,
        operation,
        error.class(),
        error.retryability(),
        error.status(),
        error.exit_code(),
        error.retry_hint(),
    )
}

fn github_not_found(operation: RemoteOperation) -> RemoteError {
    github_error(
        operation,
        RemoteErrorClass::NotFound,
        Retryability::NotRetryable,
        "GitHub did not return the requested pull request",
    )
}

fn github_stale_head(operation: RemoteOperation, message: &str) -> RemoteError {
    github_error(
        operation,
        RemoteErrorClass::StaleHead,
        Retryability::NotRetryable,
        message,
    )
    .with_retry_hint(RetryHint::RefreshObservation)
}

fn github_number(id: &ChangeRequestId, operation: RemoteOperation) -> Result<u64, RemoteError> {
    id.display_number().ok_or_else(|| {
        github_error(
            operation,
            RemoteErrorClass::Validation,
            Retryability::NotRetryable,
            "GitHub change request has no display number",
        )
    })
}

fn github_invalid_response(operation: RemoteOperation, message: String) -> RemoteError {
    github_error(
        operation,
        RemoteErrorClass::InvalidResponse,
        Retryability::NotRetryable,
        message,
    )
}

fn github_provider_error(operation: RemoteOperation, message: String) -> RemoteError {
    if crate::process::is_cancellation_error(&message) {
        github_error(
            operation,
            RemoteErrorClass::Cancelled,
            Retryability::NotRetryable,
            message,
        )
    } else {
        github_error(
            operation,
            RemoteErrorClass::Provider,
            Retryability::Unknown,
            message,
        )
    }
}

fn github_error(
    operation: RemoteOperation,
    class: RemoteErrorClass,
    retryability: Retryability,
    message: impl AsRef<str>,
) -> RemoteError {
    RemoteError::new(
        ProviderKind::GitHub,
        operation,
        class,
        retryability,
        message,
    )
}

fn lifecycle_label(state: &LifecycleState) -> &str {
    match state {
        LifecycleState::Open => "OPEN",
        LifecycleState::Closed => "CLOSED",
        LifecycleState::Merged => "MERGED",
        LifecycleState::Unknown(native) => native,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_cancellation_stays_cancelled_and_non_retryable() {
        let error = github_provider_error(
            RemoteOperation::ObserveChangeRequest,
            "gh api: subprocess canceled".into(),
        );

        assert_eq!(error.class(), RemoteErrorClass::Cancelled);
        assert_eq!(error.retryability(), Retryability::NotRetryable);
    }
}
