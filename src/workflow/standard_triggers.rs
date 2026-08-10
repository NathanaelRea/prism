//! Provider-neutral built-in Step Triggers used by the stabilization Workflow.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::step_trigger::{
    AgentOutcome, PostStepResult, PreparedState, StepTrigger, TriggerContext, TriggerDecision,
    TriggerError, TriggerFuture,
};

pub type StandardRemoteFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, TriggerError>> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StandardProvider {
    GitHub,
    GitLab,
    Forgejo,
    Other(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeRelation {
    Current,
    Behind,
    Conflicting,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mergeability {
    Mergeable,
    Conflicting,
    Blocked,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredCheckState {
    Passed,
    Pending,
    Failed,
    Cancelled,
    Skipped,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RequiredCheck {
    pub name: String,
    pub state: RequiredCheckState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReviewThreadObservation {
    pub id: String,
    pub revision: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeRequestObservation {
    pub provider: StandardProvider,
    pub change_request: String,
    pub head_sha: String,
    pub observation_revision: String,
    pub target_remote: String,
    pub target_branch: String,
    pub merge_relation: MergeRelation,
    pub mergeability: Mergeability,
    pub unresolved_threads: Vec<ReviewThreadObservation>,
    pub required_review_pending: bool,
    pub required_checks: Vec<RequiredCheck>,
    pub draft: bool,
    pub lifecycle_open: bool,
    pub policy_blockers: Vec<String>,
    /// An explicit provider capability gap. It is never interpreted as satisfied.
    pub unsupported: Option<String>,
}

impl ChangeRequestObservation {
    fn is_exact_current_head(&self) -> bool {
        !self.head_sha.trim().is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StandardObservationResult {
    Fresh(Box<ChangeRequestObservation>),
    Wait {
        summary: String,
        wake_at_unix_ms: i64,
    },
    Fail(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StandardMutationResult {
    Applied(String),
    Wait {
        summary: String,
        wake_at_unix_ms: i64,
    },
    Fail(String),
}

pub trait StandardTriggerRemote: Send + Sync + 'static {
    fn observe<'a>(
        &'a self,
        context: &'a TriggerContext,
    ) -> StandardRemoteFuture<'a, StandardObservationResult>;

    fn resolve_review_threads<'a>(
        &'a self,
        context: &'a TriggerContext,
        observation_revision: &'a str,
        thread_ids: &'a [String],
    ) -> StandardRemoteFuture<'a, StandardMutationResult>;
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MergePreparation {
    pub target_remote: String,
    pub target_branch: String,
    /// Conflicts are expected prepared state for an Agent, not a hook failure.
    pub has_conflicts: bool,
}

pub trait StandardGitOperations: Send + Sync + 'static {
    fn prepare_base_merge<'a>(
        &'a self,
        context: &'a TriggerContext,
        target_remote: &'a str,
        target_branch: &'a str,
    ) -> StandardRemoteFuture<'a, MergePreparation>;
}

#[derive(Clone)]
pub struct MergeConflictTrigger {
    remote: Arc<dyn StandardTriggerRemote>,
    git: Arc<dyn StandardGitOperations>,
}

impl MergeConflictTrigger {
    pub fn new(
        remote: Arc<dyn StandardTriggerRemote>,
        git: Arc<dyn StandardGitOperations>,
    ) -> Self {
        Self { remote, git }
    }
}

impl StepTrigger for MergeConflictTrigger {
    fn should_run_step<'a>(
        &'a self,
        context: &'a TriggerContext,
    ) -> TriggerFuture<'a, TriggerDecision> {
        Box::pin(async move {
            let observation = match observe_exact(self.remote.as_ref(), context).await? {
                ExactObservation::Fresh(observation) => observation,
                ExactObservation::Decision(decision) => return Ok(decision),
            };
            if let Some(reason) = observation.unsupported {
                return Ok(TriggerDecision::Fail { reason });
            }
            Ok(
                match (&observation.merge_relation, &observation.mergeability) {
                    (MergeRelation::Behind | MergeRelation::Conflicting, _)
                    | (_, Mergeability::Conflicting) => TriggerDecision::Run {
                        summary: match observation.merge_relation {
                            MergeRelation::Behind => "branch is behind its configured base".into(),
                            _ => "branch has merge conflicts".into(),
                        },
                    },
                    (MergeRelation::Current, Mergeability::Mergeable) => {
                        TriggerDecision::Satisfied {
                            summary: "branch is current and merges cleanly".into(),
                        }
                    }
                    (_, Mergeability::Blocked) => TriggerDecision::Wait {
                        summary: "provider mergeability is blocked; checking again".into(),
                        wake_at_unix_ms: next_poll_time(),
                    },
                    _ => TriggerDecision::Wait {
                        summary: "merge relation is still being calculated".into(),
                        wake_at_unix_ms: next_poll_time(),
                    },
                },
            )
        })
    }

    fn pre_step_run<'a>(&'a self, context: &'a TriggerContext) -> TriggerFuture<'a, PreparedState> {
        Box::pin(async move {
            let observation = match self.remote.observe(context).await? {
                StandardObservationResult::Fresh(observation)
                    if observation.is_exact_current_head() =>
                {
                    observation
                }
                StandardObservationResult::Fresh(_) => {
                    return Err(TriggerError::Protocol(
                        "provider returned merge preparation without an exact current head".into(),
                    ));
                }
                StandardObservationResult::Wait { summary, .. } => {
                    return Err(TriggerError::Protocol(format!(
                        "merge preparation observation is waiting: {summary}"
                    )));
                }
                StandardObservationResult::Fail(reason) => {
                    return Err(TriggerError::Protocol(reason));
                }
            };
            let prepared = self
                .git
                .prepare_base_merge(
                    context,
                    &observation.target_remote,
                    &observation.target_branch,
                )
                .await?;
            Ok(PreparedState(serde_json::to_value(prepared).map_err(
                |error| TriggerError::Protocol(error.to_string()),
            )?))
        })
    }
}

#[derive(Clone)]
pub struct NeedsReviewTrigger {
    remote: Arc<dyn StandardTriggerRemote>,
}

impl NeedsReviewTrigger {
    pub fn new(remote: Arc<dyn StandardTriggerRemote>) -> Self {
        Self { remote }
    }
}

impl StepTrigger for NeedsReviewTrigger {
    fn should_run_step<'a>(
        &'a self,
        context: &'a TriggerContext,
    ) -> TriggerFuture<'a, TriggerDecision> {
        Box::pin(async move {
            let observation = match observe_exact(self.remote.as_ref(), context).await? {
                ExactObservation::Fresh(observation) => observation,
                ExactObservation::Decision(decision) => return Ok(decision),
            };
            if let Some(reason) = observation.unsupported {
                return Ok(TriggerDecision::Fail { reason });
            }
            Ok(if observation.unresolved_threads.is_empty() {
                if observation.required_review_pending {
                    TriggerDecision::Wait {
                        summary: "required review is pending".into(),
                        wake_at_unix_ms: next_poll_time(),
                    }
                } else {
                    TriggerDecision::Satisfied {
                        summary: "no unresolved review threads".into(),
                    }
                }
            } else {
                TriggerDecision::Run {
                    summary: format!(
                        "{} actionable unresolved review thread(s)",
                        observation.unresolved_threads.len()
                    ),
                }
            })
        })
    }

    fn pre_step_run<'a>(&'a self, context: &'a TriggerContext) -> TriggerFuture<'a, PreparedState> {
        Box::pin(async move {
            let observation = match self.remote.observe(context).await? {
                StandardObservationResult::Fresh(observation)
                    if observation.is_exact_current_head() =>
                {
                    observation
                }
                StandardObservationResult::Fresh(_) => {
                    return Err(TriggerError::Protocol(
                        "provider returned review capture without an exact current head".into(),
                    ));
                }
                StandardObservationResult::Wait { summary, .. } => {
                    return Err(TriggerError::Protocol(format!(
                        "review capture is waiting: {summary}"
                    )));
                }
                StandardObservationResult::Fail(reason) => {
                    return Err(TriggerError::Protocol(reason));
                }
            };
            let state = CapturedReviewState {
                observation_revision: observation.observation_revision,
                head_sha: observation.head_sha,
                thread_ids: observation
                    .unresolved_threads
                    .into_iter()
                    .map(|thread| thread.id)
                    .collect(),
            };
            Ok(PreparedState(serde_json::to_value(state).map_err(
                |error| TriggerError::Protocol(error.to_string()),
            )?))
        })
    }

    fn post_step_run<'a>(
        &'a self,
        context: &'a TriggerContext,
        prepared: &'a PreparedState,
        _outcome: &'a AgentOutcome,
    ) -> TriggerFuture<'a, PostStepResult> {
        Box::pin(async move {
            let captured: CapturedReviewState = serde_json::from_value(prepared.0.clone())
                .map_err(|error| {
                    TriggerError::Protocol(format!("invalid captured review state: {error}"))
                })?;
            Ok(
                match self
                    .remote
                    .resolve_review_threads(
                        context,
                        &captured.observation_revision,
                        &captured.thread_ids,
                    )
                    .await?
                {
                    StandardMutationResult::Applied(summary) => PostStepResult::Success { summary },
                    StandardMutationResult::Wait {
                        summary,
                        wake_at_unix_ms,
                    } => PostStepResult::Wait {
                        summary,
                        wake_at_unix_ms,
                    },
                    StandardMutationResult::Fail(reason) => PostStepResult::Fail { reason },
                },
            )
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct CapturedReviewState {
    observation_revision: String,
    head_sha: String,
    thread_ids: Vec<String>,
}

#[derive(Clone)]
pub struct CiFailureTrigger {
    remote: Arc<dyn StandardTriggerRemote>,
}

impl CiFailureTrigger {
    pub fn new(remote: Arc<dyn StandardTriggerRemote>) -> Self {
        Self { remote }
    }
}

impl StepTrigger for CiFailureTrigger {
    fn should_run_step<'a>(
        &'a self,
        context: &'a TriggerContext,
    ) -> TriggerFuture<'a, TriggerDecision> {
        Box::pin(async move {
            let observation = match observe_exact(self.remote.as_ref(), context).await? {
                ExactObservation::Fresh(observation) => observation,
                ExactObservation::Decision(decision) => return Ok(decision),
            };
            if let Some(reason) = observation.unsupported {
                return Ok(TriggerDecision::Fail { reason });
            }
            let failed = observation
                .required_checks
                .iter()
                .filter(|check| {
                    matches!(
                        check.state,
                        RequiredCheckState::Failed
                            | RequiredCheckState::Cancelled
                            | RequiredCheckState::Skipped
                    )
                })
                .count();
            let pending = observation
                .required_checks
                .iter()
                .filter(|check| {
                    matches!(
                        check.state,
                        RequiredCheckState::Pending | RequiredCheckState::Unknown
                    )
                })
                .count();
            Ok(if failed > 0 {
                TriggerDecision::Run {
                    summary: format!("{failed} required check(s) failing"),
                }
            } else if pending > 0 {
                TriggerDecision::Wait {
                    summary: format!("{pending} required check(s) running; poll again"),
                    wake_at_unix_ms: next_poll_time(),
                }
            } else {
                TriggerDecision::Satisfied {
                    summary: "required checks passed".into(),
                }
            })
        })
    }
}

#[derive(Clone)]
pub struct ReadyToMergeTrigger {
    remote: Arc<dyn StandardTriggerRemote>,
}

impl ReadyToMergeTrigger {
    pub fn new(remote: Arc<dyn StandardTriggerRemote>) -> Self {
        Self { remote }
    }
}

impl StepTrigger for ReadyToMergeTrigger {
    fn should_run_step<'a>(
        &'a self,
        context: &'a TriggerContext,
    ) -> TriggerFuture<'a, TriggerDecision> {
        Box::pin(async move {
            let observation = match observe_exact(self.remote.as_ref(), context).await? {
                ExactObservation::Fresh(observation) => observation,
                ExactObservation::Decision(decision) => return Ok(decision),
            };
            if let Some(reason) = observation.unsupported {
                return Ok(TriggerDecision::Fail { reason });
            }
            if !observation.lifecycle_open {
                return Ok(TriggerDecision::Fail {
                    reason: "Change Request is no longer open".into(),
                });
            }
            if observation.draft {
                return Ok(TriggerDecision::Wait {
                    summary: "Change Request is still a draft".into(),
                    wake_at_unix_ms: next_poll_time(),
                });
            }
            if !observation.policy_blockers.is_empty() {
                return Ok(TriggerDecision::Fail {
                    reason: format!(
                        "provider policy blocker: {}",
                        observation.policy_blockers.join(", ")
                    ),
                });
            }
            if !observation.unresolved_threads.is_empty()
                || observation.required_review_pending
                || observation
                    .required_checks
                    .iter()
                    .any(|check| check.state != RequiredCheckState::Passed)
                || observation.merge_relation != MergeRelation::Current
                || observation.mergeability != Mergeability::Mergeable
            {
                return Ok(TriggerDecision::Wait {
                    summary: readiness_wait_summary(&observation),
                    wake_at_unix_ms: next_poll_time(),
                });
            }
            Ok(TriggerDecision::Satisfied {
                summary: "exact Change Request head is ready to merge".into(),
            })
        })
    }
}

enum ExactObservation {
    Fresh(Box<ChangeRequestObservation>),
    Decision(TriggerDecision),
}

async fn observe_exact(
    remote: &dyn StandardTriggerRemote,
    context: &TriggerContext,
) -> Result<ExactObservation, TriggerError> {
    Ok(match remote.observe(context).await? {
        StandardObservationResult::Fresh(observation) if observation.is_exact_current_head() => {
            ExactObservation::Fresh(observation)
        }
        StandardObservationResult::Fresh(_) => ExactObservation::Decision(TriggerDecision::Wait {
            summary: "provider returned an observation without an exact current head".into(),
            wake_at_unix_ms: next_poll_time(),
        }),
        StandardObservationResult::Wait {
            summary,
            wake_at_unix_ms,
        } => ExactObservation::Decision(TriggerDecision::Wait {
            summary,
            wake_at_unix_ms,
        }),
        StandardObservationResult::Fail(reason) => {
            ExactObservation::Decision(TriggerDecision::Fail { reason })
        }
    })
}

fn readiness_wait_summary(observation: &ChangeRequestObservation) -> String {
    if !observation.unresolved_threads.is_empty() {
        return format!(
            "{} unresolved review thread(s)",
            observation.unresolved_threads.len()
        );
    }
    if observation.required_review_pending {
        return "required review is pending".into();
    }
    let pending = observation
        .required_checks
        .iter()
        .filter(|check| check.state != RequiredCheckState::Passed)
        .count();
    if pending > 0 {
        return format!("{pending} required check(s) have not passed");
    }
    if observation.merge_relation != MergeRelation::Current {
        return "branch is not current with its configured base".into();
    }
    "provider is still calculating merge readiness".into()
}

fn next_poll_time() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
        .saturating_add(20_000)
}

