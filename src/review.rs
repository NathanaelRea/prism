use crate::config::Config;
use crate::github::{PrDetails, PrSummary, pr_cache_excluded_branch};
use crate::session::Session;
use crate::util::empty_dash;

const DEFAULT_REVIEW_FIX_TEMPLATE: &str = "Here are review comments on PR {pr_number}.\n\nIf they are applicable, fix them. Otherwise, say why not.\n\n---\n\n{comments}";

pub struct ReviewFixPromptInput<'a> {
    pub branch: &'a str,
    pub summary: &'a PrSummary,
    pub details: &'a PrDetails,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReviewFeedbackFilter<'a> {
    pub after: Option<&'a str>,
    pub authors: &'a [&'a str],
}

#[derive(Clone, Debug, Default)]
pub struct ReviewFeedback<'a> {
    pub inline_comments: Vec<&'a crate::github::PrReviewComment>,
    pub review_bodies: Vec<&'a crate::github::PrReview>,
    pub pr_comments: Vec<&'a crate::github::PrComment>,
    pub skipped_resolved_inline: usize,
    pub skipped_old: usize,
    pub skipped_author: usize,
    pub skipped_empty: usize,
}

impl ReviewFeedback<'_> {
    pub fn is_actionable(&self) -> bool {
        !self.inline_comments.is_empty()
            || !self.review_bodies.is_empty()
            || !self.pr_comments.is_empty()
    }
}

pub fn build_review_fix_prompt(session: &Session, config: &Config) -> Result<String, String> {
    if pr_cache_excluded_branch(config, &session.branch) {
        return Err("selected branch is not treated as a PR branch".to_string());
    }
    let summary = session
        .pr
        .summary
        .as_ref()
        .ok_or_else(|| "no pull request found for selected branch".to_string())?;
    let details = session
        .pr
        .details
        .as_ref()
        .ok_or_else(|| "PR comments are still loading; refresh and try again".to_string())?;

    Ok(build_review_fix_prompt_from_input(
        ReviewFixPromptInput {
            branch: &session.branch,
            summary,
            details,
        },
        config,
    ))
}

pub fn build_review_fix_prompt_from_input(
    input: ReviewFixPromptInput<'_>,
    config: &Config,
) -> String {
    let feedback = actionable_review_feedback(input.details, ReviewFeedbackFilter::default());
    let comments = render_review_feedback(&feedback);

    render_template(
        config
            .prompt_template("review_fix")
            .unwrap_or(DEFAULT_REVIEW_FIX_TEMPLATE),
        &[
            ("pr_number", input.summary.number.to_string()),
            ("branch", input.branch.to_string()),
            ("title", input.summary.title.clone()),
            ("url", input.summary.url.clone()),
            ("comments", comments),
        ],
    )
}

pub fn actionable_review_feedback<'a>(
    details: &'a PrDetails,
    filter: ReviewFeedbackFilter<'_>,
) -> ReviewFeedback<'a> {
    let mut feedback = ReviewFeedback::default();

    for comment in &details.review_comments {
        if comment.body.trim().is_empty() {
            feedback.skipped_empty += 1;
            continue;
        }
        if comment.resolved {
            feedback.skipped_resolved_inline += 1;
            continue;
        }
        if !passes_author_filter(&comment.author, filter.authors) {
            feedback.skipped_author += 1;
            continue;
        }
        if !passes_time_filter(&comment.created_at, filter.after) {
            feedback.skipped_old += 1;
            continue;
        }
        feedback.inline_comments.push(comment);
    }

    for review in &details.reviews {
        if review.body.trim().is_empty() {
            feedback.skipped_empty += 1;
            continue;
        }
        if !passes_author_filter(&review.author, filter.authors) {
            feedback.skipped_author += 1;
            continue;
        }
        if !passes_time_filter(&review.submitted_at, filter.after) {
            feedback.skipped_old += 1;
            continue;
        }
        feedback.review_bodies.push(review);
    }

    for comment in &details.comments {
        if comment.body.trim().is_empty() {
            feedback.skipped_empty += 1;
            continue;
        }
        if !passes_author_filter(&comment.author, filter.authors) {
            feedback.skipped_author += 1;
            continue;
        }
        if !passes_time_filter(&comment.created_at, filter.after) {
            feedback.skipped_old += 1;
            continue;
        }
        feedback.pr_comments.push(comment);
    }

    feedback.inline_comments.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.author.cmp(&b.author))
            .then_with(|| a.body.cmp(&b.body))
    });
    feedback.review_bodies.sort_by(|a, b| {
        a.submitted_at
            .cmp(&b.submitted_at)
            .then_with(|| a.author.cmp(&b.author))
            .then_with(|| a.body.cmp(&b.body))
    });
    feedback.pr_comments.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.author.cmp(&b.author))
            .then_with(|| a.body.cmp(&b.body))
    });

    feedback
}

