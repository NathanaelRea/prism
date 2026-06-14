use crate::session::Session;
use crate::util::empty_dash;

pub fn build_review_fix_prompt(session: &Session) -> Result<String, String> {
    let summary = session
        .pr
        .summary
        .as_ref()
        .ok_or_else(|| "no pull request found for selected branch".to_string())?;
    let details = session
        .pr
        .details
        .clone()
        .ok_or_else(|| "PR comments are still loading; refresh and try again".to_string())?;

    let mut review_comments = details.review_comments;
    review_comments.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.author.cmp(&b.author))
            .then_with(|| a.body.cmp(&b.body))
    });

    let mut prompt = format!(
        "Here are review comments on PR {}.\n\nIf they are applicable, fix them. Otherwise, say why not.\n\n---\n\n",
        summary.number
    );

    let mut wrote_comment = false;
    for comment in &review_comments {
        if comment.body.trim().is_empty() {
            continue;
        }
        if comment.resolved {
            continue;
        }
        let line = if comment.line.is_empty() {
            String::new()
        } else {
            format!(" line {}", comment.line)
        };
        wrote_comment = true;
        prompt.push_str(&format!("{}{}\n\n", empty_dash(&comment.path), line));
        prompt.push_str(comment.body.trim());
        prompt.push_str("\n\n---\n\n");
    }

    if !wrote_comment {
        prompt.push_str("No PR review comments were found.\n\n");
    }

    Ok(prompt)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;

    use crate::agent::AgentState;
    use crate::github::{PrCache, PrComment, PrDetails, PrReview, PrReviewComment, PrSummary};

    use super::*;

    #[test]
    fn review_fix_prompt_contains_unresolved_review_comments_only() {
        let session = test_session(PrDetails {
            comments: vec![PrComment {
                author: "alice".to_string(),
                body: "Please simplify this branch.".to_string(),
            }],
            reviews: vec![PrReview {
                author: "bob".to_string(),
                state: "CHANGES_REQUESTED".to_string(),
                body: "This should mention the fallback behavior.".to_string(),
            }],
            review_comments: vec![
                PrReviewComment {
                    author: "carol".to_string(),
                    path: "src/lib.rs".to_string(),
                    line: "42".to_string(),
                    body: "This resolved comment should stay out.".to_string(),
                    created_at: "2026-06-14T12:00:00Z".to_string(),
                    resolved: true,
                },
                PrReviewComment {
                    author: "dana".to_string(),
                    path: "src/review.rs".to_string(),
                    line: "9".to_string(),
                    body: "Can this be a helper?".to_string(),
                    created_at: "2026-06-14T12:05:00Z".to_string(),
                    resolved: false,
                },
            ],
            files: vec!["src/lib.rs".to_string()],
            failing_checks: vec!["cargo test".to_string()],
        });

        let prompt = build_review_fix_prompt(&session).unwrap();

        assert!(prompt.starts_with(
            "Here are review comments on PR 123.\n\nIf they are applicable, fix them. Otherwise, say why not.\n\n---\n\n"
        ));
        assert!(prompt.contains("src/review.rs line 9\n\nCan this be a helper?"));
        assert!(prompt.contains("\n\n---\n\n"));
        assert!(!prompt.contains("Please simplify this branch."));
        assert!(!prompt.contains("This should mention the fallback behavior."));
        assert!(prompt.contains("Can this be a helper?"));
        assert!(!prompt.contains("This resolved comment should stay out."));
        assert!(!prompt.contains("Inline comment"));
        assert!(!prompt.contains("Comment from"));
        assert!(!prompt.contains("Review from"));
        assert!(!prompt.contains("<open>"));
        assert!(!prompt.contains("<resolved>"));
        assert!(!prompt.contains("not resolved"));
        assert!(!prompt.contains("##"));
        assert!(!prompt.contains("###"));
        assert!(!prompt.contains("Review Packet"));
        assert!(!prompt.contains("PR Comments"));
        assert!(!prompt.contains("Changed Files"));
        assert!(!prompt.contains("Failing Checks"));
        assert!(!prompt.contains("2026-06-14"));
        assert!(!prompt.contains("Make the requested changes"));
        assert!(!prompt.contains("cargo test"));
    }

    fn test_session(details: PrDetails) -> Session {
        Session {
            path: PathBuf::from("/repo/worktree"),
            path_display: "/repo/worktree".to_string(),
            branch: "feature".to_string(),
            prompt_summary: String::new(),
            adopted: false,
            hidden: false,
            status_label: "clean".to_string(),
            agent: None,
            agent_output: VecDeque::new(),
            agent_state: AgentState::Idle,
            pr: PrCache {
                summary: Some(PrSummary {
                    number: 123,
                    title: "Title".to_string(),
                    body: String::new(),
                    url: "https://example.test/pr/123".to_string(),
                    state: "OPEN".to_string(),
                    review_decision: "CHANGES_REQUESTED".to_string(),
                    head_ref: "feature".to_string(),
                    base_ref: "main".to_string(),
                    head_sha: "abc123".to_string(),
                    updated_at: "2026-06-14T12:00:00Z".to_string(),
                    check_status: "SUCCESS".to_string(),
                    comment_count: 3,
                    merged: false,
                    draft: false,
                }),
                details: Some(details),
                ..PrCache::default()
            },
        }
    }
}