/// Production structured Git preparation. `git merge` exit status 1 with an unmerged index is an
/// expected prepared state; all argv elements remain structured and never pass through a shell.
#[derive(Clone, Default)]
pub struct ProcessStandardGitOperations;

impl StandardGitOperations for ProcessStandardGitOperations {
    fn prepare_base_merge<'a>(
        &'a self,
        context: &'a TriggerContext,
        target_remote: &'a str,
        target_branch: &'a str,
    ) -> StandardRemoteFuture<'a, MergePreparation> {
        Box::pin(async move {
            let worktree = context.subject.worktree.clone();
            let repository = crate::repo::Repository {
                root: context.subject.repository.clone(),
            };
            let config = crate::config::Config::load(&repository);
            let git = config.tool("git").to_string();
            let remote = target_remote.to_string();
            let branch = target_branch.to_string();
            tokio::task::spawn_blocking(move || {
                crate::process::run_status_named(
                    std::process::Command::new(&git)
                        .arg("-C")
                        .arg(&worktree)
                        .args(["fetch", "--", &remote, &branch]),
                    crate::process::ProcessPolicy::NetworkQuery,
                    crate::process::ProcessDescriptor::new("workflow.trigger.git.fetch"),
                )
                .map_err(TriggerError::Fixture)?;
                let merge_target = format!("refs/remotes/{remote}/{branch}");
                let output = crate::process::run_output_allow_failure_named(
                    std::process::Command::new(&git)
                        .arg("-C")
                        .arg(&worktree)
                        .args(["merge", "--no-edit", "--no-ff", &merge_target]),
                    crate::process::ProcessPolicy::LocalMutation,
                    crate::process::ProcessDescriptor::new("workflow.trigger.git.merge"),
                )
                .map_err(TriggerError::Fixture)?;
                if output.status.success() {
                    return Ok(MergePreparation {
                        target_remote: remote,
                        target_branch: branch,
                        has_conflicts: false,
                    });
                }
                let unmerged = crate::process::run_capture_named(
                    std::process::Command::new(&git)
                        .arg("-C")
                        .arg(&worktree)
                        .args(["diff", "--name-only", "--diff-filter=U"]),
                    crate::process::ProcessPolicy::Metadata,
                    crate::process::ProcessDescriptor::new("workflow.trigger.git.conflicts"),
                )
                .map_err(TriggerError::Fixture)?;
                if unmerged.trim().is_empty() {
                    return Err(TriggerError::Fixture(format!(
                        "git merge failed without conflict markers: {}",
                        output.stderr.trim()
                    )));
                }
                Ok(MergePreparation {
                    target_remote: remote,
                    target_branch: branch,
                    has_conflicts: true,
                })
            })
            .await
            .map_err(|error| {
                TriggerError::Fixture(format!("join Git merge preparation: {error}"))
            })?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeRemote {
        observations: Mutex<VecDeque<StandardObservationResult>>,
        resolved: Mutex<Vec<Vec<String>>>,
    }

    impl FakeRemote {
        fn with(observations: impl IntoIterator<Item = ChangeRequestObservation>) -> Self {
            Self {
                observations: Mutex::new(
                    observations
                        .into_iter()
                        .map(|observation| StandardObservationResult::Fresh(Box::new(observation)))
                        .collect(),
                ),
                ..Self::default()
            }
        }
    }

    impl StandardTriggerRemote for FakeRemote {
        fn observe<'a>(
            &'a self,
            _context: &'a TriggerContext,
        ) -> StandardRemoteFuture<'a, StandardObservationResult> {
            let result = self.observations.lock().unwrap().pop_front().unwrap();
            Box::pin(async move { Ok(result) })
        }

        fn resolve_review_threads<'a>(
            &'a self,
            _context: &'a TriggerContext,
            _revision: &'a str,
            thread_ids: &'a [String],
        ) -> StandardRemoteFuture<'a, StandardMutationResult> {
            self.resolved.lock().unwrap().push(thread_ids.to_vec());
            Box::pin(async {
                Ok(StandardMutationResult::Applied(
                    "captured review threads resolved".into(),
                ))
            })
        }
    }

    #[derive(Default)]
    struct FakeGit;

    impl StandardGitOperations for FakeGit {
        fn prepare_base_merge<'a>(
            &'a self,
            _context: &'a TriggerContext,
            remote: &'a str,
            branch: &'a str,
        ) -> StandardRemoteFuture<'a, MergePreparation> {
            let prepared = MergePreparation {
                target_remote: remote.into(),
                target_branch: branch.into(),
                has_conflicts: true,
            };
            Box::pin(async move { Ok(prepared) })
        }
    }

    fn context() -> TriggerContext {
        TriggerContext {
            run_id: "run".into(),
            step_key: "step".into(),
            attempt_id: "attempt".into(),
            cycle: 1,
            cycle_started_unix_ms: 1,
            subject: super::super::step_trigger::TriggerSubject {
                repository: "/repo".into(),
                worktree: "/repo/wt".into(),
                change_request: Some("github:repo:1".into()),
                change_request_head: Some("head".into()),
            },
            cancellation_requested: false,
        }
    }

    fn observation() -> ChangeRequestObservation {
        ChangeRequestObservation {
            provider: StandardProvider::GitHub,
            change_request: "github:repo:1".into(),
            head_sha: "head".into(),
            observation_revision: "revision-1".into(),
            target_remote: "origin".into(),
            target_branch: "main".into(),
            merge_relation: MergeRelation::Current,
            mergeability: Mergeability::Mergeable,
            unresolved_threads: Vec::new(),
            required_review_pending: false,
            required_checks: vec![RequiredCheck {
                name: "test".into(),
                state: RequiredCheckState::Passed,
            }],
            draft: false,
            lifecycle_open: true,
            policy_blockers: Vec::new(),
            unsupported: None,
        }
    }

    #[tokio::test]
    async fn standard_trigger_decisions_cover_repair_wait_and_ready() {
        let mut conflict = observation();
        conflict.merge_relation = MergeRelation::Conflicting;
        let merge_remote = Arc::new(FakeRemote::with([conflict.clone(), conflict]));
        let merge = MergeConflictTrigger::new(merge_remote, Arc::new(FakeGit));
        assert!(matches!(
            merge.should_run_step(&context()).await.unwrap(),
            TriggerDecision::Run { .. }
        ));
        assert!(
            serde_json::from_value::<MergePreparation>(
                merge.pre_step_run(&context()).await.unwrap().0
            )
            .unwrap()
            .has_conflicts
        );

        let mut pending = observation();
        pending.required_checks[0].state = RequiredCheckState::Pending;
        let ci = CiFailureTrigger::new(Arc::new(FakeRemote::with([pending])));
        assert!(matches!(
            ci.should_run_step(&context()).await.unwrap(),
            TriggerDecision::Wait { .. }
        ));

        let ready = ReadyToMergeTrigger::new(Arc::new(FakeRemote::with([observation()])));
        assert!(matches!(
            ready.should_run_step(&context()).await.unwrap(),
            TriggerDecision::Satisfied { .. }
        ));
    }

    #[tokio::test]
    async fn review_finalize_resolves_only_captured_threads() {
        let mut first = observation();
        first.unresolved_threads = vec![ReviewThreadObservation {
            id: "T1".into(),
            revision: "one".into(),
        }];
        let remote = Arc::new(FakeRemote::with([first.clone(), first]));
        let trigger = NeedsReviewTrigger::new(remote.clone());
        assert!(matches!(
            trigger.should_run_step(&context()).await.unwrap(),
            TriggerDecision::Run { .. }
        ));
        let prepared = trigger.pre_step_run(&context()).await.unwrap();
        let result = trigger
            .post_step_run(
                &context(),
                &prepared,
                &AgentOutcome {
                    status: super::super::step_trigger::AgentOutcomeStatus::Succeeded,
                    process_id: Some(42),
                    session_id: "fresh".into(),
                    final_text: "done".into(),
                },
            )
            .await
            .unwrap();
        assert!(matches!(result, PostStepResult::Success { .. }));
        assert_eq!(
            remote.resolved.lock().unwrap().as_slice(),
            &[vec!["T1".to_string()]]
        );
    }

    #[tokio::test]
    async fn production_merge_prepare_preserves_expected_conflict_markers() {
        fn git(path: &std::path::Path, arguments: &[&str]) {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let root = std::env::temp_dir().join(format!(
            "prism-merge-prepare-{}-{}",
            std::process::id(),
            crate::workflow::prompt_worker::now_unix_ms()
        ));
        let remote = root.join("remote.git");
        let worktree = root.join("worktree");
        std::fs::create_dir_all(&root).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "--bare"])
            .arg(&remote)
            .output()
            .unwrap();
        assert!(init.status.success());
        std::fs::create_dir_all(&worktree).unwrap();
        git(&worktree, &["init", "-b", "main"]);
        git(&worktree, &["config", "user.email", "test@example.com"]);
        git(&worktree, &["config", "user.name", "Prism Test"]);
        std::fs::write(worktree.join("shared.txt"), "base\n").unwrap();
        git(&worktree, &["add", "shared.txt"]);
        git(&worktree, &["commit", "-m", "base"]);
        git(
            &worktree,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&worktree, &["push", "-u", "origin", "main"]);
        git(&worktree, &["checkout", "-b", "feature"]);
        std::fs::write(worktree.join("shared.txt"), "feature\n").unwrap();
        git(&worktree, &["commit", "-am", "feature"]);
        git(&worktree, &["checkout", "main"]);
        std::fs::write(worktree.join("shared.txt"), "main\n").unwrap();
        git(&worktree, &["commit", "-am", "main"]);
        git(&worktree, &["push", "origin", "main"]);
        git(&worktree, &["checkout", "feature"]);

        let context = TriggerContext {
            run_id: "run".into(),
            step_key: "merge".into(),
            attempt_id: "attempt".into(),
            cycle: 1,
            cycle_started_unix_ms: 1,
            subject: super::super::step_trigger::TriggerSubject {
                repository: worktree.clone(),
                worktree: worktree.clone(),
                change_request: None,
                change_request_head: None,
            },
            cancellation_requested: false,
        };
        let prepared = ProcessStandardGitOperations
            .prepare_base_merge(&context, "origin", "main")
            .await
            .unwrap();
        assert!(prepared.has_conflicts);
        let contents = std::fs::read_to_string(worktree.join("shared.txt")).unwrap();
        assert!(contents.contains("<<<<<<<"));
        assert!(contents.contains(">>>>>>>"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn deterministic_stabilization_reaches_ready_without_merging() {
        use crate::workflow::agent_phase::{AgentExecutor, AgentFuture, AgentRequest};
        use crate::workflow::kernel::{
            MemoryWorkflowRunStore, SchedulerProgress, StartPromptWorkflow, WorkflowRunStore,
            WorkflowScheduler,
        };
        use crate::workflow::source::{TriggerCatalog, compile_workflow};
        use crate::workflow::step_trigger::{AgentOutcomeStatus, TriggerRegistry, TriggerSubject};
        use std::path::Path;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct ScenarioRemote {
            stage: Arc<AtomicUsize>,
        }
        impl StandardTriggerRemote for ScenarioRemote {
            fn observe<'a>(
                &'a self,
                _context: &'a TriggerContext,
            ) -> StandardRemoteFuture<'a, StandardObservationResult> {
                let stage = self.stage.load(Ordering::Acquire);
                let mut observed = observation();
                observed.observation_revision = format!("revision-{stage}");
                match stage {
                    0 => {
                        observed.merge_relation = MergeRelation::Conflicting;
                        observed.mergeability = Mergeability::Conflicting;
                    }
                    1 => {
                        observed.unresolved_threads = vec![ReviewThreadObservation {
                            id: "T1".into(),
                            revision: "one".into(),
                        }];
                        observed.required_checks[0].state = RequiredCheckState::Failed;
                    }
                    2 => observed.required_checks[0].state = RequiredCheckState::Failed,
                    3 => {
                        // A new review arrives after the CI repair Agent. Root re-evaluation must
                        // process it before the later CI Wait.
                        observed.unresolved_threads = vec![ReviewThreadObservation {
                            id: "T2".into(),
                            revision: "two".into(),
                        }];
                        observed.required_checks[0].state = RequiredCheckState::Pending;
                    }
                    4 => observed.required_checks[0].state = RequiredCheckState::Pending,
                    _ => observed.required_checks[0].state = RequiredCheckState::Passed,
                }
                Box::pin(async move { Ok(StandardObservationResult::Fresh(Box::new(observed))) })
            }

            fn resolve_review_threads<'a>(
                &'a self,
                _context: &'a TriggerContext,
                _revision: &'a str,
                _threads: &'a [String],
            ) -> StandardRemoteFuture<'a, StandardMutationResult> {
                self.stage.fetch_add(1, Ordering::AcqRel);
                Box::pin(async {
                    Ok(StandardMutationResult::Applied(
                        "captured threads resolved".into(),
                    ))
                })
            }
        }

        struct ScenarioGit;
        impl StandardGitOperations for ScenarioGit {
            fn prepare_base_merge<'a>(
                &'a self,
                _context: &'a TriggerContext,
                remote: &'a str,
                branch: &'a str,
            ) -> StandardRemoteFuture<'a, MergePreparation> {
                let prepared = MergePreparation {
                    target_remote: remote.into(),
                    target_branch: branch.into(),
                    has_conflicts: true,
                };
                Box::pin(async move { Ok(prepared) })
            }
        }

        struct ScenarioAgent {
            stage: Arc<AtomicUsize>,
            sessions: AtomicUsize,
        }
        impl AgentExecutor for ScenarioAgent {
            fn execute<'a>(&'a self, request: AgentRequest) -> AgentFuture<'a> {
                let session = self.sessions.fetch_add(1, Ordering::AcqRel) + 1;
                match request.step_key.as_str() {
                    "step-1" => self.stage.store(1, Ordering::Release),
                    "step-3" => self.stage.store(3, Ordering::Release),
                    _ => {}
                }
                Box::pin(async move {
                    Ok(super::super::step_trigger::AgentOutcome {
                        status: AgentOutcomeStatus::Succeeded,
                        process_id: Some(u32::try_from(session).unwrap()),
                        session_id: format!("fresh-{session}"),
                        final_text: format!("completed {}", request.step_key),
                    })
                })
            }
        }

        let workflow = compile_workflow(
            Path::new("stabilize.toml"),
            crate::workflow::source::DEFAULT_STABILIZE_SOURCE,
            &TriggerCatalog::builtins(),
        )
        .unwrap();
        let stage = Arc::new(AtomicUsize::new(0));
        let remote = Arc::new(ScenarioRemote {
            stage: stage.clone(),
        });
        let triggers = TriggerRegistry::default();
        triggers
            .insert(
                "merge_conflict",
                MergeConflictTrigger::new(remote.clone(), Arc::new(ScenarioGit)),
            )
            .unwrap();
        triggers
            .insert("needs_review", NeedsReviewTrigger::new(remote.clone()))
            .unwrap();
        triggers
            .insert("ci_failure", CiFailureTrigger::new(remote.clone()))
            .unwrap();
        triggers
            .insert("ready_to_merge", ReadyToMergeTrigger::new(remote))
            .unwrap();
        let agents = Arc::new(ScenarioAgent {
            stage: stage.clone(),
            sessions: AtomicUsize::new(0),
        });
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let scheduler = WorkflowScheduler::new(store.clone(), triggers, agents.clone());
        scheduler
            .start(StartPromptWorkflow {
                run_id: "acceptance",
                workflow: &workflow,
                subject: TriggerSubject {
                    repository: "/repo".into(),
                    worktree: "/repo/wt".into(),
                    change_request: Some("github:repo:1".into()),
                    change_request_head: Some("head".into()),
                },
                now_unix_ms: 1,
            })
            .await
            .unwrap();

        for now in 2..20 {
            let progress = scheduler.tick("acceptance", now).await.unwrap();
            if progress == SchedulerProgress::Waiting && stage.load(Ordering::Acquire) == 4 {
                break;
            }
        }
        assert_eq!(stage.load(Ordering::Acquire), 4);
        let waiting = store.load_run("acceptance").await.unwrap().unwrap();
        assert_eq!(waiting.agent_runs_consumed, 4);
        assert!(waiting.steps.iter().any(|step| {
            step.summary
                .as_deref()
                .is_some_and(|summary| summary.contains("running"))
        }));

        stage.store(5, Ordering::Release);
        scheduler.wake("acceptance", 30).await.unwrap();
        assert_eq!(
            scheduler.tick("acceptance", 30).await.unwrap(),
            SchedulerProgress::Succeeded
        );
        let complete = store.load_run("acceptance").await.unwrap().unwrap();
        assert_eq!(complete.status, crate::PromptWorkflowRunStatus::Succeeded);
        assert_eq!(agents.sessions.load(Ordering::Acquire), 4);
        let sessions = complete
            .steps
            .iter()
            .flat_map(|step| &step.attempts)
            .filter_map(|attempt| attempt.agent_outcome.as_ref())
            .map(|outcome| outcome.session_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(sessions.len(), 4);
        // ready_to_merge is check-only: no merge or cleanup lifecycle exists.
        assert_eq!(complete.steps[3].attempts.len(), 0);
    }

    #[tokio::test]
    async fn later_cycles_accept_the_provider_current_head_after_an_agent_push() {
        let mut current = observation();
        current.head_sha = "new-head-after-push".into();
        let trigger = ReadyToMergeTrigger::new(Arc::new(FakeRemote::with([current])));
        assert!(matches!(
            trigger.should_run_step(&context()).await.unwrap(),
            TriggerDecision::Satisfied { .. }
        ));
    }

    #[tokio::test]
    async fn capability_gaps_fail_instead_of_becoming_satisfied() {
        let mut unsupported = observation();
        unsupported.provider = StandardProvider::GitLab;
        unsupported.unsupported = Some("GitLab required-check policy is not supported".into());
        let trigger = ReadyToMergeTrigger::new(Arc::new(FakeRemote::with([unsupported])));
        assert!(matches!(
            trigger.should_run_step(&context()).await.unwrap(),
            TriggerDecision::Fail { .. }
        ));
    }
}