fn render_review_feedback(feedback: &ReviewFeedback<'_>) -> String {
    let mut comments = String::new();

    if feedback.is_actionable() {
        if !feedback.inline_comments.is_empty() {
            comments.push_str("Inline review comments:\n\n");
            for comment in &feedback.inline_comments {
                let line = if comment.line.is_empty() {
                    String::new()
                } else {
                    format!(" line {}", comment.line)
                };
                comments.push_str(&format!(
                    "- {}{} by {}\n\n{}\n\n",
                    empty_dash(&comment.path),
                    line,
                    empty_dash(&comment.author),
                    comment.body.trim()
                ));
            }
        }

        if !feedback.review_bodies.is_empty() {
            comments.push_str("Review bodies:\n\n");
            for review in &feedback.review_bodies {
                let state = if review.state.trim().is_empty() {
                    String::new()
                } else {
                    format!(" ({})", review.state.trim())
                };
                comments.push_str(&format!(
                    "- Review from {}{}\n\n{}\n\n",
                    empty_dash(&review.author),
                    state,
                    review.body.trim()
                ));
            }
        }

        if !feedback.pr_comments.is_empty() {
            comments.push_str("PR comments:\n\n");
            for comment in &feedback.pr_comments {
                comments.push_str(&format!(
                    "- Comment from {}\n\n{}\n\n",
                    empty_dash(&comment.author),
                    comment.body.trim()
                ));
            }
        }

        return comments;
    }

    comments.push_str("No actionable PR review feedback was found.\n\n");
    let skipped_total = feedback.skipped_resolved_inline
        + feedback.skipped_old
        + feedback.skipped_author
        + feedback.skipped_empty;
    if skipped_total > 0 {
        comments.push_str("Skipped feedback:\n");
        if feedback.skipped_resolved_inline > 0 {
            comments.push_str(&format!(
                "- {} resolved inline comment(s)\n",
                feedback.skipped_resolved_inline
            ));
        }
        if feedback.skipped_old > 0 {
            comments.push_str(&format!(
                "- {} comment(s) at or before the baseline\n",
                feedback.skipped_old
            ));
        }
        if feedback.skipped_author > 0 {
            comments.push_str(&format!(
                "- {} comment(s) outside the configured author filter\n",
                feedback.skipped_author
            ));
        }
        if feedback.skipped_empty > 0 {
            comments.push_str(&format!("- {} empty comment(s)\n", feedback.skipped_empty));
        }
        comments.push('\n');
    }
    comments
}

fn passes_author_filter(author: &str, allowed: &[&str]) -> bool {
    allowed.is_empty() || allowed.contains(&author)
}

fn passes_time_filter(value: &str, after: Option<&str>) -> bool {
    let Some(after) = after else {
        return true;
    };
    value > after
}

