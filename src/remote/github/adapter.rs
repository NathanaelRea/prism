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

    pub(in crate::remote) fn list_change_requests(
        &self,
        head_ref: Option<&str>,
    ) -> Result<Vec<ChangeRequestSummary>, RemoteError> {
        let summaries = match head_ref {
            Some(head_ref) => fetch_open_pr_summaries_for_repository_head(
                self.path,
                self.config,
                &self.repository,
                head_ref,
            ),
            None => fetch_pr_summary_index_for_repository(self.path, self.config, &self.repository),
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

    pub(in crate::remote) fn observe_change_request(
        &self,
        id: &ChangeRequestId,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        self.lookup_change_request_for(id, RemoteOperation::ObserveChangeRequest)?
            .ok_or_else(|| github_not_found(RemoteOperation::ObserveChangeRequest))
    }

    pub(in crate::remote) fn lookup_change_request(
        &self,
        id: &ChangeRequestId,
    ) -> Result<Option<ChangeRequestSummary>, RemoteError> {
        self.lookup_change_request_for(id, RemoteOperation::ObserveChangeRequest)
    }

    fn observe_change_request_for(
        &self,
        id: &ChangeRequestId,
        operation: RemoteOperation,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        self.lookup_change_request_for(id, operation)?
            .ok_or_else(|| github_not_found(operation))
    }

    fn lookup_change_request_for(
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

    pub(in crate::remote) fn change_request_details(
        &self,
        change_request: &ChangeRequest,
    ) -> Result<ChangeRequestDetails, RemoteError> {
        self.change_request_details_for(change_request, RemoteOperation::ObserveChangeRequest)
    }

    fn change_request_details_for(
        &self,
        change_request: &ChangeRequest,
        operation: RemoteOperation,
    ) -> Result<ChangeRequestDetails, RemoteError> {
        let observed = self.observe_change_request_for(&change_request.id, operation)?;
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
        .map_err(|error| github_provider_error(operation, error))?;
        let after = self.observe_change_request_for(&change_request.id, operation)?;
        ensure_association(
            &after.change_request,
            change_request,
            operation,
            "pull request association changed while details were loaded",
        )?;
        Ok(normalize_details(change_request, details, operation))
    }

    pub(in crate::remote) fn repository_policy(
        &self,
        target_branch: &str,
    ) -> Result<RepositoryPolicy, RemoteError> {
        let policy = fetch_repo_policy(self.path, self.config, &self.repository, target_branch)
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

    pub(in crate::remote) fn create_change_request(
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
        .map_err(|error| github_provider_error(RemoteOperation::CreateChangeRequest, error))?;
        let summaries = fetch_open_pr_summaries_for_repository_head(
            self.path,
            self.config,
            &self.repository,
            &request.source_branch,
        )
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

    pub(in crate::remote) fn merge_change_request(
        &self,
        request: &GuardedMerge,
    ) -> Result<MergeMutationResult, RemoteError> {
        self.validate_change_request_id(&request.id, RemoteOperation::MergeChangeRequest)?;
        self.validate_repository(
            &request.target_repository,
            RemoteOperation::MergeChangeRequest,
        )?;
        let number = github_number(&request.id, RemoteOperation::MergeChangeRequest)?;
        let before =
            self.observe_change_request_for(&request.id, RemoteOperation::MergeChangeRequest)?;
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
        .map_err(|error| github_provider_error(RemoteOperation::MergeChangeRequest, error))?;
        let summary =
            self.observe_change_request_for(&request.id, RemoteOperation::MergeChangeRequest)?;
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

    pub(in crate::remote) fn submit_review(
        &self,
        request: &SubmitReview,
    ) -> Result<(), RemoteError> {
        let operation = RemoteOperation::SubmitReview;
        self.validate_change_request_id(&request.id, operation)?;
        let number = github_number(&request.id, operation)?;
        let observed = self.observe_change_request_for(&request.id, operation)?;

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
        let mut command = std::process::Command::new(self.config.tool("gh"));
        command
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
            &mut command,
            crate::process::ProcessPolicy::NetworkQuery,
            crate::process::ProcessDescriptor::new("gh.api.pull-request-review.create"),
        )
        .map_err(|error| github_provider_error(operation, error))?;
        if output.status.success() {
            if output.stdout_truncated {
                return Err(github_invalid_response(
                    operation,
                    "GitHub create pull request review response was truncated".to_string(),
                ));
            }
            let response =
                serde_json::from_str::<GithubCreatedReview>(&output.stdout).map_err(|error| {
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
        let message = if output.stderr.trim().is_empty() {
            format!(
                "gh api create pull request review exited with {}",
                output.status
            )
        } else {
            output.stderr.trim().to_string()
        };
        let error = github_provider_error(operation, message);
        Err(match output.status.code() {
            Some(exit_code) => error.with_exit_code(exit_code),
            None => error,
        })
    }

    pub(in crate::remote) fn resolve_review_thread(
        &self,
        request: &ResolveReviewThread,
    ) -> Result<(), RemoteError> {
        let operation = RemoteOperation::ResolveReviewThread;
        self.validate_change_request_id(&request.id, operation)?;
        let observed = self.observe_change_request_for(&request.id, operation)?;
        ensure_expected_head(
            &observed.change_request.head_sha,
            &request.expected_head_sha,
        )?;
        let details = self.change_request_details_for(&observed.change_request, operation)?;
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
        let immediately_before = self.observe_change_request_for(&request.id, operation)?;
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
        .map_err(|error| github_provider_error(operation, error))?;
        // The mutation response authoritatively confirms resolution. A successful
        // post-observation can still expose a concurrent head or identity change,
        // but an unavailable refresh must not report the completed mutation as failed.
        if let Ok(after) = self.observe_change_request_for(&request.id, operation) {
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
    github_error(
        operation,
        RemoteErrorClass::Provider,
        Retryability::Unknown,
        message,
    )
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
