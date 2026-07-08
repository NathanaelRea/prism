use crate::config::Config;
use crate::github::{PrDetails, PrSummary, pr_cache_excluded_branch};
use crate::session::Session;
use crate::util::empty_dash;

const DEFAULT_CI_FAILURE_TEMPLATE: &str = "Here are CI failures on PR {pr_number}.\n\nFix the failing checks. Use the log tails below as the primary clues.\n\nPR: {url}\nBranch: {branch}\nHead SHA: {head_sha}\n\n---\n\n{failures}";

pub struct CiFailurePromptInput<'a> {
    pub branch: &'a str,
    pub summary: &'a PrSummary,
    pub details: &'a PrDetails,
}

pub fn build_ci_failure_prompt(session: &Session, config: &Config) -> Result<String, String> {
    if pr_cache_excluded_branch(config, &session.branch) {
        return Err("selected branch is not treated as a PR branch".to_string());
    }
    let summary = session
        .pr
        .summary
        .as_ref()
        .ok_or_else(|| "no pull request found for selected branch".to_string())?;
    let details =
        session.pr.details.as_ref().ok_or_else(|| {
            "CI failure details are still loading; refresh and try again".to_string()
        })?;

    Ok(build_ci_failure_prompt_from_input(
        CiFailurePromptInput {
            branch: &session.branch,
            summary,
            details,
        },
        config,
    ))
}

pub fn build_ci_failure_prompt_from_input(
    input: CiFailurePromptInput<'_>,
    config: &Config,
) -> String {
    let mut failures = String::new();

    if input.details.ci_failures.is_empty() {
        if input.details.failing_checks.is_empty() {
            failures.push_str("No failing GitHub Actions checks were found.\n");
        } else {
            failures.push_str("Failing checks were reported, but run logs were not available:\n");
            for check in &input.details.failing_checks {
                failures.push_str(&format!("- {}\n", empty_dash(check)));
            }
        }
    } else {
        for failure in &input.details.ci_failures {
            failures.push_str(&format!(
                "Workflow: {}\nCheck: {}\nConclusion: {}\nRun: {}\n\n",
                empty_dash(&failure.workflow),
                empty_dash(&failure.name),
                empty_dash(&failure.conclusion),
                empty_dash(&failure.url),
            ));
            if failure.log_tail.trim().is_empty() {
                failures.push_str("No failed log tail was available.\n");
            } else {
                failures.push_str("Log tail:\n```text\n");
                failures.push_str(failure.log_tail.trim());
                failures.push_str("\n```\n");
            }
            failures.push_str("\n---\n\n");
        }
    }

    render_template(
        config
            .prompt_template("ci_failure")
            .unwrap_or(DEFAULT_CI_FAILURE_TEMPLATE),
        &[
            ("pr_number", input.summary.number.to_string()),
            ("branch", input.branch.to_string()),
            ("title", input.summary.title.clone()),
            ("url", input.summary.url.clone()),
            ("head_sha", input.summary.head_sha.clone()),
            ("check_status", input.summary.check_status.clone()),
            ("failures", failures),
        ],
    )
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
    use crate::github::{CiFailure, PrCache, PrDetails, PrSummary};

    use super::*;

    #[test]
    fn ci_failure_prompt_contains_pr_metadata_and_log_tails() {
        let session = test_session(PrDetails {
            failing_checks: vec!["test".to_string()],
            ci_failures: vec![CiFailure {
                workflow: "CI".to_string(),
                name: "test".to_string(),
                conclusion: "failure".to_string(),
                url: "https://github.com/example/repo/actions/runs/99".to_string(),
                run_id: "99".to_string(),
                log_tail: "error: expected true\nfailures: test_name".to_string(),
            }],
            ..PrDetails::default()
        });

        let prompt = build_ci_failure_prompt(&session, &test_config()).unwrap();

        assert!(prompt.starts_with("Here are CI failures on PR 123."));
        assert!(prompt.contains("PR: https://example.test/pr/123"));
        assert!(prompt.contains("Branch: feature"));
        assert!(prompt.contains("Head SHA: abc123"));
        assert!(prompt.contains("Workflow: CI"));
        assert!(prompt.contains("Check: test"));
        assert!(prompt.contains("Run: https://github.com/example/repo/actions/runs/99"));
        assert!(prompt.contains("```text\nerror: expected true\nfailures: test_name\n```"));
    }

    #[test]
    fn ci_failure_prompt_uses_configured_template() {
        let mut config = test_config();
        config.prompt_templates.insert(
            "ci_failure".to_string(),
            "Fix CI for PR {pr_number} ({check_status}):\n{failures}".to_string(),
        );
        let session = test_session(PrDetails {
            failing_checks: vec!["lint".to_string()],
            ..PrDetails::default()
        });

        let prompt = build_ci_failure_prompt(&session, &config).unwrap();

        assert!(prompt.starts_with("Fix CI for PR 123 (failed):"));
        assert!(prompt.contains("- lint"));
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
                    review_decision: "UNKNOWN".to_string(),
                    requested_reviewers: Vec::new(),
                    head_ref: "feature".to_string(),
                    base_ref: "main".to_string(),
                    head_sha: "abc123".to_string(),
                    updated_at: "2026-06-14T12:00:00Z".to_string(),
                    check_status: "failed".to_string(),
                    merge_state_status: "CLEAN".to_string(),
                    comment_count: 0,
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
            default_base: Some("main".to_string()),
            default_agent: "opencode".to_string(),
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            opencode_port_base: 41_000,
            opencode_port_span: 1_000,
            opencode_shutdown_owned_servers: false,
            opencode_plan_plugin: false,
            agent_prompt_modes: BTreeMap::new(),
            prompt_templates: BTreeMap::new(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            checks: Checks::default(),
            merge_method: crate::config::MergeMethod::Squash,
            icon_style: crate::config::IconStyle::Unicode,
            icon_style_configured: false,
            auto: crate::config::AutoConfig::default(),
            layout: crate::config::LayoutConfig::default(),
            worktree_columns: Vec::new(),
            user_path: PathBuf::from("/repo/.config/prism/config.toml"),
            repo_config_path: PathBuf::from("/repo/.prism/config.toml"),
            escape_key: EscapeKey::EscEsc,
        }
    }
}
