#![allow(dead_code)]

use super::stabilization_model::PendingPushGuard;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GuardedPushDecision {
    AlreadySatisfied,
    ValidToPush,
    Invalidated { reason: String },
}

pub(crate) fn decide_guarded_push(
    guard: &PendingPushGuard,
    local_head_sha: Option<&str>,
    remote_head_sha: Option<&str>,
    pr_head_sha: Option<&str>,
) -> GuardedPushDecision {
    if pr_head_sha == Some(guard.commit_sha.as_str())
        || remote_head_sha == Some(guard.commit_sha.as_str())
    {
        return GuardedPushDecision::AlreadySatisfied;
    }

    if local_head_sha != Some(guard.expected_local_head_sha.as_str()) {
        return GuardedPushDecision::Invalidated {
            reason: format!(
                "local HEAD moved from {} to {}",
                short_sha(&guard.expected_local_head_sha),
                local_head_sha
                    .map(short_sha)
                    .unwrap_or("unknown".to_string())
            ),
        };
    }

    if remote_head_sha != guard.expected_remote_head_sha.as_deref() {
        return GuardedPushDecision::Invalidated {
            reason: format!(
                "remote branch moved from {} to {}",
                guard
                    .expected_remote_head_sha
                    .as_deref()
                    .map(short_sha)
                    .unwrap_or("none".to_string()),
                remote_head_sha.map(short_sha).unwrap_or("none".to_string())
            ),
        };
    }

    if let Some(expected_pr_head) = guard.expected_pr_head_sha.as_deref()
        && pr_head_sha.is_some()
        && pr_head_sha != Some(expected_pr_head)
    {
        return GuardedPushDecision::Invalidated {
            reason: format!(
                "PR head moved from {} to {}",
                short_sha(expected_pr_head),
                pr_head_sha.map(short_sha).unwrap_or("unknown".to_string())
            ),
        };
    }

    GuardedPushDecision::ValidToPush
}

fn short_sha(value: &str) -> String {
    value.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_flow::stabilization_model::RepairKind;

    #[test]
    fn valid_guarded_push_is_allowed() {
        let guard = guard();

        let decision = decide_guarded_push(&guard, Some("repair"), Some("remote"), Some("remote"));

        assert_eq!(decision, GuardedPushDecision::ValidToPush);
    }

    #[test]
    fn already_pushed_commit_is_satisfied() {
        let guard = guard();

        let decision = decide_guarded_push(&guard, Some("repair"), Some("repair"), Some("repair"));

        assert_eq!(decision, GuardedPushDecision::AlreadySatisfied);
    }

    #[test]
    fn local_head_movement_invalidates_guard() {
        let guard = guard();

        let decision = decide_guarded_push(&guard, Some("other"), Some("remote"), Some("remote"));

        assert!(matches!(decision, GuardedPushDecision::Invalidated { .. }));
    }

    #[test]
    fn remote_head_movement_invalidates_guard() {
        let guard = guard();

        let decision = decide_guarded_push(&guard, Some("repair"), Some("other"), Some("remote"));

        assert!(matches!(decision, GuardedPushDecision::Invalidated { .. }));
    }

    fn guard() -> PendingPushGuard {
        PendingPushGuard {
            repair_kind: RepairKind::Review,
            commit_sha: "repair".to_string(),
            expected_local_head_sha: "repair".to_string(),
            expected_remote_head_sha: Some("remote".to_string()),
            pr_number: Some(42),
            expected_pr_head_sha: Some("remote".to_string()),
            expected_base_sha: Some("base".to_string()),
            guarded_review_thread_ids: vec!["thread-1".to_string()],
        }
    }
}
