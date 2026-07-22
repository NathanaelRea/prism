#![allow(dead_code)]

use std::path::PathBuf;

use crate::github::{CiFailure, PrCheckState};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StabilizationStatus {
    Observing,
    Blocked,
    Waiting,
    Ready,
    Done,
    Escalated,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StabilizationSnapshot {
    pub run: Option<AutoRunRef>,
    pub repository: RepositoryFacts,
    pub worktree: WorktreeFacts,
    pub pull_request: Option<PullRequestFacts>,
    pub policy: PolicyFacts,
    pub goal: StabilizationGoal,
    pub pending_push: Option<PendingPushGuard>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AutoRunRef {
    pub id: String,
    pub status: super::AutoRunStatus,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
    pub current_head_sha: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepositoryFacts {
    pub root: PathBuf,
    pub default_base: Option<String>,
    pub github_remote: Option<String>,
    pub policy_refreshed_unix_ms: Option<u64>,
    pub policy_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorktreeFacts {
    pub path: PathBuf,
    pub branch: String,
    pub is_default_branch: bool,
    pub detached: bool,
    pub dirty: bool,
    pub local_head_sha: Option<String>,
    pub remote_head_sha: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PullRequestFacts {
    pub number: u64,
    pub url: String,
    pub state: PullRequestState,
    pub draft: bool,
    pub head_sha: String,
    pub base_ref: String,
    pub base_sha: Option<String>,
    pub updated_at: String,
    pub ci: CiFacts,
    pub review: ReviewFacts,
    pub mergeability: MergeabilityFacts,
    pub top_level_comment_count: usize,
    pub observation_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PullRequestState {
    Open,
    Closed,
    Merged,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CiFacts {
    pub aggregate: PrCheckState,
    pub required: Vec<CheckFact>,
    pub optional_failures: Vec<String>,
    pub failures: Vec<CiFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CheckFact {
    pub name: String,
    pub state: PrCheckState,
    pub required: bool,
    pub head_sha: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReviewFacts {
    pub decision: String,
    pub approval_required: bool,
    pub actionable_reviews: Vec<ActionableReviewItem>,
    pub unresolved_threads: Vec<ReviewThreadFact>,
    pub top_level_comments: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ActionableReviewItem {
    ReviewBody {
        review_id: String,
        author: String,
        state: String,
        body: String,
        submitted_at: String,
    },
    ReviewThreadComment(ReviewThreadFact),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReviewThreadFact {
    pub thread_id: String,
    pub comment_id: String,
    pub path: String,
    pub line: Option<u64>,
    pub body: String,
    pub author: String,
    pub resolved: bool,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MergeabilityFacts {
    Unknown,
    Clean,
    Blocked { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PolicyFacts {
    Unknown { reason: Option<String> },
    Satisfied,
    Blocked { blockers: Vec<PolicyBlocker> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PolicyBlocker {
    RequiredApprovalMissing,
    RequiredCheckMissing(String),
    RequiredCheckFailing(String),
    ConversationsUnresolved,
    BranchOutOfDate,
    PermissionDenied,
    MergeQueueRequired,
    Unknown(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPushGuard {
    pub repair_kind: RepairKind,
    pub commit_sha: String,
    pub expected_local_head_sha: String,
    pub expected_remote_head_sha: Option<String>,
    pub pr_number: Option<u64>,
    pub expected_pr_head_sha: Option<String>,
    pub expected_base_sha: Option<String>,
    pub guarded_review_thread_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepairKind {
    Review,
    Ci,
    Merge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StabilizationGoal {
    pub auto_merge: bool,
    pub cleanup_after_merge: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StabilizationBlocker {
    NotEligible,
    NeedsImplementation,
    NeedsPullRequest,
    PendingPush,
    DirtyWorktree,
    ObservationFailed,
    DraftPullRequest,
    WrongBase,
    HeadDiverged,
    MergeBlocked,
    ReviewFeedbackFound,
    ReviewApprovalMissing,
    CiFailed,
    CiPending,
    CiMissingRequiredChecks,
    PolicyBlocked,
    PolicyUnknown,
    ReadyForManualMerge,
    ReadyToAutoMerge,
    Merged,
    Escalate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StabilizationWorkItem {
    pub kind: StabilizationWorkKind,
    pub blocker: StabilizationBlocker,
    pub reason: String,
    pub guard: WorkGuard,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StabilizationWorkKind {
    RunImplementation,
    RunPlan,
    RunLocalVerification,
    CommitImplementation,
    PushInitialAndOpenPr,
    PushPendingRepair,
    FixReview,
    VerifyReviewFix,
    CommitReviewFix,
    FixCi,
    VerifyCiFix,
    CommitCiFix,
    WaitForCi,
    WaitForReview,
    MarkReadyForManualMerge,
    Merge,
    Done,
    Escalate,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkGuard {
    pub local_head_sha: Option<String>,
    pub remote_head_sha: Option<String>,
    pub pr_head_sha: Option<String>,
    pub base_sha: Option<String>,
    pub review_thread_ids: Vec<String>,
}

impl StabilizationStatus {
    pub(crate) fn keeps_run_active(self) -> bool {
        matches!(
            self,
            Self::Observing | Self::Blocked | Self::Waiting | Self::Ready
        )
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Observing => "observing",
            Self::Blocked => "blocked",
            Self::Waiting => "waiting",
            Self::Ready => "ready",
            Self::Done => "done",
            Self::Escalated => "escalated",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "observing" => Ok(Self::Observing),
            "blocked" => Ok(Self::Blocked),
            "waiting" => Ok(Self::Waiting),
            "ready" => Ok(Self::Ready),
            "done" => Ok(Self::Done),
            "escalated" => Ok(Self::Escalated),
            _ => Err(format!("unknown stabilization status: {value}")),
        }
    }
}

impl StabilizationBlocker {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::NotEligible => "not_eligible",
            Self::NeedsImplementation => "needs_implementation",
            Self::NeedsPullRequest => "needs_pull_request",
            Self::PendingPush => "pending_push",
            Self::DirtyWorktree => "dirty_worktree",
            Self::ObservationFailed => "observation_failed",
            Self::DraftPullRequest => "draft_pull_request",
            Self::WrongBase => "wrong_base",
            Self::HeadDiverged => "head_diverged",
            Self::MergeBlocked => "merge_blocked",
            Self::ReviewFeedbackFound => "review_feedback_found",
            Self::ReviewApprovalMissing => "review_approval_missing",
            Self::CiFailed => "ci_failed",
            Self::CiPending => "ci_pending",
            Self::CiMissingRequiredChecks => "ci_missing_required_checks",
            Self::PolicyBlocked => "policy_blocked",
            Self::PolicyUnknown => "policy_unknown",
            Self::ReadyForManualMerge => "ready_for_manual_merge",
            Self::ReadyToAutoMerge => "ready_to_auto_merge",
            Self::Merged => "merged",
            Self::Escalate => "escalate",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "not_eligible" => Ok(Self::NotEligible),
            "needs_implementation" => Ok(Self::NeedsImplementation),
            "needs_pull_request" => Ok(Self::NeedsPullRequest),
            "pending_push" => Ok(Self::PendingPush),
            "dirty_worktree" => Ok(Self::DirtyWorktree),
            "observation_failed" => Ok(Self::ObservationFailed),
            "draft_pull_request" => Ok(Self::DraftPullRequest),
            "wrong_base" => Ok(Self::WrongBase),
            "head_diverged" => Ok(Self::HeadDiverged),
            "merge_blocked" => Ok(Self::MergeBlocked),
            "review_feedback_found" => Ok(Self::ReviewFeedbackFound),
            "review_approval_missing" => Ok(Self::ReviewApprovalMissing),
            "ci_failed" => Ok(Self::CiFailed),
            "ci_pending" => Ok(Self::CiPending),
            "ci_missing_required_checks" => Ok(Self::CiMissingRequiredChecks),
            "policy_blocked" => Ok(Self::PolicyBlocked),
            "policy_unknown" => Ok(Self::PolicyUnknown),
            "ready_for_manual_merge" => Ok(Self::ReadyForManualMerge),
            "ready_to_auto_merge" => Ok(Self::ReadyToAutoMerge),
            "merged" => Ok(Self::Merged),
            "escalate" => Ok(Self::Escalate),
            _ => Err(format!("unknown stabilization blocker: {value}")),
        }
    }
}

impl StabilizationWorkKind {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::RunImplementation => "run_implementation",
            Self::RunPlan => "run_plan",
            Self::RunLocalVerification => "run_local_verification",
            Self::CommitImplementation => "commit_implementation",
            Self::PushInitialAndOpenPr => "push_initial_and_open_pr",
            Self::PushPendingRepair => "push_pending_repair",
            Self::FixReview => "fix_review",
            Self::VerifyReviewFix => "verify_review_fix",
            Self::CommitReviewFix => "commit_review_fix",
            Self::FixCi => "fix_ci",
            Self::VerifyCiFix => "verify_ci_fix",
            Self::CommitCiFix => "commit_ci_fix",
            Self::WaitForCi => "wait_for_ci",
            Self::WaitForReview => "wait_for_review",
            Self::MarkReadyForManualMerge => "mark_ready_for_manual_merge",
            Self::Merge => "merge",
            Self::Done => "done",
            Self::Escalate => "escalate",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "run_implementation" => Ok(Self::RunImplementation),
            "run_plan" => Ok(Self::RunPlan),
            "run_local_verification" => Ok(Self::RunLocalVerification),
            "commit_implementation" => Ok(Self::CommitImplementation),
            "push_initial_and_open_pr" => Ok(Self::PushInitialAndOpenPr),
            "push_pending_repair" => Ok(Self::PushPendingRepair),
            "fix_review" => Ok(Self::FixReview),
            "verify_review_fix" => Ok(Self::VerifyReviewFix),
            "commit_review_fix" => Ok(Self::CommitReviewFix),
            "fix_ci" => Ok(Self::FixCi),
            "verify_ci_fix" => Ok(Self::VerifyCiFix),
            "commit_ci_fix" => Ok(Self::CommitCiFix),
            "wait_for_ci" => Ok(Self::WaitForCi),
            "wait_for_review" => Ok(Self::WaitForReview),
            "mark_ready_for_manual_merge" => Ok(Self::MarkReadyForManualMerge),
            "merge" => Ok(Self::Merge),
            "done" => Ok(Self::Done),
            "escalate" => Ok(Self::Escalate),
            _ => Err(format!("unknown stabilization work kind: {value}")),
        }
    }
}

impl StabilizationWorkItem {
    pub(crate) fn status(&self) -> StabilizationStatus {
        match self.kind {
            StabilizationWorkKind::WaitForCi | StabilizationWorkKind::WaitForReview => {
                StabilizationStatus::Waiting
            }
            StabilizationWorkKind::MarkReadyForManualMerge | StabilizationWorkKind::Merge => {
                StabilizationStatus::Ready
            }
            StabilizationWorkKind::Done => StabilizationStatus::Done,
            StabilizationWorkKind::Escalate => StabilizationStatus::Escalated,
            _ => StabilizationStatus::Blocked,
        }
    }
}