fn render_template(template: &str, values: &[(&str, String)]) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start + 1..].find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let placeholder_end = start + end + 2;
        let key = &rest[start + 1..placeholder_end - 1];
        if let Some((_, value)) = values.iter().find(|(name, _)| *name == key) {
            out.push_str(value);
        } else {
            out.push_str(&rest[start..placeholder_end]);
        }
        rest = &rest[placeholder_end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use crate::agent::AgentState;
    use crate::config::{Checks, Config, EscapeKey};
    use crate::github::{PrCache, PrComment, PrDetails, PrReview, PrReviewComment, PrSummary};

    use super::*;

    #[test]
    fn review_fix_prompt_contains_actionable_feedback_sections() {
        let session = test_session(PrDetails {
            comments: vec![PrComment {
                author: "alice".to_string(),
                body: "Please simplify this branch.".to_string(),
                created_at: "2026-06-14T12:10:00Z".to_string(),
                ..PrComment::default()
            }],
            reviews: vec![PrReview {
                author: "bob".to_string(),
                state: "CHANGES_REQUESTED".to_string(),
                body: "This should mention the fallback behavior.".to_string(),
                submitted_at: "2026-06-14T12:09:00Z".to_string(),
                ..PrReview::default()
            }],
            review_comments: vec![
                PrReviewComment {
                    author: "carol".to_string(),
                    path: "src/lib.rs".to_string(),
                    line: "42".to_string(),
                    body: "This resolved comment should stay out.".to_string(),
                    created_at: "2026-06-14T12:00:00Z".to_string(),
                    resolved: true,
                    ..PrReviewComment::default()
                },
                PrReviewComment {
                    author: "dana".to_string(),
                    path: "src/review.rs".to_string(),
                    line: "9".to_string(),
                    body: "Can this be a helper?".to_string(),
                    created_at: "2026-06-14T12:05:00Z".to_string(),
                    resolved: false,
                    ..PrReviewComment::default()
                },
            ],
            files: vec!["src/lib.rs".to_string()],
            failing_checks: vec!["cargo test".to_string()],
            ci_failures: Vec::new(),
        });

        let prompt = build_review_fix_prompt(&session, &test_config()).unwrap();

        assert!(prompt.starts_with(
            "Here are review comments on PR 123.\n\nIf they are applicable, fix them. Otherwise, say why not.\n\n---\n\n"
        ));
        assert!(prompt.contains("Inline review comments:"));
        assert!(prompt.contains("- src/review.rs line 9 by dana\n\nCan this be a helper?"));
        assert!(prompt.contains("Review bodies:"));
        assert!(prompt.contains(
            "- Review from bob (CHANGES_REQUESTED)\n\nThis should mention the fallback behavior."
        ));
        assert!(prompt.contains("PR comments:"));
        assert!(prompt.contains("- Comment from alice\n\nPlease simplify this branch."));
        assert!(prompt.contains("\n\n---\n\n"));
        assert!(prompt.contains("Can this be a helper?"));
        assert!(!prompt.contains("This resolved comment should stay out."));
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

    #[test]
    fn review_fix_prompt_uses_configured_template() {
        let mut config = test_config();
        config.prompt_templates.insert(
            "review_fix".to_string(),
            "Fix PR {pr_number} on {branch}:\n{comments}".to_string(),
        );
        let session = test_session(PrDetails {
            review_comments: vec![PrReviewComment {
                author: "dana".to_string(),
                path: "src/review.rs".to_string(),
                line: "9".to_string(),
                body: "Can this be a helper?".to_string(),
                created_at: "2026-06-14T12:05:00Z".to_string(),
                resolved: false,
                ..PrReviewComment::default()
            }],
            ..PrDetails::default()
        });

        let prompt = build_review_fix_prompt(&session, &config).unwrap();

        assert!(prompt.starts_with("Fix PR 123 on feature:"));
        assert!(prompt.contains("- src/review.rs line 9 by dana\n\nCan this be a helper?"));
    }

    #[test]
    fn actionable_feedback_filters_resolved_old_and_author_mismatches() {
        let details = PrDetails {
            comments: vec![
                PrComment {
                    author: "bot".to_string(),
                    body: "new top-level".to_string(),
                    created_at: "2026-06-14T12:10:00Z".to_string(),
                    ..PrComment::default()
                },
                PrComment {
                    author: "human".to_string(),
                    body: "wrong author".to_string(),
                    created_at: "2026-06-14T12:11:00Z".to_string(),
                    ..PrComment::default()
                },
                PrComment {
                    author: "bot".to_string(),
                    body: "missing timestamp".to_string(),
                    ..PrComment::default()
                },
            ],
            reviews: vec![PrReview {
                author: "bot".to_string(),
                state: "COMMENTED".to_string(),
                body: "old review body".to_string(),
                submitted_at: "2026-06-14T12:00:00Z".to_string(),
                ..PrReview::default()
            }],
            review_comments: vec![
                PrReviewComment {
                    author: "bot".to_string(),
                    body: "resolved inline".to_string(),
                    created_at: "2026-06-14T12:12:00Z".to_string(),
                    resolved: true,
                    ..PrReviewComment::default()
                },
                PrReviewComment {
                    author: "bot".to_string(),
                    path: "src/lib.rs".to_string(),
                    line: "5".to_string(),
                    body: "new inline".to_string(),
                    created_at: "2026-06-14T12:13:00Z".to_string(),
                    resolved: false,
                    ..PrReviewComment::default()
                },
            ],
            ..PrDetails::default()
        };

        let feedback = actionable_review_feedback(
            &details,
            ReviewFeedbackFilter {
                after: Some("2026-06-14T12:05:00Z"),
                authors: &["bot"],
            },
        );

        assert_eq!(feedback.inline_comments.len(), 1);
        assert_eq!(feedback.inline_comments[0].body, "new inline");
        assert_eq!(feedback.pr_comments.len(), 1);
        assert_eq!(feedback.pr_comments[0].body, "new top-level");
        assert!(feedback.review_bodies.is_empty());
        assert_eq!(feedback.skipped_resolved_inline, 1);
        assert_eq!(feedback.skipped_old, 2);
        assert_eq!(feedback.skipped_author, 1);
    }

    #[test]
    fn review_fix_prompt_explains_skipped_feedback() {
        let session = test_session(PrDetails {
            review_comments: vec![PrReviewComment {
                author: "dana".to_string(),
                path: "src/review.rs".to_string(),
                line: "9".to_string(),
                body: "Already handled.".to_string(),
                created_at: "2026-06-14T12:05:00Z".to_string(),
                resolved: true,
                ..PrReviewComment::default()
            }],
            ..PrDetails::default()
        });

        let prompt = build_review_fix_prompt(&session, &test_config()).unwrap();

        assert!(prompt.contains("No actionable PR review feedback was found."));
        assert!(prompt.contains("Skipped feedback:"));
        assert!(prompt.contains("- 1 resolved inline comment(s)"));
    }

    #[test]
    fn render_template_does_not_expand_inserted_values() {
        let rendered = render_template(
            "{title} {url}",
            &[
                ("title", "Title with {url}".to_string()),
                ("url", "https://example.test/pr/123".to_string()),
            ],
        );

        assert_eq!(rendered, "Title with {url} https://example.test/pr/123");
    }

    fn test_session(details: PrDetails) -> Session {
        Session {
            repo_index: 0,
            repo_label: "repo".to_string(),
            repo_key: None,
            path: PathBuf::from("/repo/worktree"),
            path_display: "/repo/worktree".to_string(),
            branch: "feature".to_string(),
            prompt_summary: String::new(),
            classification: crate::session::SessionClassification::Work,
            visibility: 0,
            adopted: false,
            hidden: false,
            status_label: "clean".to_string(),
            agent_state: AgentState::Idle,
            opencode_status: None,
            pr: PrCache {
                summary: Some(PrSummary {
                    number: 123,
                    title: "Title".to_string(),
                    body: String::new(),
                    url: "https://example.test/pr/123".to_string(),
                    state: "OPEN".to_string(),
                    review_decision: "CHANGES_REQUESTED".to_string(),
                    requested_reviewers: Vec::new(),
                    head_ref: "feature".to_string(),
                    base_ref: "main".to_string(),
                    head_sha: "abc123".to_string(),
                    updated_at: "2026-06-14T12:00:00Z".to_string(),
                    check_status: "SUCCESS".to_string(),
                    merge_state_status: "CLEAN".to_string(),
                    comment_count: 3,
                    merged: false,
                    draft: false,
                }),
                details: Some(details),
                ..PrCache::default()
            },
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        }
    }

    fn test_config() -> Config {
        Config {
            default_agent: "opencode".to_string(),
            default_base: Some("main".to_string()),
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            opencode_port_base: 41_000,
            opencode_port_span: 1_000,
            opencode_shutdown_owned_servers: false,
            opencode_plan_plugin: false,
            escape_key: EscapeKey::EscEsc,
            merge_method: crate::config::MergeMethod::Squash,
            icon_style: crate::config::IconStyle::Unicode,
            icon_style_configured: false,
            auto: crate::config::AutoConfig::default(),
            layout: crate::config::LayoutConfig::default(),
            checks: Checks::default(),
            worktree_columns: Vec::new(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            prompt_templates: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/user.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-repo-config.toml"),
        }
    }
}
