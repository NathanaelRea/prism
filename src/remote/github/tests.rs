use super::*;
use crate::config::Config;
use crate::observability;
use crate::remote::coordinator::{
    PrCacheEligibility, PrCacheRepository, load_pr_cache_for_branch, pr_cache_pollable_for_session,
    pr_summary_matches_worktree, refresh_pr_summary_index_for_sessions,
    resolve_pr_summary_for_session,
};
use crate::remote::migrations::{migrate_pr_cache_schema, table_has_column};
use crate::remote::store::{
    load_pr_cache, load_pr_details_cache, load_repo_policy_cache,
    load_repo_policy_cache_for_identity, load_repo_policy_cache_for_repository,
    persist_pr_cache_snapshot, persist_pr_summary_mutation, record_pr_summary, save_pr_cache,
    save_pr_details_cache, save_pr_details_cache_for_association, save_repo_policy_cache,
};
use crate::session::Session;
use crate::test_support::write_executable;
use rusqlite::params;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn migrates_existing_pr_cache_schema_additively_without_losing_rows() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "
            create table pr_cache (
              branch text primary key, number integer not null, title text not null,
              url text not null, state text not null, review_decision text not null,
              head_ref text not null, base_ref text not null, head_sha text not null,
              updated_at text not null, check_status text not null, merged integer not null,
              draft integer not null, last_refreshed text not null,
              refreshed_unix_ms integer not null
            );
            create table pr_details_cache (
              branch text primary key, comments text not null, reviews text not null,
              review_comments text not null, files text not null,
              failing_checks text not null, refreshed_unix_ms integer not null
            );
            create table repo_policy_cache (
              repo_remote text primary key, default_branch text,
              required_approvals integer not null default 0,
              require_conversation_resolution integer not null default 0,
              require_branch_up_to_date integer not null default 0,
              required_checks text not null default '[]',
              merge_queue_required integer not null default 0,
              refreshed_unix_ms integer not null, error text
            );
            insert into pr_cache values (
              'feature', 42, 'Old row', 'https://example.test/42', 'OPEN', '',
              'feature', 'main', 'head-a', '2026-01-01', 'pending', 0, 0,
              'before migration', 123
            );
            insert into pr_details_cache values (
              'feature', '[]', '[]', '[]', '[\"src/lib.rs\"]', '[]', 123
            );
            insert into pr_cache values (
              'github-feature', 43, 'GitHub row', 'https://github.com/acme/widgets/pull/43',
              'OPEN', '', 'github-feature', 'main', 'head-b', '2026-01-02', 'pending',
              0, 0, 'before migration', 124
            );
            insert into pr_details_cache values (
              'github-feature', '[]', '[]', '[]', '[\"src/main.rs\"]', '[]', 124
            );
            insert into repo_policy_cache values (
              'acme/widgets', 'main', 1, 1, 1, '[\"ci\"]', 0, 125, null
            );
            ",
    )
    .unwrap();

    migrate_pr_cache_schema(&conn).unwrap();
    migrate_pr_cache_schema(&conn).unwrap();

    assert!(table_has_column(&conn, "pr_cache", "body").unwrap());
    assert!(table_has_column(&conn, "pr_cache", "observation_error").unwrap());
    assert!(table_has_column(&conn, "pr_details_cache", "pr_number").unwrap());
    assert!(table_has_column(&conn, "pr_details_cache", "head_sha").unwrap());
    assert!(table_has_column(&conn, "pr_cache", "native_cr_id").unwrap());
    assert!(table_has_column(&conn, "pr_cache", "identity_complete").unwrap());
    assert!(table_has_column(&conn, "pr_cache", "native_state_evidence").unwrap());
    assert!(table_has_column(&conn, "pr_details_cache", "target_project_path").unwrap());
    assert!(table_has_column(&conn, "repo_policy_cache", "target_branch").unwrap());
    let old_row = conn
        .query_row(
            "select title, body, comment_count from pr_cache where branch = 'feature'",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(old_row, ("Old row".to_string(), String::new(), 0));
    assert_eq!(
        conn.query_row(
            "select native_state_evidence from pr_cache where branch = 'feature'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap(),
        "{}"
    );
    let association = conn
        .query_row(
            "select pr_number, head_sha from pr_details_cache where branch = 'feature'",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(association, (None, None));
    let github_identity = conn
        .query_row(
            "select provider, canonical_host, project_path, native_cr_id, display_number,
                        source_project_path, target_project_path, identity_complete
                   from pr_cache where branch = 'github-feature'",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        github_identity,
        (
            Some("github".to_string()),
            Some("github.com".to_string()),
            Some("acme/widgets".to_string()),
            None,
            Some(43),
            None,
            Some("acme/widgets".to_string()),
            0,
        )
    );
    let details_identity = conn
        .query_row(
            "select provider, target_project_path, identity_complete
                   from pr_details_cache where branch = 'github-feature'",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        details_identity,
        (
            Some("github".to_string()),
            Some("acme/widgets".to_string()),
            0,
        )
    );
    let policy_identity = conn
        .query_row(
            "select provider, canonical_host, project_path, target_branch, identity_complete
                   from repo_policy_cache where repo_remote = 'acme/widgets'",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        policy_identity,
        (
            Some("github".to_string()),
            Some("github.com".to_string()),
            Some("acme/widgets".to_string()),
            Some("main".to_string()),
            1,
        )
    );
}

#[test]
fn migration_normalizes_and_deduplicates_github_policy_identity_keys_only() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
            "
            create table repo_policy_cache_v2 (
              provider text not null,
              canonical_host text not null,
              project_path text not null,
              target_branch text not null,
              repo_remote text not null,
              default_branch text,
              required_approvals integer not null default 0,
              require_conversation_resolution integer not null default 0,
              require_branch_up_to_date integer not null default 0,
              required_checks text not null default '[]',
              merge_queue_required integer not null default 0,
              refreshed_unix_ms integer not null,
              error text,
              primary key (provider, canonical_host, project_path, target_branch)
            );
            insert into repo_policy_cache_v2 values
              ('github', 'github.com', 'acme/widget', 'main', 'acme/widget', 'main', 1, 0, 0, '[]', 0, 10, null),
              ('github', 'github.com', 'Acme/Widget', 'main', 'Acme/Widget', 'main', 2, 0, 0, '[]', 0, 20, null),
              ('gitlab', 'gitlab.com', 'acme/widget', 'main', 'acme/widget', 'main', 3, 0, 0, '[]', 0, 30, null),
              ('gitlab', 'gitlab.com', 'Acme/Widget', 'main', 'Acme/Widget', 'main', 4, 0, 0, '[]', 0, 40, null);
            ",
        )
        .unwrap();

    migrate_pr_cache_schema(&conn).unwrap();
    migrate_pr_cache_schema(&conn).unwrap();

    let github = conn
        .query_row(
            "select count(*), project_path, project_path_key, required_approvals
                   from repo_policy_cache_v2 where provider = 'github'",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        github,
        (1, "Acme/Widget".to_string(), "acme/widget".to_string(), 2)
    );
    assert_eq!(
        conn.query_row(
            "select count(*) from repo_policy_cache_v2 where provider = 'gitlab'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        2
    );
}

#[test]
fn direct_and_index_summary_paths_produce_equivalent_cache_facts() {
    let temp = unique_temp_dir("prism-pr-equivalent-summary-paths");
    fs::create_dir_all(&temp).unwrap();
    let direct_repo =
        Repository::with_config_dir_for_test(temp.join("direct-repo"), temp.join("direct-config"));
    let index_repo =
        Repository::with_config_dir_for_test(temp.join("index-repo"), temp.join("index-config"));
    let config = test_config();
    let identity = test_identity(
        crate::remote::ProviderKind::GitHub,
        "github.com",
        "example/repo",
        "PR_equivalent",
    );
    let old_summary = PrSummary {
        change_request_identity: Some(identity.clone()),
        ..test_summary("feature", "head-a", 1)
    };
    let new_summary = PrSummary {
        change_request_identity: Some(identity),
        ..test_summary("feature", "head-a", 2)
    };
    let details = PrDetails {
        comments: vec![PrComment {
            body: "preserved".to_string(),
            ..PrComment::default()
        }],
        ..PrDetails::default()
    };
    let mut direct = PrCache::observed(old_summary.clone(), Some(details.clone()));
    record_pr_summary(&direct_repo, "feature", &mut direct, new_summary.clone());

    let poll_started_at = Instant::now();
    let mut sessions = vec![test_session(
        "feature",
        PrCache::observed(old_summary, Some(details)),
    )];
    sessions[0].pr.begin_summary_poll(poll_started_at);
    refresh_pr_summary_index_for_sessions(
        &[PrCacheRepository {
            repo: &index_repo,
            config: &config,
        }],
        &mut sessions,
        0,
        vec![new_summary.clone()],
        poll_started_at,
    );

    assert_eq!(direct.summary(), Some(&new_summary));
    assert_eq!(sessions[0].pr.summary(), direct.summary());
    assert_eq!(
        sessions[0].pr.details().unwrap().comments[0].body,
        direct.details().unwrap().comments[0].body
    );
    assert!(direct.trusted_summary_and_details().is_ok());
    assert!(sessions[0].pr.trusted_summary_and_details().is_ok());

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn canonical_change_request_or_head_changes_invalidate_cached_details() {
    let base_summary = PrSummary {
        change_request_identity: Some(test_identity(
            crate::remote::ProviderKind::GitHub,
            "github.com",
            "example/repo",
            "PR_one",
        )),
        ..test_summary("feature", "head-a", 1)
    };
    let details = PrDetails {
        comments: vec![PrComment {
            body: "cached".to_string(),
            ..PrComment::default()
        }],
        ..PrDetails::default()
    };
    let changed_identities = [
        test_identity(
            crate::remote::ProviderKind::GitLab,
            "github.com",
            "example/repo",
            "PR_one",
        ),
        test_identity(
            crate::remote::ProviderKind::GitHub,
            "github.example.com",
            "example/repo",
            "PR_one",
        ),
        test_identity(
            crate::remote::ProviderKind::GitHub,
            "github.com",
            "example/other",
            "PR_one",
        ),
        test_identity(
            crate::remote::ProviderKind::GitHub,
            "github.com",
            "example/repo",
            "PR_two",
        ),
    ];

    for identity in changed_identities {
        let mut cache = PrCache::observed(base_summary.clone(), Some(details.clone()));
        cache.record_summary_observation(
            Some(PrSummary {
                change_request_identity: Some(identity),
                ..base_summary.clone()
            }),
            "now".to_string(),
        );
        assert!(cache.details().is_none());
    }

    let mut cache = PrCache::observed(base_summary.clone(), Some(details));
    cache.record_summary_observation(
        Some(PrSummary {
            head_sha: "head-b".to_string(),
            ..base_summary
        }),
        "now".to_string(),
    );
    assert!(cache.details().is_none());
}

#[test]
fn create_pr_uses_fill_with_explicit_empty_body_and_default_base_when_configured() {
    assert_eq!(
        create_pr_args(Some("main"), "", None, None),
        vec!["pr", "create", "--fill", "--body", "", "--base", "main"]
    );
    assert_eq!(
        create_pr_args(None, "manual description", None, None),
        vec!["pr", "create", "--fill", "--body", "manual description"]
    );
    assert_eq!(
        create_pr_args(
            Some("main"),
            "manual description",
            Some("owner/repo"),
            Some("contributor:topic"),
        ),
        vec![
            "pr",
            "create",
            "--fill",
            "--body",
            "manual description",
            "--repo",
            "owner/repo",
            "--base",
            "main",
            "--head",
            "contributor:topic"
        ]
    );
}

#[test]
fn merge_pr_args_use_configured_method() {
    assert_eq!(
        merge_pr_args("42", MergeMethod::Squash, "abc123", None),
        vec![
            "pr",
            "merge",
            "42",
            "--squash",
            "--match-head-commit",
            "abc123"
        ]
    );
    assert_eq!(
        merge_pr_args("42", MergeMethod::Merge, "abc123", None),
        vec![
            "pr",
            "merge",
            "42",
            "--merge",
            "--match-head-commit",
            "abc123"
        ]
    );
    assert_eq!(
        merge_pr_args("42", MergeMethod::Rebase, "abc123", None),
        vec![
            "pr",
            "merge",
            "42",
            "--rebase",
            "--match-head-commit",
            "abc123"
        ]
    );
}

#[test]
fn merge_pull_request_does_not_delegate_branch_deletion_to_gh() {
    let temp = unique_temp_dir("prism-merge-no-delete-branch-test");
    let worktree = temp.join("worktree");
    fs::create_dir_all(&worktree).unwrap();
    let log = temp.join("gh.log");
    let gh = temp.join("gh");
    write_executable(
        &gh,
        &format!(
            r#"#!/bin/sh
printf 'pwd=%s\nargs=%s\n' "$PWD" "$*" > '{}'
exit 0
"#,
            log.display()
        ),
    );

    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());

    merge_pull_request(&config, &worktree, 42, "abc123", None).unwrap();

    let commands = fs::read_to_string(&log).unwrap();
    let actual_pwd = commands
        .lines()
        .find_map(|line| line.strip_prefix("pwd="))
        .expect("gh shim should record its working directory");
    assert_eq!(
        PathBuf::from(actual_pwd).canonicalize().unwrap(),
        worktree.canonicalize().unwrap()
    );
    assert!(commands.contains("args=pr merge 42 --squash --match-head-commit abc123"));
    assert!(!commands.contains("--delete-branch"));

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn pr_json_parser_reads_summary_details_and_missing_fields() {
    let raw = r#"{
            "number": 42,
            "title": "Fix review",
            "mergedAt": "2026-01-01T00:00:00Z",
            "isDraft": true,
            "comments": [{
                "id": "PRC_kw123",
                "author": {"login": "reviewer"},
                "body": "hello",
                "createdAt": "2026-01-01T00:00:00Z"
            }],
            "reviews": [{
                "id": "PRR_kw123",
                "author": {"login": "maintainer"},
                "state": "CHANGES_REQUESTED",
                "body": "review body",
                "submittedAt": "2026-01-01T00:01:00Z"
            }],
            "files": [{"path": "src/main.rs"}],
            "statusCheckRollup": {
                "contexts": {
                    "nodes": [{"name": "test", "status": "COMPLETED", "conclusion": "FAILURE"}]
                }
            }
        }"#;
    assert!(parse_merged_status(raw));
    assert_eq!(parse_check_status(raw), "failed");
    let details = parse_pr_details(raw);
    assert_eq!(details.files, vec!["src/main.rs"]);
    assert_eq!(details.failing_checks, vec!["test"]);
    assert_eq!(details.check_contexts[0].name, "test");
    assert_eq!(details.check_contexts[0].state, PrCheckState::Failed);
    assert_eq!(details.comments[0].id, "PRC_kw123");
    assert_eq!(details.comments[0].body, "hello");
    assert_eq!(details.comments[0].created_at, "2026-01-01T00:00:00Z");
    assert_eq!(details.reviews[0].id, "PRR_kw123");
    assert_eq!(details.reviews[0].state, "CHANGES_REQUESTED");
    assert_eq!(details.reviews[0].body, "review body");
    assert_eq!(details.reviews[0].submitted_at, "2026-01-01T00:01:00Z");
}

#[test]
fn paginated_rest_reviews_accept_numeric_github_ids() {
    let pages = serde_json::from_str::<Vec<Vec<GhPrReview>>>(
        r#"[[{"id":4833758968,"user":{"login":"maintainer"},"state":"APPROVED"}]]"#,
    )
    .unwrap();

    let reviews = parse_gh_reviews(&pages.into_iter().flatten().collect::<Vec<_>>());
    assert_eq!(reviews[0].id, "4833758968");
    assert_eq!(reviews[0].author, "maintainer");
    assert_eq!(reviews[0].state, "APPROVED");
}

#[test]
fn paginated_rest_comments_accept_numeric_github_ids() {
    let pages = serde_json::from_str::<Vec<Vec<GhPrComment>>>(
        r#"[[{"id":2195515127,"user":{"login":"reviewer"},"body":"Looks good"}]]"#,
    )
    .unwrap();

    let comments = parse_gh_comments(&pages.into_iter().flatten().collect::<Vec<_>>());
    assert_eq!(comments[0].id, "2195515127");
    assert_eq!(comments[0].author, "reviewer");
    assert_eq!(comments[0].body, "Looks good");
}

#[test]
fn empty_check_rollup_is_authoritative_no_ci_but_missing_rollup_is_unknown() {
    for rollup in ["[]", "null", r#"{"contexts":{"nodes":[]}}"#] {
        let raw = format!(
            r#"{{
                    "number": 42,
                    "state": "OPEN",
                    "statusCheckRollup": {rollup}
                }}"#
        );
        let node = serde_json::from_str::<GithubPullRequest>(&raw).unwrap();
        let summary = pr_summary_from_node(&node, None).unwrap();

        assert_eq!(summary.check_state(), PrCheckState::Success);
    }

    let node =
        serde_json::from_str::<GithubPullRequest>(r#"{"number":42,"state":"OPEN"}"#).unwrap();
    let summary = pr_summary_from_node(&node, None).unwrap();
    assert_eq!(summary.check_state(), PrCheckState::Unknown);
}

#[test]
fn malformed_or_truncated_check_rollup_is_unknown_evidence() {
    for raw in [
        r#"{"number":42,"state":"OPEN","statusCheckRollup":{}}"#,
        r#"{"number":42,"state":"OPEN","statusCheckRollup":{"contexts":{}}}"#,
        r#"{"number":42,"state":"OPEN","statusCheckRollup":{"contexts":{"pageInfo":{"hasNextPage":true},"nodes":[]}}}"#,
    ] {
        assert!(serde_json::from_str::<GithubPullRequest>(raw).is_err());
    }
    let capped = serde_json::json!({
        "number": 42,
        "state": "OPEN",
        "statusCheckRollup": vec![serde_json::json!({"name": "check"}); 100]
    });
    assert!(serde_json::from_value::<GithubPullRequest>(capped).is_err());
}

#[test]
fn check_state_normalizes_display_labels_for_workflow_decisions() {
    assert_eq!(PrCheckState::from_label("running"), PrCheckState::Pending);
    assert_eq!(PrCheckState::from_label("pending"), PrCheckState::Pending);
    assert_eq!(PrCheckState::from_label("passed"), PrCheckState::Success);
    assert_eq!(PrCheckState::from_label("success"), PrCheckState::Success);
    assert_eq!(PrCheckState::from_label("failed"), PrCheckState::Failed);
    assert_eq!(PrCheckState::from_label("mixed"), PrCheckState::Mixed);
    assert_eq!(PrCheckState::from_label(""), PrCheckState::Unknown);
}

#[test]
fn rest_check_failures_are_detected_case_insensitively() {
    let contexts = vec![
        GithubStatusContext {
            name: Some("check-run".to_string()),
            conclusion: Some("failure".to_string()),
            ..GithubStatusContext::default()
        },
        GithubStatusContext {
            context: Some("commit-status".to_string()),
            state: Some("error".to_string()),
            ..GithubStatusContext::default()
        },
    ];

    assert_eq!(
        collect_failing_checks_from_contexts(&contexts),
        ["check-run", "commit-status"]
    );
}

#[test]
fn resolve_review_thread_args_target_exact_thread_id() {
    let host = crate::remote::HostIdentity::new("github.example.com", None).unwrap();
    let config = crate::test_support::test_config();
    let args = resolve_review_thread_args(&config, &host, "PRRT_thread_1");

    assert_eq!(args[0], "api");
    assert_eq!(args[1], "graphql");
    assert!(
        args.windows(2)
            .any(|pair| { pair == ["--hostname".to_string(), "github.example.com".to_string()] })
    );
    assert!(args.contains(&"thread=PRRT_thread_1".to_string()));
    assert!(
        args.iter()
            .any(|arg| arg.contains("resolveReviewThread") && arg.contains("threadId: $thread"))
    );
}

#[test]
fn configured_api_override_uses_full_rest_and_graphql_endpoints_with_canonical_host() {
    let host = crate::remote::HostIdentity::new("github.example.com", None).unwrap();
    let mut config = crate::test_support::test_config();
    config.remote_hosts.insert(
        "github.example.com".to_string(),
        crate::config::RemoteHostConfig {
            provider: crate::remote::ProviderKind::GitHub,
            web_url: None,
            api_url: Some("https://broker.example.com/github/api/v3".to_string()),
            credential_env: None,
            allow_http: false,
        },
    );

    let graphql = github_graphql_api_args(&config, &host);
    let rest = github_api_endpoint(&config, &host, "/repos/Acme/Widget");

    assert_eq!(graphql[1], "https://broker.example.com/github/api/graphql");
    assert!(
        graphql
            .windows(2)
            .any(|pair| { pair == ["--hostname".to_string(), "github.example.com".to_string()] })
    );
    assert_eq!(
        rest,
        "https://broker.example.com/github/api/v3/repos/Acme/Widget"
    );
}

#[cfg(unix)]
#[test]
fn ghes_summary_graphql_uses_the_canonical_hostname() {
    let temp = unique_temp_dir("prism-ghes-summary-host");
    fs::create_dir_all(&temp).unwrap();
    let gh = temp.join("gh");
    let log = temp.join("gh.log");
    write_executable(
        &gh,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nprintf '%s\\n' '{{\"data\":{{\"repository\":{{\"pullRequests\":{{\"nodes\":[],\"pageInfo\":{{\"hasNextPage\":false}}}}}}}}}}'\n",
            log.display()
        ),
    );
    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    let repository = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.example.com", None).unwrap(),
        "Acme/Widget",
    )
    .unwrap();

    assert!(
        fetch_pr_summary_index_for_repository(&temp, &config, &repository)
            .unwrap()
            .is_empty()
    );
    let command = fs::read_to_string(log).unwrap();
    assert!(command.contains("api graphql --hostname github.example.com"));
    assert!(command.contains("owner=Acme"));
    assert!(command.contains("name=Widget"));
    assert!(command.contains("states: OPEN"));

    fs::remove_dir_all(temp).unwrap();
}

#[cfg(unix)]
#[test]
fn exact_summary_observation_queries_only_the_requested_number() {
    let temp = unique_temp_dir("prism-github-exact-summary");
    fs::create_dir_all(&temp).unwrap();
    let gh = temp.join("gh");
    let log = temp.join("gh.log");
    write_executable(
        &gh,
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" > '{}'
printf '%s\n' '{{"data":{{"repository":{{"pullRequest":{{"id":"PR_42","number":42,"state":"MERGED","headRefName":"feature","baseRefName":"main","headRefOid":"head","headRepository":{{"nameWithOwner":"Acme/Widget"}},"baseRepository":{{"nameWithOwner":"Acme/Widget"}},"merged":true}}}}}}}}'
"#,
            log.display()
        ),
    );
    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    let repository = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.com", None).unwrap(),
        "Acme/Widget",
    )
    .unwrap();

    let summary = fetch_pr_summary_for_repository_number(&temp, &config, &repository, 42)
        .unwrap()
        .unwrap();

    assert_eq!(summary.number, 42);
    assert!(summary.merged);
    let command = fs::read_to_string(log).unwrap();
    assert!(command.contains("number=42"));
    assert!(command.contains("pullRequest(number: $number)"));
    assert!(!command.contains("pullRequests("));
    fs::remove_dir_all(temp).unwrap();
}

#[cfg(unix)]
#[test]
fn ghes_review_thread_mutation_uses_the_canonical_hostname() {
    let temp = unique_temp_dir("prism-ghes-thread-host");
    fs::create_dir_all(&temp).unwrap();
    let gh = temp.join("gh");
    let log = temp.join("gh.log");
    write_executable(
        &gh,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nprintf '%s\\n' '{{\"data\":{{\"resolveReviewThread\":{{\"thread\":{{\"id\":\"PRRT_1\",\"isResolved\":true}}}}}}}}'\n",
            log.display()
        ),
    );
    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    let host = crate::remote::HostIdentity::new("github.example.com", None).unwrap();

    resolve_review_thread(&temp, &config, &host, "PRRT_1").unwrap();

    assert!(
        fs::read_to_string(log)
            .unwrap()
            .contains("api graphql --hostname github.example.com")
    );
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn phase_1_failed_forced_summary_keeps_stale_display_but_authoritative_access_errors() {
    let temp = unique_temp_dir("prism-phase-1-failed-summary-refresh");
    fs::create_dir_all(&temp).unwrap();
    let gh = temp.join("gh");
    write_executable(&gh, "#!/bin/sh\necho 'GitHub unavailable' >&2\nexit 1\n");
    let git = temp.join("git");
    write_executable(
        &git,
        "#!/bin/sh\ncase \"$*\" in *\"remote get-url origin\"*) echo git@github.com:owner/repo.git ;; esac\n",
    );
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let stale_summary = test_summary("feature", "head-a", 2);
    let stale_details = PrDetails {
        files: vec!["src/stale.rs".to_string()],
        ..PrDetails::default()
    };
    let mut cache = PrCache::observed(stale_summary.clone(), Some(stale_details));
    cache.record_summary_observation(Some(stale_summary.clone()), "before failure".to_string());

    assert!(refresh_pr_cache(&repo, "feature", &mut cache, &temp, &config, true).is_err());

    assert_eq!(cache.summary(), Some(&stale_summary));
    assert_eq!(cache.details().unwrap().files, vec!["src/stale.rs"]);
    assert_eq!(cache.last_refreshed(), Some("before failure"));
    assert!(cache.display_error().is_some_and(|error| !error.is_empty()));
    assert!(pr_summary_or_error(&cache).is_err());

    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn phase_1_details_for_head_a_are_rejected_after_same_pr_advances_to_head_b() {
    let temp = unique_temp_dir("prism-phase-1-stale-head-details");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let head_a = test_summary("feature", "head-a", 0);
    let mut cache = PrCache::observed(head_a.clone(), None);
    let mut poll_result = cache.begin_details_poll();
    let mut observation = successful_details_observation_for(&head_a);
    observation.review_comments = Ok(vec![PrReviewComment {
        thread_id: "PRRT_from_head_a".to_string(),
        body: "stale".to_string(),
        ..PrReviewComment::default()
    }]);
    poll_result.record_details_observation(observation);
    cache.record_summary_observation(
        Some(test_summary("feature", "head-b", 0)),
        "advanced".to_string(),
    );

    let applied = record_pr_details_poll_result(&repo, "feature", &mut cache, poll_result);

    assert!(!applied);
    assert!(cache.details().is_none());
    assert!(load_pr_details_cache(&repo, "feature").is_none());

    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn phase_1_malformed_github_summary_output_is_failure_not_authoritative_absence() {
    let temp = unique_temp_dir("prism-phase-1-malformed-summary");
    fs::create_dir_all(&temp).unwrap();
    let gh = temp.join("gh");
    write_executable(&gh, "#!/bin/sh\nprintf '{not valid json'\n");
    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());

    let result = fetch_pr_summary(&temp, "feature", &config);

    assert!(
        result.is_err(),
        "malformed output must not mean no pull request"
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn pr_cache_round_trips_details() {
    let temp = unique_temp_dir("prism-pr-details-cache-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let summary = PrSummary {
        number: 42,
        change_request_identity: Some(crate::remote::test_change_request_identity()),
        native_state_evidence: crate::remote::NativeStateEvidence {
            lifecycle: vec!["OPEN".to_string()],
            review: vec!["CHANGES_REQUESTED".to_string()],
            mergeability: vec!["CLEAN".to_string()],
            check: vec!["COMPLETED".to_string(), "FAILURE".to_string()],
            queue: vec!["PREPARING".to_string()],
        },
        title: "Fix review".to_string(),
        author: "author".to_string(),
        body: "Body with \"quotes\"".to_string(),
        url: "https://github.com/example/repo/pull/42".to_string(),
        state: "OPEN".to_string(),
        review_decision: "CHANGES_REQUESTED".to_string(),
        requested_reviewers: vec!["alice".to_string()],
        head_ref: "feature".to_string(),
        base_ref: "main".to_string(),
        head_sha: "abc123".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        check_status: "failed".to_string(),
        merge_state_status: "CLEAN".to_string(),
        queue_state: "preparing_merged_result".to_string(),
        comment_count: 2,
        merged: false,
        draft: false,
    };
    let details = PrDetails {
        comments: vec![PrComment {
            author: "reviewer".to_string(),
            body: "please fix\nthis".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            ..PrComment::default()
        }],
        reviews: vec![PrReview {
            author: "maintainer".to_string(),
            state: "CHANGES_REQUESTED".to_string(),
            body: "needs work".to_string(),
            submitted_at: "2026-01-01T00:01:00Z".to_string(),
            ..PrReview::default()
        }],
        review_comments: vec![PrReviewComment {
            author: "reviewer".to_string(),
            path: "src/main.rs".to_string(),
            line: "12".to_string(),
            body: "inline note".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            resolved: true,
            ..PrReviewComment::default()
        }],
        files: vec!["src/main.rs".to_string()],
        failing_checks: vec!["test".to_string()],
        check_contexts: vec![PrCheckContext {
            name: "test".to_string(),
            state: PrCheckState::Failed,
        }],
        ci_failures: vec![CiFailure {
            workflow: "CI".to_string(),
            name: "test".to_string(),
            conclusion: "failure".to_string(),
            url: "https://github.com/example/repo/actions/runs/99".to_string(),
            run_id: "99".to_string(),
            log_tail: "failed log".to_string(),
        }],
    };
    let mut cache = PrCache::observed(summary, Some(details));
    let observed = cache.summary().cloned();
    cache.record_summary_observation(observed, "now".to_string());

    save_pr_cache(&repo, "feature", &cache).unwrap();
    save_pr_details_cache(&repo, "feature", cache.details().unwrap()).unwrap();
    let loaded = load_pr_cache(&repo, "feature");
    assert_eq!(
        loaded.summary().unwrap().queue_state,
        "preparing_merged_result"
    );
    assert_eq!(
        loaded.summary().unwrap().native_state_evidence,
        cache.summary().unwrap().native_state_evidence
    );
    let prism_dir = repo.prism_dir();

    assert_eq!(loaded.summary().unwrap().number, 42);
    assert_eq!(loaded.summary().unwrap().merge_state_status, "CLEAN");
    let loaded_details = loaded.details().unwrap();
    assert_eq!(loaded_details.comments[0].author, "reviewer");
    assert_eq!(loaded_details.comments[0].body, "please fix\nthis");
    assert_eq!(
        loaded_details.comments[0].created_at,
        "2026-01-01T00:00:00Z"
    );
    assert_eq!(loaded_details.reviews[0].state, "CHANGES_REQUESTED");
    assert_eq!(
        loaded_details.reviews[0].submitted_at,
        "2026-01-01T00:01:00Z"
    );
    assert_eq!(loaded_details.review_comments[0].path, "src/main.rs");
    assert!(loaded_details.review_comments[0].resolved);
    assert_eq!(loaded_details.files, vec!["src/main.rs"]);
    assert_eq!(loaded_details.failing_checks, vec!["test"]);
    assert_eq!(loaded_details.check_contexts[0].name, "test");
    assert_eq!(loaded_details.check_contexts[0].state, PrCheckState::Failed);
    assert_eq!(loaded_details.ci_failures[0].log_tail, "");

    let _ = fs::remove_dir_all(prism_dir);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn restart_accepts_only_details_associated_with_persisted_pr_and_head() {
    let temp = unique_temp_dir("prism-pr-details-association-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let summary = test_summary("feature", "head-a", 1);
    let details = PrDetails {
        comments: vec![PrComment {
            body: "associated".to_string(),
            ..PrComment::default()
        }],
        ..PrDetails::default()
    };
    let mut cache = PrCache::observed(summary.clone(), Some(details.clone()));
    cache.record_summary_observation(Some(summary.clone()), "now".to_string());
    save_pr_cache(&repo, "feature", &cache).unwrap();
    save_pr_details_cache(&repo, "feature", &details).unwrap();

    let associated = load_pr_cache(&repo, "feature");
    assert_eq!(
        associated.details_observation_quality(),
        PrObservationQuality::PreservedStale
    );
    assert!(associated.trusted_details().is_err());

    let moved = PrCache::observed(test_summary("feature", "head-b", 1), None);
    save_pr_cache(&repo, "feature", &moved).unwrap();
    let stale = load_pr_cache(&repo, "feature");
    assert!(stale.details().is_none());

    save_pr_cache(&repo, "feature", &cache).unwrap();
    observability::with_writable_db(&repo, |conn| {
        conn.execute(
            "update pr_details_cache set pr_number = null, head_sha = null where branch = ?1",
            params!["feature"],
        )
        .map_err(|error| error.to_string())?;
        Ok(())
    })
    .unwrap();
    let mut legacy = load_pr_cache(&repo, "feature");
    assert!(legacy.details().is_some());
    assert_eq!(
        legacy.details_observation_quality(),
        PrObservationQuality::PreservedStale
    );
    assert!(legacy.trusted_details().is_err());
    let mutation =
        legacy.record_summary_observation(Some(summary.clone()), "refreshed".to_string());
    persist_pr_summary_mutation(&repo, "feature", &mut legacy, mutation);
    assert!(load_pr_cache(&repo, "feature").details.is_none());

    save_pr_details_cache_for_association(
        &repo,
        "feature",
        &details,
        &PrDetailsAssociation::from_summary(&summary),
        &["review threads: unavailable".to_string()],
        &[],
    )
    .unwrap();
    let partial = load_pr_cache(&repo, "feature");
    assert_eq!(
        partial.details_observation_quality(),
        PrObservationQuality::PreservedStale
    );
    assert!(partial.trusted_details().is_err());

    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn successful_details_write_does_not_clear_previous_persistence_failure() {
    let temp = unique_temp_dir("prism-pr-persistence-error-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut cache = cache_with_observed_details();
    cache.record_persistence_result(Err("summary write failed".to_string()));
    save_pr_cache(&repo, "feature", &cache).unwrap();
    let poll_result = cache.begin_details_poll();

    assert!(record_pr_details_poll_result(
        &repo,
        "feature",
        &mut cache,
        poll_result,
    ));

    assert_eq!(cache.display_error(), Some("summary write failed"));
    assert!(cache.trusted_details().is_err());

    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn obsolete_details_generation_is_rejected_for_same_pr_and_head() {
    let temp = unique_temp_dir("prism-obsolete-details-generation-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut cache = cache_with_observed_details();
    let obsolete = cache.begin_details_poll();
    let _current = cache.begin_details_poll();

    assert!(!record_pr_details_poll_result(
        &repo, "feature", &mut cache, obsolete,
    ));
    assert_eq!(cache.details().unwrap().comments[0].body, "old comment");

    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn pr_summary_refresh_preserves_details_when_signature_matches() {
    let summary = test_summary("feature", "abc123", 2);
    let details = PrDetails {
        review_comments: vec![PrReviewComment {
            author: "reviewer".to_string(),
            path: "src/main.rs".to_string(),
            line: "12".to_string(),
            body: "inline note".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            resolved: false,
            ..PrReviewComment::default()
        }],
        ..PrDetails::default()
    };
    let mut cache = PrCache::observed(summary.clone(), Some(details));
    cache.record_summary_failure("previous error".to_string());

    cache.record_summary_observation(Some(summary), "now".to_string());

    assert!(cache.details().is_some());
    assert!(cache.display_error().is_none());
    assert_eq!(cache.last_refreshed(), Some("now"));
}

#[test]
fn pr_summary_refresh_drops_details_when_signature_changes() {
    let old_summary = test_summary("feature", "abc123", 2);
    let new_summary = test_summary("feature", "def456", 2);
    let mut cache = PrCache::observed(old_summary, Some(PrDetails::default()));

    cache.record_summary_observation(Some(new_summary.clone()), "now".to_string());

    assert_eq!(cache.summary(), Some(&new_summary));
    assert!(cache.details().is_none());
}

#[test]
fn summary_refresh_preserves_details_when_pr_and_head_are_unchanged() {
    let old_summary = test_summary("feature", "abc123", 2);
    let mut new_summary = old_summary.clone();
    new_summary.review_decision = "APPROVED".to_string();
    new_summary.updated_at = "2026-01-02T00:00:00Z".to_string();
    let details = PrDetails {
        comments: vec![PrComment {
            body: "keep me".to_string(),
            ..PrComment::default()
        }],
        ..PrDetails::default()
    };
    let mut cache = PrCache::observed(old_summary, Some(details));

    cache.record_summary_observation(Some(new_summary), "now".to_string());

    assert_eq!(cache.details().unwrap().comments[0].body, "keep me");
    assert!(cache.trusted_details().is_ok());
}

fn cache_with_observed_details() -> PrCache {
    let summary = test_summary("feature", "abc123", 2);
    PrCache::observed(
        summary,
        Some(PrDetails {
            comments: vec![PrComment {
                body: "old comment".to_string(),
                ..PrComment::default()
            }],
            review_comments: vec![PrReviewComment {
                thread_id: "old-thread".to_string(),
                ..PrReviewComment::default()
            }],
            failing_checks: vec!["old-check".to_string()],
            check_contexts: vec![PrCheckContext {
                name: "old-check".to_string(),
                state: PrCheckState::Failed,
            }],
            ci_failures: vec![CiFailure {
                run_id: "old-run".to_string(),
                log_tail: "old log".to_string(),
                ..CiFailure::default()
            }],
            ..PrDetails::default()
        }),
    )
}

fn successful_details_observation_for(summary: &PrSummary) -> PrDetailsObservation {
    PrDetailsObservation {
        association: PrDetailsAssociation::from_summary(summary),
        comments: Ok(Vec::new()),
        reviews: Ok(Vec::new()),
        review_comments: Ok(Vec::new()),
        files: Ok(Vec::new()),
        failing_checks: Ok(Vec::new()),
        check_contexts: Ok(Vec::new()),
        ci_failures: Ok(Vec::new()),
        partial_errors: Vec::new(),
    }
}

#[test]
fn partial_comment_failure_preserves_previous_comments() {
    let (temp, repo, mut cache, summary) = persisted_cache_with_observed_details();
    let mut observation = successful_details_observation_for(&summary);
    observation.comments = Err("comments unavailable".to_string());

    assert!(record_pr_details_observation(
        &repo,
        "feature",
        &mut cache,
        observation,
    ));

    assert_eq!(cache.details().unwrap().comments[0].body, "old comment");
    assert_eq!(
        cache.details_observation_quality(),
        PrObservationQuality::PreservedStale
    );
    assert!(cache.trusted_details().is_err());
    let loaded = load_pr_cache(&repo, "feature");
    assert_eq!(loaded.details().unwrap().comments[0].body, "old comment");
    assert_eq!(
        loaded.details_observation_quality(),
        PrObservationQuality::PreservedStale
    );
    assert!(
        loaded
            .display_error()
            .is_some_and(|error| error.contains("comments: comments unavailable"))
    );
    assert!(loaded.trusted_details().is_err());

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn partial_review_thread_failure_preserves_previous_threads() {
    let (temp, repo, mut cache, summary) = persisted_cache_with_observed_details();
    let mut observation = successful_details_observation_for(&summary);
    observation.review_comments = Err("threads unavailable".to_string());

    assert!(record_pr_details_observation(
        &repo,
        "feature",
        &mut cache,
        observation,
    ));

    assert_eq!(
        cache.details().unwrap().review_comments[0].thread_id,
        "old-thread"
    );
    assert!(cache.trusted_details().is_err());
    let loaded = load_pr_cache(&repo, "feature");
    assert_eq!(
        loaded.details().unwrap().review_comments[0].thread_id,
        "old-thread"
    );
    assert!(
        loaded
            .display_error()
            .is_some_and(|error| error.contains("review threads: threads unavailable"))
    );
    assert!(loaded.trusted_details().is_err());

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn partial_check_failure_preserves_previous_checks() {
    let (temp, repo, mut cache, summary) = persisted_cache_with_observed_details();
    let mut observation = successful_details_observation_for(&summary);
    observation.failing_checks = Err("checks unavailable".to_string());
    observation.check_contexts = Err("check contexts unavailable".to_string());

    assert!(record_pr_details_observation(
        &repo,
        "feature",
        &mut cache,
        observation,
    ));

    assert_eq!(cache.details().unwrap().failing_checks, vec!["old-check"]);
    assert_eq!(cache.details().unwrap().check_contexts[0].name, "old-check");
    assert!(cache.trusted_details().is_err());
    let loaded = load_pr_cache(&repo, "feature");
    assert_eq!(loaded.details().unwrap().failing_checks, vec!["old-check"]);
    assert_eq!(
        loaded.details().unwrap().check_contexts[0].name,
        "old-check"
    );
    assert!(
        loaded
            .display_error()
            .is_some_and(|error| error.contains("checks: checks unavailable"))
    );
    assert!(loaded.trusted_details().is_err());

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn unavailable_ci_logs_preserve_previous_logs_without_poisoning_other_details() {
    let (temp, repo, mut cache, summary) = persisted_cache_with_observed_details();
    let mut observation = successful_details_observation_for(&summary);
    observation.ci_failures = Err("logs unavailable".to_string());

    assert!(record_pr_details_observation(
        &repo,
        "feature",
        &mut cache,
        observation,
    ));

    assert_eq!(cache.details().unwrap().ci_failures[0].log_tail, "old log");
    assert!(cache.trusted_details().is_ok());
    assert!(
        cache
            .display_error()
            .is_some_and(|error| error.contains("CI logs unavailable: logs unavailable"))
    );
    let loaded = load_pr_cache(&repo, "feature");
    assert_eq!(loaded.details().unwrap().ci_failures[0].run_id, "old-run");
    assert_eq!(loaded.details().unwrap().ci_failures[0].log_tail, "");
    assert!(
        loaded
            .display_error()
            .is_some_and(|error| error.contains("CI logs unavailable: logs unavailable"))
    );
    assert!(loaded.trusted_details().is_err());

    let _ = fs::remove_dir_all(temp);
}

fn persisted_cache_with_observed_details() -> (PathBuf, Repository, PrCache, PrSummary) {
    let temp = unique_temp_dir("prism-partial-pr-details");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
    let cache = cache_with_observed_details();
    let summary = cache.summary().unwrap().clone();
    save_pr_cache(&repo, "feature", &cache).unwrap();
    save_pr_details_cache(&repo, "feature", cache.details().unwrap()).unwrap();
    (temp, repo, cache, summary)
}

#[test]
fn pr_summary_refresh_clears_cache_when_branch_has_no_pr() {
    let summary = test_summary("feature", "abc123", 2);
    let mut cache = PrCache::observed(summary, Some(PrDetails::default()));
    cache.record_summary_failure("previous error".to_string());

    cache.record_summary_observation(None, "now".to_string());

    assert!(cache.summary().is_none());
    assert!(cache.details().is_none());
    assert!(cache.display_error().is_none());
    assert_eq!(cache.last_refreshed(), Some("now"));
}

#[test]
fn pr_cache_eligibility_excludes_default_detached_missing_remote_and_merged_prs() {
    let merged_summary = PrSummary {
        merged: true,
        ..test_summary("feature", "abc123", 0)
    };
    let mut merged = test_session("feature", PrCache::observed(merged_summary, None));
    merged.path = std::path::PathBuf::from("/not-used");

    assert!(
        !PrCacheEligibility {
            is_default_branch: true,
            is_detached: false,
            has_github_remote: true,
        }
        .can_observe()
    );
    assert!(
        !PrCacheEligibility {
            is_default_branch: false,
            is_detached: true,
            has_github_remote: true,
        }
        .can_observe()
    );
    assert!(
        !PrCacheEligibility {
            is_default_branch: false,
            is_detached: false,
            has_github_remote: false,
        }
        .can_observe()
    );
    assert!(!pr_cache_pollable_for_session(&merged, &test_config()));
}

#[test]
fn missing_pr_details_obey_poll_interval_after_an_attempt_starts() {
    let mut cache = PrCache::observed(test_summary("feature", "abc123", 0), None);

    assert!(pr_details_due(&cache));
    let _poll = cache.begin_details_poll();

    assert!(!pr_details_due(&cache));
}

#[test]
fn pr_cache_comment_count_prefers_loaded_details_over_summary() {
    let cache = PrCache::observed(
        test_summary("feature", "abc123", 12),
        Some(PrDetails {
            comments: vec![PrComment {
                author: "reviewer".to_string(),
                body: "top-level".to_string(),
                ..PrComment::default()
            }],
            review_comments: vec![
                PrReviewComment {
                    author: "reviewer".to_string(),
                    path: "src/main.rs".to_string(),
                    line: "10".to_string(),
                    body: "inline".to_string(),
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    resolved: false,
                    ..PrReviewComment::default()
                },
                PrReviewComment {
                    author: "reviewer".to_string(),
                    path: "src/lib.rs".to_string(),
                    line: "20".to_string(),
                    body: "resolved".to_string(),
                    created_at: "2026-01-02T00:00:00Z".to_string(),
                    resolved: true,
                    ..PrReviewComment::default()
                },
            ],
            ..PrDetails::default()
        }),
    );

    assert_eq!(pr_cache_comment_count(&cache), 3);
    assert!(pr_cache_has_comments(&cache));
}

#[test]
fn preserved_stale_cache_remains_displayable_but_has_distinct_render_signature() {
    let fresh = cache_with_observed_details();
    let mut stale = fresh.clone();
    stale.mark_preserved_stale();

    assert_eq!(stale.summary(), fresh.summary());
    assert!(stale.details().is_some());
    assert_ne!(
        pr_cache_render_signature(&stale),
        pr_cache_render_signature(&fresh)
    );
    assert!(stale.trusted_summary_and_details().is_err());
}

#[test]
fn pr_summary_index_refresh_updates_sessions_and_pr_cache_storage() {
    let temp = unique_temp_dir("prism-pr-summary-index-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    config.default_base = Some("main".to_string());
    let git = temp.join("git");
    write_executable(&git, "#!/bin/sh\nprintf 'abc123\\n'\n");
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let mut feature_summary = test_summary("feature", "abc123", 2);
    feature_summary.change_request_identity = Some(test_identity(
        crate::remote::ProviderKind::GitHub,
        "github.com",
        "example/repo",
        "PR_feature",
    ));
    let stale_summary = test_summary("stale", "old", 1);
    let details = PrDetails {
        comments: vec![PrComment {
            author: "reviewer".to_string(),
            body: "new comment".to_string(),
            ..PrComment::default()
        }],
        ..PrDetails::default()
    };
    let mut sessions = vec![
        test_session(
            "main",
            PrCache::observed(test_summary("main", "main", 0), None),
        ),
        test_session(
            "feature",
            PrCache::observed(feature_summary.clone(), Some(details.clone())),
        ),
        test_session("stale", PrCache::observed(stale_summary.clone(), None)),
    ];
    for session in &mut sessions {
        session.path = temp.clone();
    }

    let poll_started_at = Instant::now();
    for session in &mut sessions {
        session.pr.begin_summary_poll(poll_started_at);
    }
    refresh_pr_summary_index_for_sessions(
        &[PrCacheRepository {
            repo: &repo,
            config: &config,
        }],
        &mut sessions,
        0,
        vec![feature_summary.clone()],
        poll_started_at,
    );

    assert!(sessions[0].pr.summary().is_none());
    assert!(sessions[2].pr.summary().is_none());
    assert_eq!(sessions[1].pr.summary(), Some(&feature_summary));
    assert!(sessions[1].pr.details().is_some());

    let loaded = load_pr_cache(&repo, "feature");
    assert_eq!(loaded.summary(), Some(&feature_summary));
    assert_eq!(loaded.details().unwrap().comments[0].body, "new comment");
    assert!(load_pr_cache(&repo, "stale").summary().is_none());

    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn stale_pr_summary_index_refresh_does_not_clear_newer_direct_refresh() {
    let temp = unique_temp_dir("prism-stale-pr-summary-index-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    config.default_base = Some("main".to_string());
    let poll_started_at = Instant::now();
    let summary = test_summary("feature", "abc123", 2);
    let mut cache = PrCache::observed(summary.clone(), None);
    cache.record_summary_observation(Some(summary.clone()), "created".to_string());
    cache.begin_summary_poll(poll_started_at);
    cache.begin_summary_poll(poll_started_at + std::time::Duration::from_millis(1));
    save_pr_cache(&repo, "feature", &cache).unwrap();
    let mut sessions = vec![test_session("feature", cache)];

    refresh_pr_summary_index_for_sessions(
        &[PrCacheRepository {
            repo: &repo,
            config: &config,
        }],
        &mut sessions,
        0,
        Vec::new(),
        poll_started_at,
    );

    assert_eq!(sessions[0].pr.summary(), Some(&summary));
    assert_eq!(load_pr_cache(&repo, "feature").summary(), Some(&summary));

    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn merged_pr_from_previous_branch_generation_is_not_reused() {
    let temp = unique_temp_dir("prism-reused-branch-pr-test");
    fs::create_dir_all(&temp).unwrap();
    let git = temp.join("git");
    fs::write(
            &git,
            "#!/bin/sh\ncase \"$*\" in *\"remote get-url origin\"*) echo git@github.com:owner/repo.git ;; *\"merge-base --is-ancestor\"*) exit 1 ;; *) printf 'new-head\\n' ;; esac\n",
        )
        .unwrap();
    let mut permissions = fs::metadata(&git).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&git, permissions).unwrap();

    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let mut old_summary = test_summary("feature", "old-head", 0);
    old_summary.state = "MERGED".to_string();
    old_summary.merged = true;
    let mut sessions = vec![test_session("feature", PrCache::default())];
    sessions[0].path = temp.join("feature");
    let old_cache = PrCache::observed(old_summary.clone(), None);
    save_pr_cache(&repo, "feature", &old_cache).unwrap();

    let loaded = load_pr_cache_for_branch(&repo, &config, "feature", &sessions[0].path);

    assert_eq!(loaded.summary(), Some(&old_summary));
    assert!(loaded.trusted_summary().is_err());

    let poll_started_at = Instant::now();
    for session in &mut sessions {
        session.pr.begin_summary_poll(poll_started_at);
    }
    refresh_pr_summary_index_for_sessions(
        &[PrCacheRepository {
            repo: &repo,
            config: &config,
        }],
        &mut sessions,
        0,
        vec![old_summary],
        poll_started_at,
    );

    assert!(sessions[0].pr.summary().is_none());
    assert!(load_pr_cache(&repo, "feature").summary().is_none());

    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn open_pr_from_previous_branch_generation_is_not_reused_even_when_old_head_is_ancestor() {
    let temp = unique_temp_dir("prism-reused-open-branch-pr-test");
    fs::create_dir_all(&temp).unwrap();
    let git = temp.join("git");
    fs::write(
            &git,
            "#!/bin/sh\ncase \"$*\" in *\"remote get-url origin\"*) echo git@github.com:owner/repo.git ;; *\"merge-base --is-ancestor\"*) exit 0 ;; *) printf 'new-head\\n' ;; esac\n",
        )
        .unwrap();
    let mut permissions = fs::metadata(&git).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&git, permissions).unwrap();

    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let old_summary = test_summary("feature", "old-head", 0);
    let mut old_cache = PrCache::observed(old_summary.clone(), None);
    old_cache.record_summary_observation(Some(old_summary.clone()), "old".to_string());
    save_pr_cache(&repo, "feature", &old_cache).unwrap();

    let loaded = load_pr_cache_for_branch(&repo, &config, "feature", &temp);

    assert_eq!(loaded.summary(), Some(&old_summary));
    assert!(loaded.trusted_summary().is_err());

    let mut sessions = vec![test_session("feature", PrCache::default())];
    sessions[0].path = temp.clone();
    let poll_started_at = Instant::now();
    for session in &mut sessions {
        session.pr.begin_summary_poll(poll_started_at);
    }
    refresh_pr_summary_index_for_sessions(
        &[PrCacheRepository {
            repo: &repo,
            config: &config,
        }],
        &mut sessions,
        0,
        vec![old_summary],
        poll_started_at,
    );
    assert!(sessions[0].pr.summary().is_none());

    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn canonical_cached_pr_requires_fresh_reassociation_after_restart() {
    let temp = unique_temp_dir("prism-synthetic-canonical-pr-test");
    fs::create_dir_all(&temp).unwrap();
    let git = temp.join("git");
    write_executable(
        &git,
        "#!/bin/sh\ncase \"$*\" in *\"remote get-url origin\"*) printf 'https://github.com/example/repo.git\\n' ;; *) printf 'local-head\\n' ;; esac\n",
    );
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let mut summary = test_summary("provider-topic", "remote-head", 1);
    summary.change_request_identity = Some(test_identity(
        crate::remote::ProviderKind::GitHub,
        "github.com",
        "example/repo",
        "PR_canonical",
    ));
    let details = PrDetails {
        comments: vec![PrComment {
            body: "persisted association".to_string(),
            ..PrComment::default()
        }],
        ..PrDetails::default()
    };
    let cache = PrCache::observed(summary.clone(), Some(details));
    persist_pr_cache_snapshot(&repo, "pr-42", &cache).unwrap();

    let loaded = load_pr_cache_for_branch(&repo, &config, "pr-42", &temp);

    assert_eq!(loaded.summary(), Some(&summary));
    assert_eq!(
        loaded.details().unwrap().comments[0].body,
        "persisted association"
    );
    let mut session = test_session("pr-42", loaded);
    session.path = temp.clone();
    assert_eq!(
        resolve_pr_summary_for_session(&session, &config, &[summary]),
        None
    );

    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn guarded_repair_head_uses_exact_lookup_after_restart() {
    let temp = unique_temp_dir("prism-guarded-repair-restart-test");
    fs::create_dir_all(&temp).unwrap();
    let git = temp.join("git");
    let gh = temp.join("gh");
    write_executable(
        &git,
        "#!/bin/sh\ncase \"$*\" in *\"remote get-url origin\"*) printf '%s\\n' 'https://github.com/example/repo.git' ;; *\"remote get-url upstream\"*) exit 2 ;; *) exit 1 ;; esac\n",
    );
    write_executable(
        &gh,
        r#"#!/bin/sh
printf '%s\n' '{"data":{"repository":{"pullRequest":{"id":"PR_test","number":42,"title":"Repair","state":"MERGED","mergedAt":"2026-08-01T00:00:00Z","headRefName":"feature","baseRefName":"main","headRefOid":"repair-head","headRepository":{"nameWithOwner":"example/repo"},"baseRepository":{"nameWithOwner":"example/repo"}}}}}'
"#,
    );
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    let identity = test_identity(
        crate::remote::ProviderKind::GitHub,
        "github.com",
        "example/repo",
        "PR_test",
    );
    let mut summary = test_summary("feature", "repair-head", 0);
    summary.change_request_identity = Some(identity.clone());
    persist_pr_cache_snapshot(&repo, "feature", &PrCache::observed(summary, None)).unwrap();
    let mut loaded = load_pr_cache(&repo, "feature");
    loaded.reauthorize_guarded_summary(&identity, "repair-head");

    crate::remote::dispatcher::refresh_change_request_cache(
        &repo,
        "feature",
        &mut loaded,
        &temp,
        &config,
        false,
    )
    .unwrap();

    assert!(loaded.summary().is_some_and(|summary| summary.merged));
    assert_eq!(
        loaded.summary_observation_quality(),
        PrObservationQuality::Fresh
    );
    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn unavailable_remote_discovery_preserves_persisted_cache_as_stale() {
    let temp = unique_temp_dir("prism-unavailable-remote-cache-test");
    fs::create_dir_all(&temp).unwrap();
    let git = temp.join("git");
    write_executable(&git, "#!/bin/sh\nexit 1\n");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let summary = test_summary("feature", "head-42", 1);
    let details = PrDetails {
        comments: vec![PrComment {
            body: "cached display state".to_string(),
            ..PrComment::default()
        }],
        ..PrDetails::default()
    };
    persist_pr_cache_snapshot(
        &repo,
        "feature",
        &PrCache::observed(summary.clone(), Some(details)),
    )
    .unwrap();

    let loaded = load_pr_cache_for_branch(&repo, &config, "feature", &temp);

    assert_eq!(loaded.summary(), Some(&summary));
    assert_eq!(
        loaded.details().unwrap().comments[0].body,
        "cached display state"
    );
    assert_eq!(
        loaded.summary_observation_quality(),
        PrObservationQuality::PreservedStale
    );
    assert!(loaded.display_error().is_some());
    assert_eq!(load_pr_cache(&repo, "feature").summary(), Some(&summary));

    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn known_open_pr_is_preserved_while_local_repair_is_unpushed() {
    let temp = unique_temp_dir("prism-known-open-pr-local-divergence-test");
    fs::create_dir_all(&temp).unwrap();
    let git = temp.join("git");
    fs::write(&git, "#!/bin/sh\nprintf 'local-repair-head\\n'\n").unwrap();
    let mut permissions = fs::metadata(&git).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&git, permissions).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let summary = PrSummary {
        change_request_identity: Some(test_identity(
            crate::remote::ProviderKind::GitHub,
            "github.com",
            "example/repo",
            "PR_local_repair",
        )),
        ..test_summary("feature", "remote-pr-head", 0)
    };
    let mut sessions = vec![test_session(
        "feature",
        PrCache::observed(summary.clone(), None),
    )];
    sessions[0].path = temp.clone();
    let poll_started_at = Instant::now();
    sessions[0].pr.begin_summary_poll(poll_started_at);

    refresh_pr_summary_index_for_sessions(
        &[PrCacheRepository {
            repo: &repo,
            config: &config,
        }],
        &mut sessions,
        0,
        vec![summary.clone()],
        poll_started_at,
    );

    assert_eq!(sessions[0].pr.summary(), Some(&summary));
    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn parses_graphql_pr_summary_index() {
    let raw = r#"{
          "data": {
            "repository": {
              "pullRequests": {
                "pageInfo": {"hasNextPage": false},
                "nodes": [
                  {
                    "number": 9,
                    "title": "Batch polling",
                    "author": {"login": "octocat"},
                    "body": "summary",
                    "url": "https://github.com/example/repo/pull/9",
                    "state": "OPEN",
                    "reviewDecision": null,
                    "reviewRequests": {
                      "nodes": [
                        {"requestedReviewer": {"__typename": "User", "login": "alice"}},
                        {"requestedReviewer": {"__typename": "Team", "slug": "backend"}}
                      ]
                    },
                    "headRefName": "feature",
                    "baseRefName": "main",
                    "headRefOid": "abc123",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "mergeStateStatus": "DIRTY",
                    "merged": false,
                    "isDraft": false,
                    "comments": {"totalCount": 2},
                    "reviewThreads": {"totalCount": 3},
                    "commits": {
                      "nodes": [
                        {
                          "commit": {
                            "statusCheckRollup": {
                              "contexts": {
                                "pageInfo": {"hasNextPage": false},
                                "nodes": [
                                  {
                                    "__typename": "StatusContext",
                                    "context": "ci",
                                    "state": "SUCCESS"
                                  }
                                ]
                              }
                            }
                          }
                        }
                      ]
                    }
                  }
                ]
              }
            }
          }
        }"#;

    let summaries = parse_pr_summary_index(raw);

    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].number, 9);
    assert_eq!(summaries[0].author, "octocat");
    assert_eq!(summaries[0].head_ref, "feature");
    assert_eq!(summaries[0].review_decision, "UNKNOWN");
    assert_eq!(summaries[0].requested_reviewers, vec!["alice", "backend"]);
    assert_eq!(summaries[0].comment_count, 5);
    assert_eq!(summaries[0].check_status, "passed");
    assert_eq!(summaries[0].merge_state_status, "DIRTY");
}

#[test]
fn github_preserves_merge_state_status_separately_from_mergeability() {
    let raw = r#"{
        "data": {
            "repository": {
                "pullRequests": {
                    "pageInfo": {"hasNextPage": false},
                    "nodes": [{
                        "number": 117,
                        "mergeable": "MERGEABLE",
                        "mergeStateStatus": "BLOCKED",
                        "commits": {
                            "nodes": [{
                                "commit": {
                                    "statusCheckRollup": {
                                        "contexts": {
                                            "pageInfo": {"hasNextPage": false},
                                            "nodes": [{
                                                "__typename": "CheckRun",
                                                "name": "test",
                                                "status": "IN_PROGRESS",
                                                "conclusion": null
                                            }]
                                        }
                                    }
                                }
                            }]
                        }
                    }]
                }
            }
        }
    }"#;

    let mut summaries = parse_pr_summary_index(raw);

    assert_eq!(summaries[0].check_status, "running");
    assert_eq!(summaries[0].merge_state_status, "BLOCKED");
    assert_eq!(
        summaries[0].native_state_evidence.mergeability,
        vec!["MERGEABLE", "BLOCKED"]
    );

    let mut summary = summaries.remove(0);
    summary.change_request_identity = Some(test_identity(
        crate::remote::ProviderKind::GitHub,
        "github.com",
        "example/repo",
        "117",
    ));
    let normalized =
        adapter::normalize_summary(summary, crate::remote::RemoteOperation::ListChangeRequests)
            .unwrap();

    assert_eq!(
        normalized.mergeability,
        crate::remote::MergeabilityState::Mergeable
    );

    let mut behind = parse_pr_summary_index(&raw.replace("BLOCKED", "BEHIND")).remove(0);
    assert_eq!(behind.merge_state_status, "BEHIND");
    behind.change_request_identity = Some(test_identity(
        crate::remote::ProviderKind::GitHub,
        "github.com",
        "example/repo",
        "117",
    ));
    let normalized =
        adapter::normalize_summary(behind, crate::remote::RemoteOperation::ListChangeRequests)
            .unwrap();

    assert_eq!(
        normalized.mergeability,
        crate::remote::MergeabilityState::Behind
    );
}

#[test]
fn graphql_queue_state_distinguishes_native_entry_absence_and_unobserved() {
    let queued = try_parse_pr_summary_index(
            r#"{"data":{"repository":{"pullRequests":{"nodes":[{"number":42,"mergeQueueEntry":{"state":"AWAITING_CHECKS"}}],"pageInfo":{"hasNextPage":false}}}}}"#,
        )
        .unwrap();
    let not_queued = try_parse_pr_summary_index(
            r#"{"data":{"repository":{"pullRequests":{"nodes":[{"number":42,"mergeQueueEntry":null}],"pageInfo":{"hasNextPage":false}}}}}"#,
        )
        .unwrap();
    let direct: GithubPullRequest = serde_json::from_str(r#"{"number":42}"#).unwrap();

    assert_eq!(queued[0].queue_state, "AWAITING_CHECKS");
    assert_eq!(not_queued[0].queue_state, "not_queued");
    assert_eq!(
        pr_summary_from_node(&direct, None).unwrap().queue_state,
        "unknown"
    );
}

#[test]
fn graphql_summary_index_preserves_unknown_lifecycle_without_dropping_other_items() {
    let raw = r#"{
          "data": {"repository": {"pullRequests": {
            "pageInfo": {"hasNextPage": false},
            "nodes": [
              {"number": 9, "state": "OPEN"},
              {"number": 10, "state": "SUPERSEDED_BY_TRAIN"}
            ]
          }}}
        }"#;

    let summaries = try_parse_pr_summary_index(raw).unwrap();

    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].state, "OPEN");
    assert_eq!(summaries[1].state, "SUPERSEDED_BY_TRAIN");
}

#[test]
fn incomplete_graphql_summary_index_is_an_observation_failure() {
    let raw = r#"{"data":{"repository":{}}}"#;

    assert!(try_parse_pr_summary_index(raw).is_err());
}

#[test]
fn incomplete_graphql_summary_pagination_is_a_reported_failure() {
    let raw = include_str!("../../../tests/fixtures/remote/github/summary-truncated.json");
    let error = try_parse_pr_summary_index(raw).unwrap_err();
    let summary = test_summary("feature", "abc123", 0);
    let mut cache = PrCache::observed(summary.clone(), None);
    let poll_started_at = Instant::now();
    cache.begin_summary_poll(poll_started_at);

    assert!(apply_pr_summary_poll_result(
        &mut cache,
        poll_started_at,
        Err(error.clone()),
        "not refreshed",
    ));

    assert!(error.contains("pagination is incomplete"));
    assert_eq!(cache.summary(), Some(&summary));
    assert_eq!(
        cache.summary_observation_quality(),
        PrObservationQuality::PreservedStale
    );
    assert_eq!(cache.display_error(), Some(error.as_str()));
    assert!(cache.trusted_summary().is_err());
}

#[test]
fn paginated_graphql_summary_index_combines_every_page() {
    let raw = r#"[
          {"data":{"repository":{"pullRequests":{
            "pageInfo":{"hasNextPage":true,"endCursor":"page-1"},
            "nodes":[{"number":107,"state":"OPEN","headRefName":"feat/remote-adapters"}]
          }}}},
          {"data":{"repository":{"pullRequests":{
            "pageInfo":{"hasNextPage":false,"endCursor":"page-2"},
            "nodes":[{"number":108,"state":"OPEN","headRefName":"feat/tmux-name-convention"}]
          }}}}
        ]"#;

    let summaries = try_parse_pr_summary_index(raw).unwrap();

    assert_eq!(
        summaries
            .iter()
            .map(|summary| (summary.number, summary.head_ref.as_str()))
            .collect::<Vec<_>>(),
        [
            (107, "feat/remote-adapters"),
            (108, "feat/tmux-name-convention")
        ]
    );
}

#[test]
fn exact_graphql_summary_distinguishes_absence_from_query_errors() {
    let repository = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::parse("github.com").unwrap(),
        "example/repo",
    )
    .unwrap();
    let absent = r#"{"data":{"repository":{"pullRequest":null}}}"#;
    let failed = r#"{
          "data":{"repository":{"pullRequest":null}},
          "errors":[{"message":"temporary failure"}]
        }"#;

    assert_eq!(
        try_parse_pr_summary_for_repository(absent, &repository).unwrap(),
        None
    );
    assert!(
        try_parse_pr_summary_for_repository(failed, &repository)
            .unwrap_err()
            .contains("GraphQL errors")
    );
}

#[test]
fn incomplete_or_truncated_graphql_check_rollup_is_rejected() {
    let response = |page_info: &str| {
        format!(
            r#"{{"data":{{"repository":{{"pullRequests":{{
                    "pageInfo":{{"hasNextPage":false}},
                    "nodes":[{{"number":1,"state":"OPEN","commits":{{"nodes":[{{"commit":{{
                        "statusCheckRollup":{{"contexts":{{{page_info}"nodes":[]}}}}
                    }}}}]}}}}]
                }}}}}}}}"#
        )
    };

    assert!(
        try_parse_pr_summary_index(&response(""))
            .unwrap_err()
            .contains("missing check rollup")
    );
    assert!(
        try_parse_pr_summary_index(&response(r#""pageInfo":{"hasNextPage":true},"#))
            .unwrap_err()
            .contains("first 50")
    );
}

#[test]
fn parses_classic_branch_protection_without_discarding_checks_shape() {
    let facts = parse_classic_branch_protection(
            r#"{
                "url":"https://api.github.com/repos/owner/repo/branches/main/protection",
                "required_pull_request_reviews":{"required_approving_review_count":0,"require_code_owner_reviews":true},
                "required_status_checks":{
                    "strict":true,
                    "contexts":["ci", " lint ", ""],
                    "checks":[{"context":"ci"}, {"context":"build"}]
                },
                "required_conversation_resolution":{"enabled":true}
            }"#,
        )
        .unwrap();

    assert_eq!(facts.required_approvals, 1);
    assert!(facts.require_conversation_resolution);
    assert!(facts.require_branch_up_to_date);
    assert_eq!(facts.required_checks, ["ci", "lint", "build"]);
    assert!(!facts.merge_queue_required);
    assert!(parse_classic_branch_protection("{}").is_err());
}

#[test]
fn fetches_and_combines_exact_branch_classic_and_evaluated_ruleset_policy() {
    let temp = unique_temp_dir("prism-github-exact-policy");
    fs::create_dir_all(&temp).unwrap();
    let gh = temp.join("gh");
    let log = temp.join("gh.log");
    write_executable(
        &gh,
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *'/repos/owner/repo/branches/release%2Fnext/protection'*)
    printf '%s\n' '{{"url":"https://api.github.com/repos/owner/repo/branches/release%2Fnext/protection","required_pull_request_reviews":{{"required_approving_review_count":1}},"required_status_checks":{{"strict":false,"contexts":["classic-ci"]}},"required_conversation_resolution":{{"enabled":true}}}}'
    ;;
  *'/repos/owner/repo/rules/branches/release%2Fnext?per_page=100'*)
    printf '%s\n' '[[{{"type":"pull_request","parameters":{{"required_approving_review_count":2,"required_review_thread_resolution":false,"require_code_owner_review":false,"require_last_push_approval":false}}}},{{"type":"required_status_checks","parameters":{{"strict_required_status_checks_policy":true,"required_status_checks":[{{"context":"ruleset-ci"}}]}}}},{{"type":"merge_queue","parameters":{{"check_response_timeout_minutes":60,"grouping_strategy":"ALLGREEN","max_entries_to_build":5,"max_entries_to_merge":5,"merge_method":"SQUASH","min_entries_to_merge":1,"min_entries_to_merge_wait_minutes":0}}}}]]'
    ;;
  *)
    printf '%s\n' 'unexpected gh command' >&2
    exit 1
    ;;
esac
"#,
            log.display()
        ),
    );
    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    let repository = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.com", None).unwrap(),
        "owner/repo",
    )
    .unwrap();

    let policy = fetch_repo_policy(&temp, &config, &repository, "release/next").unwrap();

    assert_eq!(policy.target_branch.as_deref(), Some("release/next"));
    assert_eq!(policy.required_approvals, 2);
    assert!(policy.require_conversation_resolution);
    assert!(policy.require_branch_up_to_date);
    assert_eq!(policy.required_checks, ["classic-ci", "ruleset-ci"]);
    assert!(policy.merge_queue_required);
    assert!(policy.error.is_none());
    let commands = fs::read_to_string(&log).unwrap();
    assert!(commands.contains("--paginate --slurp"));

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn authoritative_unprotected_and_empty_rulesets_produce_known_empty_policy() {
    let temp = unique_temp_dir("prism-github-empty-policy");
    fs::create_dir_all(&temp).unwrap();
    let gh = temp.join("gh");
    write_executable(
        &gh,
        r#"#!/bin/sh
case "$*" in
  *'/protection'*)
    printf '%s\n' 'gh: Branch not protected (HTTP 404)' >&2
    exit 1
    ;;
  *'/rules/branches/'*)
    printf '%s\n' '[[]]'
    ;;
  *) exit 1 ;;
esac
"#,
    );
    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    let repository = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.com", None).unwrap(),
        "owner/repo",
    )
    .unwrap();

    let policy = fetch_repo_policy(&temp, &config, &repository, "main").unwrap();

    assert_eq!(policy.required_approvals, 0);
    assert!(policy.required_checks.is_empty());
    assert!(!policy.merge_queue_required);
    assert!(policy.error.is_none());

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn evaluated_rules_require_paginated_envelope_and_complete_parameters() {
    assert!(parse_evaluated_branch_rules("[]").is_err());
    assert!(parse_evaluated_branch_rules(r#"[{"type":"merge_queue"}]"#).is_err());
    assert!(parse_evaluated_branch_rules(r#"[[{"type":"merge_queue"}]]"#).is_err());
    assert!(
        parse_evaluated_branch_rules(r#"[[{"type":"required_status_checks"}]]"#)
            .unwrap_err()
            .contains("missing parameters")
    );
    assert!(
        parse_evaluated_branch_rules(
            r#"[[{"type":"pull_request","parameters":{"required_approving_review_count":"one"}}]]"#,
        )
        .unwrap_err()
        .contains("malformed pull_request")
    );
}

#[test]
fn evaluated_rules_ignore_known_non_merge_constraints() {
    let facts = parse_evaluated_branch_rules(
        r#"[[
                {"type":"required_linear_history"},
                {"type":"required_signatures"},
                {"type":"commit_message_pattern","parameters":{"operator":"starts_with"}},
                {"type":"copilot_code_review","parameters":{"review_on_push":false}}
            ]]"#,
    )
    .unwrap();

    assert_eq!(facts.required_approvals, 0);
    assert!(facts.required_checks.is_empty());
    assert!(!facts.merge_queue_required);
}

#[test]
fn safety_relevant_and_unknown_rules_produce_unknown_policy_evidence() {
    for rule_type in [
        "workflows",
        "required_deployments",
        "code_scanning",
        "future_rule",
    ] {
        let raw = format!(r#"[[{{"type":"{rule_type}"}}]]"#);
        let error = parse_evaluated_branch_rules(&raw).unwrap_err();

        assert!(error.contains("policy evidence is unknown"));
        assert!(error.contains(rule_type));
        assert!(!error.contains("malformed"));
    }
}

#[test]
fn only_explicit_unprotected_404_is_authoritative_classic_absence() {
    assert!(is_unprotected_branch_response(
        "gh: Branch not protected (HTTP 404)"
    ));
    assert!(!is_unprotected_branch_response(
        "gh: Branch not found (HTTP 404)"
    ));
    assert!(!is_unprotected_branch_response(
        "gh: Resource not accessible by integration (HTTP 403)"
    ));
}

#[test]
fn failed_policy_refresh_preserves_identity_matched_stale_facts() {
    let temp = unique_temp_dir("prism-github-stale-policy-refresh");
    fs::create_dir_all(&temp).unwrap();
    let gh = temp.join("gh");
    write_executable(&gh, "#!/bin/sh\necho 'policy unavailable' >&2\nexit 1\n");
    let git = temp.join("git");
    write_executable(
        &git,
        "#!/bin/sh\nprintf 'https://github.com/owner/repo.git\\n'\n",
    );
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let stale = RepoPolicyCache {
        repo_remote: "owner/repo".to_string(),
        provider: Some(crate::remote::ProviderKind::GitHub),
        canonical_host: Some("github.com".to_string()),
        project_path: Some("owner/repo".to_string()),
        target_branch: Some("main".to_string()),
        identity_complete: true,
        default_branch: Some("main".to_string()),
        required_approvals: 2,
        require_conversation_resolution: true,
        require_branch_up_to_date: true,
        required_checks: vec!["ci".to_string()],
        merge_queue_required: true,
        refreshed_unix_ms: 123,
        error: None,
    };
    save_repo_policy_cache(&repo, &stale).unwrap();

    let refreshed = refresh_repo_policy_cache(&repo, &temp, &config).unwrap();

    assert_eq!(refreshed.required_approvals, stale.required_approvals);
    assert_eq!(refreshed.required_checks, stale.required_checks);
    assert_eq!(
        refreshed.require_conversation_resolution,
        stale.require_conversation_resolution
    );
    assert_eq!(
        refreshed.require_branch_up_to_date,
        stale.require_branch_up_to_date
    );
    assert_eq!(refreshed.merge_queue_required, stale.merge_queue_required);
    assert_eq!(refreshed.refreshed_unix_ms, stale.refreshed_unix_ms);
    assert!(
        refreshed
            .error
            .as_deref()
            .is_some_and(|error| error.contains("policy unavailable"))
    );
    assert_eq!(load_repo_policy_cache(&repo, "owner/repo"), Some(refreshed));

    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn repo_policy_cache_round_trips_success_and_error() {
    let temp = unique_temp_dir("prism-repo-policy-cache-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let policy = RepoPolicyCache {
        repo_remote: "owner/repo".to_string(),
        provider: Some(crate::remote::ProviderKind::GitHub),
        canonical_host: Some("github.com".to_string()),
        project_path: Some("owner/repo".to_string()),
        target_branch: Some("main".to_string()),
        identity_complete: true,
        default_branch: Some("main".to_string()),
        required_approvals: 1,
        require_conversation_resolution: true,
        require_branch_up_to_date: true,
        required_checks: vec!["ci".to_string(), "lint".to_string()],
        merge_queue_required: false,
        refreshed_unix_ms: 123,
        error: None,
    };

    save_repo_policy_cache(&repo, &policy).unwrap();
    let loaded = load_repo_policy_cache(&repo, "owner/repo").unwrap();

    assert_eq!(loaded, policy);
    let github_repository = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.com", None).unwrap(),
        "owner/repo",
    )
    .unwrap();
    assert_eq!(
        load_repo_policy_cache_for_repository(&repo, &github_repository),
        Some(policy.clone())
    );
    let enterprise_repository = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.example.com", None).unwrap(),
        "owner/repo",
    )
    .unwrap();
    assert!(load_repo_policy_cache_for_repository(&repo, &enterprise_repository).is_none());

    let error_policy = RepoPolicyCache {
        repo_remote: "owner/repo".to_string(),
        refreshed_unix_ms: 456,
        error: Some("gh auth failed".to_string()),
        ..RepoPolicyCache::default()
    };
    save_repo_policy_cache(&repo, &error_policy).unwrap();
    assert_eq!(
        load_repo_policy_cache(&repo, "owner/repo"),
        Some(error_policy)
    );

    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn repo_policy_cache_keeps_distinct_target_branches_under_one_identity() {
    let temp = unique_temp_dir("prism-repo-policy-identity-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let repository = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitLab,
        crate::remote::HostIdentity::new("gitlab.example.com", Some(8443)).unwrap(),
        "owner/repo",
    )
    .unwrap();
    let policy = |target: &str, approvals: u64| RepoPolicyCache {
        repo_remote: "owner/repo".to_string(),
        provider: Some(crate::remote::ProviderKind::GitLab),
        canonical_host: Some("gitlab.example.com:8443".to_string()),
        project_path: Some("owner/repo".to_string()),
        target_branch: Some(target.to_string()),
        identity_complete: true,
        default_branch: Some("main".to_string()),
        required_approvals: approvals,
        refreshed_unix_ms: approvals,
        ..RepoPolicyCache::default()
    };

    save_repo_policy_cache(&repo, &policy("main", 1)).unwrap();
    save_repo_policy_cache(&repo, &policy("release/next", 2)).unwrap();

    assert_eq!(
        load_repo_policy_cache_for_identity(&repo, &repository, "main")
            .unwrap()
            .required_approvals,
        1
    );
    assert_eq!(
        load_repo_policy_cache_for_identity(&repo, &repository, "release/next")
            .unwrap()
            .required_approvals,
        2
    );

    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn github_policy_identity_queries_and_upserts_use_normalized_path_keys() {
    let temp = unique_temp_dir("prism-github-policy-case-key-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let policy = |project_path: &str, approvals: u64| RepoPolicyCache {
        repo_remote: project_path.to_string(),
        provider: Some(crate::remote::ProviderKind::GitHub),
        canonical_host: Some("github.com".to_string()),
        project_path: Some(project_path.to_string()),
        target_branch: Some("main".to_string()),
        identity_complete: true,
        default_branch: Some("main".to_string()),
        required_approvals: approvals,
        refreshed_unix_ms: approvals,
        ..RepoPolicyCache::default()
    };
    save_repo_policy_cache(&repo, &policy("Acme/Widget", 1)).unwrap();
    save_repo_policy_cache(&repo, &policy("acme/widget", 2)).unwrap();
    let lowercase = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.com", None).unwrap(),
        "ACME/WIDGET",
    )
    .unwrap();

    let loaded = load_repo_policy_cache_for_identity(&repo, &lowercase, "main").unwrap();
    let count = observability::with_writable_db(&repo, |conn| {
        conn.query_row(
            "select count(*) from repo_policy_cache_v2 where provider = 'github'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| error.to_string())
    })
    .unwrap();

    assert_eq!(loaded.required_approvals, 2);
    assert_eq!(loaded.project_path.as_deref(), Some("acme/widget"));
    assert_eq!(count, 1);
    let _ = fs::remove_dir_all(repo.prism_dir());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn parses_requested_reviewers_from_gh_pr_view() {
    let raw = r#"{
          "reviewRequests": [
            {"requestedReviewer": {"login": "alice"}},
            {"requestedReviewer": {"slug": "backend"}},
            {"requestedReviewer": {"login": "alice"}}
          ]
        }"#;

    assert_eq!(parse_requested_reviewers(raw), vec!["alice", "backend"]);
}

#[test]
fn parses_github_remote_urls() {
    assert_eq!(
        parse_github_remote("git@github.com:owner/repo.git"),
        Some(("owner".to_string(), "repo".to_string()))
    );
    assert_eq!(
        parse_github_remote("https://github.com/owner/repo"),
        Some(("owner".to_string(), "repo".to_string()))
    );
    assert_eq!(parse_github_remote("https://example.com/owner/repo"), None);
}

#[test]
fn parses_inline_review_comments() {
    let raw = r#"[
            {
                "path": "src/main.rs",
                "line": 12,
                "id": "PRRC_kw123",
                "body": "please simplify",
                "created_at": "2026-01-01T00:00:00Z",
                "user": {"login": "reviewer"}
            }
        ]"#;
    let comments = parse_inline_review_comments(raw);
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].path, "src/main.rs");
    assert_eq!(comments[0].id, "PRRC_kw123");
    assert_eq!(comments[0].line, "12");
    assert_eq!(comments[0].author, "reviewer");
    assert_eq!(comments[0].created_at, "2026-01-01T00:00:00Z");
    assert!(!comments[0].resolved);
}

#[test]
fn parses_review_thread_resolution_status() {
    let raw = r#"{
          "data": {
            "repository": {
                "pullRequest": {
                  "reviewThreads": {
                    "totalCount": 2,
                    "pageInfo": {"hasNextPage": false},
                    "nodes": [
                    {
                      "id": "PRRT_kw123",
                      "isResolved": true,
                      "comments": {
                        "totalCount": 1,
                        "pageInfo": {"hasNextPage": false},
                        "nodes": [
                          {
                            "id": "PRRC_kw123",
                            "path": "src/main.rs",
                            "line": 12,
                            "body": "please simplify",
                            "createdAt": "2026-01-01T00:00:00Z",
                            "author": {"login": "reviewer"}
                          }
                        ]
                      }
                    },
                    {
                      "id": "PRRT_kw456",
                      "isResolved": false,
                      "comments": {
                        "totalCount": 1,
                        "pageInfo": {"hasNextPage": false},
                        "nodes": [
                          {
                            "id": "PRRC_kw456",
                            "path": "src/lib.rs",
                            "originalLine": 20,
                            "body": "still needs work",
                            "createdAt": "2026-01-02T00:00:00Z",
                            "author": {"login": "maintainer"}
                          }
                        ]
                      }
                    }
                  ]
                }
              }
            }
          }
        }"#;

    let comments = parse_review_thread_comments(raw);

    assert_eq!(comments.len(), 2);
    assert_eq!(comments[0].author, "reviewer");
    assert_eq!(comments[0].thread_id, "PRRT_kw123");
    assert_eq!(comments[0].id, "PRRC_kw123");
    assert_eq!(comments[0].path, "src/main.rs");
    assert_eq!(comments[0].line, "12");
    assert!(comments[0].resolved);
    assert_eq!(comments[1].author, "maintainer");
    assert_eq!(comments[1].thread_id, "PRRT_kw456");
    assert_eq!(comments[1].id, "PRRC_kw456");
    assert_eq!(comments[1].path, "src/lib.rs");
    assert_eq!(comments[1].line, "20");
    assert!(!comments[1].resolved);
}

#[test]
fn rejects_truncated_review_threads() {
    let raw = r#"{
          "data": {
            "repository": {
              "pullRequest": {
                "reviewThreads": {
                  "totalCount": 2,
                  "pageInfo": {"hasNextPage": false},
                  "nodes": [
                    {
                      "id": "PRRT_kw123",
                      "isResolved": false,
                      "comments": {"totalCount": 0, "pageInfo": {"hasNextPage": false}, "nodes": []}
                    }
                  ]
                }
              }
            }
          }
        }"#;

    assert_eq!(
        try_parse_review_thread_comments(raw).unwrap_err(),
        "GitHub returned only 1 of 2 review threads"
    );
}

#[test]
fn combines_paginated_review_threads() {
    let raw = r#"[
          {
            "data": {"repository": {"pullRequest": {"reviewThreads": {
              "totalCount": 2,
              "pageInfo": {"hasNextPage": true},
              "nodes": [{
                "id": "PRRT_1",
                "isResolved": false,
                "comments": {"totalCount": 1, "pageInfo": {"hasNextPage": false}, "nodes": [{"id": "C1", "body": "one"}]}
              }]
            }}}}
          },
          {
            "data": {"repository": {"pullRequest": {"reviewThreads": {
              "totalCount": 2,
              "pageInfo": {"hasNextPage": false},
              "nodes": [{
                "id": "PRRT_2",
                "isResolved": false,
                "comments": {"totalCount": 1, "pageInfo": {"hasNextPage": false}, "nodes": [{"id": "C2", "body": "two"}]}
              }]
            }}}}
          }
        ]"#;

    let comments = try_parse_review_thread_comments(raw).unwrap();

    assert_eq!(comments.len(), 2);
    assert_eq!(comments[0].thread_id, "PRRT_1");
    assert_eq!(comments[1].thread_id, "PRRT_2");
}

#[test]
fn rejects_truncated_comments_inside_a_review_thread() {
    let raw = r#"[{
          "data": {"repository": {"pullRequest": {"reviewThreads": {
            "totalCount": 1,
            "pageInfo": {"hasNextPage": false},
            "nodes": [{
              "id": "PRRT_1",
              "isResolved": false,
              "comments": {
                "totalCount": 101,
                "pageInfo": {"hasNextPage": true},
                "nodes": [{"id": "C1", "body": "one"}]
              }
            }]
          }}}}
        }]"#;

    let error = try_parse_review_thread_comments(raw).unwrap_err();

    assert!(error.contains("only 1 of 101 comments"));
}

#[test]
fn canonical_target_number_details_use_complete_paginated_endpoints() {
    let temp = unique_temp_dir("prism-github-paginated-details");
    fs::create_dir_all(&temp).unwrap();
    let gh = temp.join("gh");
    let log = temp.join("gh.log");
    write_executable(
        &gh,
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *'/repos/target/repo/issues/42/comments?per_page=100'*)
    printf '%s\n' '[[{{"id":"C1","body":"one","user":{{"login":"alice"}}}}],[{{"id":"C2","body":"two","user":{{"login":"bob"}}}}]]'
    ;;
  *'/repos/target/repo/pulls/42/reviews?per_page=100'*)
    printf '%s\n' '[[{{"id":"R1","state":"APPROVED","user":{{"login":"reviewer"}}}}]]'
    ;;
  *'/repos/target/repo/pulls/42/files?per_page=100'*)
    printf '%s\n' '[[{{"filename":"src/one.rs"}}],[{{"filename":"src/two.rs"}}]]'
    ;;
  *'/repos/target/repo/commits/head-sha/check-runs?per_page=100'*)
    printf '%s\n' '[{{"total_count":1,"check_runs":[{{"name":"build","status":"completed","conclusion":"success"}}]}}]'
    ;;
  *'/repos/target/repo/commits/head-sha/statuses?per_page=100'*)
    printf '%s\n' '[[{{"context":"legacy-ci","state":"success"}}]]'
    ;;
  *'api graphql'*)
    printf '%s\n' '[{{"data":{{"repository":{{"pullRequest":{{"reviewThreads":{{"totalCount":0,"pageInfo":{{"hasNextPage":false}},"nodes":[]}}}}}}}}}}]'
    ;;
  *)
    printf '%s\n' 'unexpected gh command' >&2
    exit 1
    ;;
esac
"#,
            log.display()
        ),
    );
    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    let repository = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.com", None).unwrap(),
        "target/repo",
    )
    .unwrap();

    let details = fetch_pr_details_for_repository_number(
        &temp,
        &config,
        &repository,
        42,
        "synthetic-local-branch",
        "head-sha",
    )
    .unwrap();

    assert_eq!(details.comments.unwrap().len(), 2);
    assert_eq!(details.reviews.unwrap().len(), 1);
    assert_eq!(details.files.unwrap(), ["src/one.rs", "src/two.rs"]);
    assert!(details.review_comments.unwrap().is_empty());
    assert!(details.failing_checks.unwrap().is_empty());
    assert_eq!(details.check_contexts.unwrap().len(), 2);
    let commands = fs::read_to_string(log).unwrap();
    assert_eq!(commands.matches("--paginate --slurp").count(), 6);
    assert!(!commands.contains("synthetic-local-branch"));

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn fetch_pr_summary_uses_merged_at_instead_of_removed_merged_field() {
    let temp = unique_temp_dir("prism-gh-summary-test");
    let bin = temp.join("bin");
    let repo = temp.join("repo");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&repo).unwrap();
    let gh = bin.join("gh");
    let git = bin.join("git");
    fs::write(
        &gh,
        r#"#!/bin/sh
for arg in "$@"; do
  case "$arg" in
    merged|merged,*|*,merged|*,merged,*)
      echo 'Unknown JSON field: "merged"' >&2
      exit 1
      ;;
  esac
done
cat <<'JSON'
{
  "number": 7,
  "id": "PR_test",
  "title": "Test PR",
  "url": "https://github.com/example/repo/pull/7",
  "state": "CLOSED",
  "reviewDecision": null,
  "headRefName": "feature",
  "baseRefName": "main",
  "headRefOid": "abc123",
  "headRepository": {"nameWithOwner": "example/repo"},
  "updatedAt": "2026-01-01T00:00:00Z",
  "statusCheckRollup": [],
  "mergedAt": "2026-01-02T00:00:00Z",
  "isDraft": false
}
JSON
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&gh).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&gh, permissions).unwrap();
    write_executable(
        &git,
        "#!/bin/sh\nprintf 'https://github.com/example/repo.git\\n'\n",
    );

    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    config
        .tools
        .insert("git".to_string(), git.display().to_string());

    let summary = fetch_pr_summary(&repo, "feature", &config)
        .unwrap()
        .unwrap()
        .0;

    assert_eq!(summary.number, 7);
    assert_eq!(summary.review_decision, "UNKNOWN");
    assert!(summary.merged);
    assert_eq!(
        summary
            .change_request_identity
            .as_ref()
            .map(|identity| identity.native_id()),
        Some("PR_test")
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn fetch_pr_summary_preserves_unknown_native_lifecycle() {
    let temp = unique_temp_dir("prism-gh-unknown-summary-test");
    let bin = temp.join("bin");
    let repo = temp.join("repo");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&repo).unwrap();
    let gh = bin.join("gh");
    let git = bin.join("git");
    write_executable(
        &gh,
        r#"#!/bin/sh
cat <<'JSON'
{
  "number": 7,
  "id": "PR_test",
  "title": "Test PR",
  "state": "SUPERSEDED_BY_TRAIN",
  "headRefName": "feature",
  "baseRefName": "main",
  "headRefOid": "abc123",
  "headRepository": {"nameWithOwner": "example/repo"},
  "statusCheckRollup": []
}
JSON
"#,
    );
    write_executable(
        &git,
        "#!/bin/sh\nprintf 'https://github.com/example/repo.git\\n'\n",
    );
    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    config
        .tools
        .insert("git".to_string(), git.display().to_string());

    let summary = fetch_pr_summary(&repo, "feature", &config)
        .unwrap()
        .unwrap()
        .0;

    assert_eq!(summary.state, "SUPERSEDED_BY_TRAIN");
    assert!(!summary.merged);
    assert_eq!(
        summary.native_state_evidence.lifecycle,
        ["SUPERSEDED_BY_TRAIN"]
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn github_summary_retains_known_lossy_and_future_native_states() {
    let node: GithubPullRequest = serde_json::from_str(
        r#"{
                "number":42,
                "state":"OPEN",
                "reviewDecision":"REVIEW_REQUIRED",
                "mergeStateStatus":"HAS_HOOKS",
                "statusCheckRollup":[
                    {"name":"build","status":"COMPLETED","conclusion":"NEUTRAL"},
                    {"context":"future","state":"NEW_CHECK_STATE"}
                ],
                "mergeQueueEntry":{"state":"AWAITING_CHECKS"}
            }"#,
    )
    .unwrap();

    let summary = pr_summary_from_node(&node, None).unwrap();

    assert_eq!(summary.native_state_evidence.lifecycle, ["OPEN"]);
    assert_eq!(summary.native_state_evidence.review, ["REVIEW_REQUIRED"]);
    assert_eq!(summary.native_state_evidence.mergeability, ["HAS_HOOKS"]);
    assert_eq!(
        summary.native_state_evidence.check,
        ["COMPLETED", "NEUTRAL", "NEW_CHECK_STATE"]
    );
    assert_eq!(summary.native_state_evidence.queue, ["AWAITING_CHECKS"]);
}

#[test]
fn closed_unmerged_request_does_not_match_a_worktree() {
    let mut summary = test_summary("feature", "head123", 0);
    summary.state = "CLOSED".to_string();

    assert!(!pr_summary_matches_worktree(
        &summary,
        "feature",
        Some(&summary),
        None,
        None,
    ));
}

#[test]
fn initial_association_requires_origin_push_source_and_exact_local_head() {
    let origin_push = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.example.com", None).unwrap(),
        "Contributor/Widget",
    )
    .unwrap();
    let target = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.example.com", None).unwrap(),
        "Acme/Widget",
    )
    .unwrap();
    let unrelated = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.example.com", None).unwrap(),
        "Other/Widget",
    )
    .unwrap();
    let native = crate::remote::NativeChangeRequestId::new("PR_42").unwrap();
    let mut summary = test_summary("topic", "head-42", 0);
    summary.change_request_identity = Some(crate::remote::CanonicalChangeRequestIdentity::new(
        &target, &native, &unrelated, &target,
    ));

    assert!(!pr_summary_matches_worktree(
        &summary,
        "topic",
        None,
        Some(&origin_push),
        Some("head-42"),
    ));

    summary.change_request_identity = Some(crate::remote::CanonicalChangeRequestIdentity::new(
        &target,
        &native,
        &crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::new("github.example.com", None).unwrap(),
            "contributor/widget",
        )
        .unwrap(),
        &target,
    ));
    assert!(!pr_summary_matches_worktree(
        &summary,
        "topic",
        None,
        Some(&origin_push),
        Some("different-head"),
    ));
    assert!(pr_summary_matches_worktree(
        &summary,
        "topic",
        None,
        Some(&origin_push),
        Some("head-42"),
    ));
}

#[test]
fn summary_index_association_uses_the_branch_push_remote() {
    let temp = unique_temp_dir("prism-branch-push-summary-association");
    fs::create_dir_all(&temp).unwrap();
    let git = temp.join("git");
    write_executable(
        &git,
        r#"#!/bin/sh
case "$*" in
  *"branch --show-current"*) printf '%s\n' 'topic' ;;
  *"for-each-ref --format=%(push:remotename)%00%(push) refs/heads/topic"*) printf 'publish\000refs/remotes/publish/review/topic\n' ;;
  *"remote get-url --push --all publish"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url publish --push"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url origin --push"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
  *"rev-parse HEAD"*) printf '%s\n' 'head-42' ;;
  *) exit 1 ;;
esac
"#,
    );
    let mut config = test_config();
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let host = crate::remote::HostIdentity::new("github.com", None).unwrap();
    let source = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        host.clone(),
        "contributor/widget",
    )
    .unwrap();
    let target = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        host,
        "acme/widget",
    )
    .unwrap();
    let mut summary = test_summary("review/topic", "head-42", 0);
    summary.change_request_identity = Some(crate::remote::CanonicalChangeRequestIdentity::new(
        &target,
        &crate::remote::NativeChangeRequestId::new("PR_42").unwrap(),
        &source,
        &target,
    ));
    let mut session = test_session("topic", PrCache::default());
    session.path = temp.clone();

    assert_eq!(
        resolve_pr_summary_for_session(&session, &config, std::slice::from_ref(&summary)),
        Some(summary)
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn explicit_cached_target_pr_preserves_maintainer_fork_association() {
    let host = crate::remote::HostIdentity::new("github.com", None).unwrap();
    let source = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        host.clone(),
        "contributor/widget",
    )
    .unwrap();
    let target = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        host,
        "acme/widget",
    )
    .unwrap();
    let identity = crate::remote::CanonicalChangeRequestIdentity::new(
        &target,
        &crate::remote::NativeChangeRequestId::new("PR_fork").unwrap(),
        &source,
        &target,
    );
    let known = PrSummary {
        change_request_identity: Some(identity.clone()),
        ..test_summary("contributor-topic", "remote-head", 0)
    };
    let observed = PrSummary {
        head_sha: "advanced-remote-head".to_string(),
        ..known.clone()
    };
    let maintainer_origin = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.com", None).unwrap(),
        "acme/widget",
    )
    .unwrap();

    assert!(pr_summary_matches_worktree(
        &observed,
        "pr/42",
        Some(&known),
        Some(&maintainer_origin),
        Some("local-repair"),
    ));
}

#[test]
fn unknown_lifecycle_poll_preserves_matching_canonical_session_association() {
    let identity = test_identity(
        crate::remote::ProviderKind::GitHub,
        "github.com",
        "example/repo",
        "PR_42",
    );
    let mut known = test_summary("provider-feature", "head123", 0);
    known.change_request_identity = Some(identity.clone());
    let mut observed = known.clone();
    observed.state = "SUPERSEDED_BY_TRAIN".to_string();
    let mut session = test_session(
        "pr/42",
        PrCache::observed(known, Some(PrDetails::default())),
    );

    let resolved =
        resolve_pr_summary_for_session(&session, &test_config(), std::slice::from_ref(&observed));
    let poll_started_at = Instant::now();
    session.pr.begin_summary_poll(poll_started_at);
    assert!(apply_pr_summary_poll_result(
        &mut session.pr,
        poll_started_at,
        Ok(resolved),
        "now",
    ));

    assert_eq!(session.pr.summary(), Some(&observed));
    assert_eq!(
        session.pr.summary_observation_quality(),
        PrObservationQuality::Fresh
    );
    assert!(session.pr.trusted_summary().is_ok());
    assert_eq!(
        session
            .pr
            .summary()
            .and_then(|summary| summary.change_request_identity.as_ref()),
        Some(&identity)
    );
}

fn test_config() -> Config {
    crate::test_support::test_config()
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
}

#[cfg(unix)]
#[test]
fn adapter_submit_review_posts_authorized_commit_to_configured_api() {
    let directory = unique_temp_dir("prism-github-adapter-submit-review");
    fs::create_dir_all(&directory).unwrap();
    let log = directory.join("review-command");
    let gh = directory.join("configured-gh");
    write_executable(
        &gh,
        &format!(
            r#"#!/bin/sh
case "$2" in
  *"/api/graphql")
    printf '%s\n' '{{"data":{{"repository":{{"pullRequest":{{"id":"PR_42","number":42,"title":"Change","state":"OPEN","headRefName":"topic","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{{"nameWithOwner":"example/repo"}},"baseRepository":{{"nameWithOwner":"example/repo"}}}}}}}}}}'
    ;;
  *"/repos/example/repo/pulls/42/reviews")
    {{ pwd; printf '<%s>\n' "$@"; }} > '{}'
    printf '%s\n' '{{"commit_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}'
    ;;
  *) exit 1 ;;
esac
"#,
            log.display()
        ),
    );
    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    config.remote_hosts.insert(
        "github.example.com".to_string(),
        crate::config::RemoteHostConfig {
            provider: ProviderKind::GitHub,
            web_url: None,
            api_url: Some("https://broker.example.com/github/api/v3".to_string()),
            credential_env: None,
            allow_http: false,
        },
    );
    let change_request = adapter_change_request_for_host("github.example.com");
    let adapter = GitHubAdapter::new(
        &directory,
        &config,
        change_request.target_repository.clone(),
    )
    .unwrap();

    adapter
        .submit_review(&SubmitReview {
            id: change_request.id,
            expected_head_sha: change_request.head_sha,
            kind: ReviewSubmissionKind::RequestChanges,
            body: "  needs changes  ".to_string(),
        })
        .unwrap();

    let command = fs::read_to_string(&log).unwrap();
    let args = command.lines().skip(1).collect::<Vec<_>>();
    assert_eq!(
        command
            .lines()
            .next()
            .map(PathBuf::from)
            .map(|path| path.canonicalize().unwrap()),
        Some(directory.canonicalize().unwrap())
    );
    assert_eq!(args[0], "<api>");
    assert_eq!(
        args[1],
        "<https://broker.example.com/github/api/v3/repos/example/repo/pulls/42/reviews>"
    );
    assert!(
        args.windows(2)
            .any(|pair| pair == ["<--hostname>", "<github.example.com>"])
    );
    assert!(args.windows(2).any(|pair| pair == ["<--method>", "<POST>"]));
    assert!(args.windows(2).any(|pair| {
        pair == [
            "<-f>",
            "<commit_id=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa>",
        ]
    }));
    assert!(
        args.windows(2)
            .any(|pair| pair == ["<-f>", "<event=REQUEST_CHANGES>"])
    );
    assert!(
        args.windows(2)
            .any(|pair| pair == ["<-f>", "<body=needs changes>"])
    );
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn adapter_submit_review_rejects_a_stale_head_before_mutation() {
    let directory = unique_temp_dir("prism-github-adapter-submit-review-stale");
    fs::create_dir_all(&directory).unwrap();
    let mutation = directory.join("mutation");
    let gh = directory.join("configured-gh");
    write_executable(
        &gh,
        &format!(
            r#"#!/bin/sh
case "$*" in
  "api graphql"*)
    printf '%s\n' '{{"data":{{"repository":{{"pullRequest":{{"id":"PR_42","number":42,"title":"Change","state":"OPEN","headRefName":"topic","baseRefName":"main","headRefOid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","headRepository":{{"nameWithOwner":"example/repo"}},"baseRepository":{{"nameWithOwner":"example/repo"}}}}}}}}}}'
    ;;
  *"/pulls/42/reviews"*) touch '{}' ;;
  *) exit 1 ;;
esac
"#,
            mutation.display()
        ),
    );
    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    let change_request = adapter_change_request();
    let adapter = GitHubAdapter::new(
        &directory,
        &config,
        change_request.target_repository.clone(),
    )
    .unwrap();

    let error = adapter
        .submit_review(&SubmitReview {
            id: change_request.id,
            expected_head_sha: change_request.head_sha,
            kind: ReviewSubmissionKind::Approve,
            body: String::new(),
        })
        .unwrap_err();

    assert_eq!(error.operation(), RemoteOperation::SubmitReview);
    assert_eq!(error.class(), RemoteErrorClass::StaleHead);
    assert_eq!(error.retryability(), Retryability::NotRetryable);
    assert_eq!(error.retry_hint(), Some(RetryHint::RefreshObservation));
    assert!(!mutation.exists());
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn adapter_submit_review_rejects_a_mismatched_created_review_commit() {
    let directory = unique_temp_dir("prism-github-adapter-submit-review-mismatch");
    fs::create_dir_all(&directory).unwrap();
    let gh = directory.join("configured-gh");
    write_executable(
        &gh,
        r#"#!/bin/sh
case "$*" in
  "api graphql"*)
    printf '%s\n' '{"data":{"repository":{"pullRequest":{"id":"PR_42","number":42,"title":"Change","state":"OPEN","headRefName":"topic","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{"nameWithOwner":"example/repo"},"baseRepository":{"nameWithOwner":"example/repo"}}}}}'
    ;;
  *"/pulls/42/reviews"*)
    printf '%s\n' '{"commit_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}'
    ;;
  *) exit 1 ;;
esac
"#,
    );
    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    let change_request = adapter_change_request();
    let adapter = GitHubAdapter::new(
        &directory,
        &config,
        change_request.target_repository.clone(),
    )
    .unwrap();

    let error = adapter
        .submit_review(&SubmitReview {
            id: change_request.id,
            expected_head_sha: change_request.head_sha,
            kind: ReviewSubmissionKind::Approve,
            body: String::new(),
        })
        .unwrap_err();

    assert_eq!(error.operation(), RemoteOperation::SubmitReview);
    assert_eq!(error.class(), RemoteErrorClass::StaleHead);
    assert_eq!(error.retryability(), Retryability::NotRetryable);
    assert_eq!(error.retry_hint(), Some(RetryHint::RefreshObservation));
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn adapter_submit_review_rejects_a_malformed_success_response() {
    let directory = unique_temp_dir("prism-github-adapter-submit-review-malformed");
    fs::create_dir_all(&directory).unwrap();
    let gh = directory.join("configured-gh");
    write_executable(
        &gh,
        r#"#!/bin/sh
case "$*" in
  "api graphql"*)
    printf '%s\n' '{"data":{"repository":{"pullRequest":{"id":"PR_42","number":42,"title":"Change","state":"OPEN","headRefName":"topic","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{"nameWithOwner":"example/repo"},"baseRepository":{"nameWithOwner":"example/repo"}}}}}'
    ;;
  *"/pulls/42/reviews"*)
    printf '%s\n' '{"id":123}'
    ;;
  *) exit 1 ;;
esac
"#,
    );
    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    let change_request = adapter_change_request();
    let adapter = GitHubAdapter::new(
        &directory,
        &config,
        change_request.target_repository.clone(),
    )
    .unwrap();

    let error = adapter
        .submit_review(&SubmitReview {
            id: change_request.id,
            expected_head_sha: change_request.head_sha,
            kind: ReviewSubmissionKind::Comment,
            body: "comment".to_string(),
        })
        .unwrap_err();

    assert_eq!(error.operation(), RemoteOperation::SubmitReview);
    assert_eq!(error.class(), RemoteErrorClass::InvalidResponse);
    assert_eq!(error.retryability(), Retryability::NotRetryable);
    assert_eq!(error.retry_hint(), None);
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn adapter_details_reject_association_changes_after_detail_calls() {
    let directory = unique_temp_dir("prism-github-adapter-details-drift");
    fs::create_dir_all(&directory).unwrap();
    let mut config = test_config();
    install_adapter_fixture(&mut config, &directory, 2, "PRRT_1");
    let change_request = adapter_change_request();
    let adapter = GitHubAdapter::new(
        &directory,
        &config,
        change_request.target_repository.clone(),
    )
    .unwrap();

    let error = adapter.change_request_details(&change_request).unwrap_err();

    assert_eq!(error.operation(), RemoteOperation::ObserveChangeRequest);
    assert_eq!(error.class(), RemoteErrorClass::StaleHead);
    assert_eq!(error.retryability(), Retryability::NotRetryable);
    assert_eq!(error.retry_hint(), Some(RetryHint::RefreshObservation));
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn adapter_resolve_rejects_thread_from_another_change_request_before_mutation() {
    let directory = unique_temp_dir("prism-github-adapter-thread-association");
    fs::create_dir_all(&directory).unwrap();
    let mut config = test_config();
    install_adapter_fixture(&mut config, &directory, 999, "PRRT_other");
    let change_request = adapter_change_request();
    let adapter = GitHubAdapter::new(
        &directory,
        &config,
        change_request.target_repository.clone(),
    )
    .unwrap();

    let error = adapter
        .resolve_review_thread(&resolve_request(&change_request))
        .unwrap_err();

    assert_eq!(error.operation(), RemoteOperation::ResolveReviewThread);
    assert_eq!(error.class(), RemoteErrorClass::Validation);
    assert_eq!(error.retryability(), Retryability::NotRetryable);
    assert!(!directory.join("mutation").exists());
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn adapter_resolve_rechecks_head_immediately_before_mutation() {
    let directory = unique_temp_dir("prism-github-adapter-thread-pre-mutation-drift");
    fs::create_dir_all(&directory).unwrap();
    let mut config = test_config();
    install_adapter_fixture(&mut config, &directory, 4, "PRRT_1");
    let change_request = adapter_change_request();
    let adapter = GitHubAdapter::new(
        &directory,
        &config,
        change_request.target_repository.clone(),
    )
    .unwrap();

    let error = adapter
        .resolve_review_thread(&resolve_request(&change_request))
        .unwrap_err();

    assert_eq!(error.operation(), RemoteOperation::ResolveReviewThread);
    assert_eq!(error.class(), RemoteErrorClass::StaleHead);
    assert_eq!(error.retryability(), Retryability::NotRetryable);
    assert_eq!(error.retry_hint(), Some(RetryHint::RefreshObservation));
    assert!(!directory.join("mutation").exists());
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn adapter_resolve_rejects_head_change_after_mutation() {
    let directory = unique_temp_dir("prism-github-adapter-thread-post-mutation-drift");
    fs::create_dir_all(&directory).unwrap();
    let mut config = test_config();
    install_adapter_fixture(&mut config, &directory, 5, "PRRT_1");
    let change_request = adapter_change_request();
    let adapter = GitHubAdapter::new(
        &directory,
        &config,
        change_request.target_repository.clone(),
    )
    .unwrap();

    let error = adapter
        .resolve_review_thread(&resolve_request(&change_request))
        .unwrap_err();

    assert_eq!(error.operation(), RemoteOperation::ResolveReviewThread);
    assert_eq!(error.class(), RemoteErrorClass::StaleHead);
    assert_eq!(error.retryability(), Retryability::NotRetryable);
    assert_eq!(error.retry_hint(), Some(RetryHint::RefreshObservation));
    assert!(directory.join("mutation").exists());
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
fn install_adapter_fixture(
    config: &mut Config,
    directory: &std::path::Path,
    drift_at_observation: usize,
    observed_thread_id: &str,
) {
    let counter = directory.join("observations");
    let mutation = directory.join("mutation");
    crate::test_support::install_tool(
        config,
        directory,
        "gh",
        &format!(
            r#"#!/bin/sh
case "$*" in
  *"resolveReviewThread(input:"*)
    touch '{}'
    printf '%s\n' '{{"data":{{"resolveReviewThread":{{"thread":{{"id":"PRRT_1","isResolved":true}}}}}}}}'
    ;;
  *"reviewThreads(first: 100"*)
    printf '%s\n' '[{{"data":{{"repository":{{"pullRequest":{{"reviewThreads":{{"totalCount":1,"pageInfo":{{"hasNextPage":false}},"nodes":[{{"id":"{}","isResolved":false,"comments":{{"totalCount":1,"pageInfo":{{"hasNextPage":false}},"nodes":[{{"id":"PRRC_1","author":{{"login":"reviewer"}},"path":"src/lib.rs","line":7,"body":"review","createdAt":"2026-01-01T00:00:00Z"}}]}}}}]}}}}}}}}}}]'
    ;;
  *"/issues/42/comments"*|*"/pulls/42/reviews"*|*"/pulls/42/files"*|*"/statuses"*)
    printf '%s\n' '[[]]'
    ;;
  *"/check-runs"*)
    printf '%s\n' '[{{"total_count":0,"check_runs":[]}}]'
    ;;
  *"api graphql"*)
    count=0
    if [ -f '{}' ]; then read count < '{}'; fi
    count=$((count + 1))
    printf '%s\n' "$count" > '{}'
    head='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
    if [ "$count" -ge '{}' ]; then head='bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'; fi
    printf '{{"data":{{"repository":{{"pullRequest":{{"id":"PR_42","number":42,"title":"Change","state":"OPEN","headRefName":"topic","baseRefName":"main","headRefOid":"%s","headRepository":{{"nameWithOwner":"example/repo"}},"baseRepository":{{"nameWithOwner":"example/repo"}},"comments":{{"totalCount":3}},"reviewThreads":{{"totalCount":1}}}}}}}}}}\n' "$head"
    ;;
  *) exit 1 ;;
esac
"#,
            mutation.display(),
            observed_thread_id,
            counter.display(),
            counter.display(),
            counter.display(),
            drift_at_observation,
        ),
    );
}

fn adapter_change_request() -> ChangeRequest {
    adapter_change_request_for_host("github.com")
}

fn adapter_change_request_for_host(host: &str) -> ChangeRequest {
    let repository = RemoteRepositoryId::new(
        ProviderKind::GitHub,
        crate::remote::HostIdentity::new(host, None).unwrap(),
        "example/repo",
    )
    .unwrap();
    ChangeRequest {
        id: ChangeRequestId::new(
            repository.clone(),
            crate::remote::NativeChangeRequestId::new("PR_42").unwrap(),
            Some(42),
        ),
        source_repository: repository.clone(),
        target_repository: repository,
        source_branch: "topic".to_string(),
        target_branch: "main".to_string(),
        head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
    }
}

fn resolve_request(change_request: &ChangeRequest) -> ResolveReviewThread {
    ResolveReviewThread {
        id: change_request.id.clone(),
        thread_id: NativeReviewThreadId::new("PRRT_1").unwrap(),
        expected_head_sha: change_request.head_sha.clone(),
    }
}

fn test_summary(head_ref: &str, head_sha: &str, comment_count: u64) -> PrSummary {
    PrSummary {
        number: 42,
        change_request_identity: None,
        native_state_evidence: crate::remote::NativeStateEvidence::default(),
        title: "Fix review".to_string(),
        author: "author".to_string(),
        body: "Body".to_string(),
        url: "https://github.com/example/repo/pull/42".to_string(),
        state: "OPEN".to_string(),
        review_decision: "CHANGES_REQUESTED".to_string(),
        requested_reviewers: vec!["alice".to_string()],
        head_ref: head_ref.to_string(),
        base_ref: "main".to_string(),
        head_sha: head_sha.to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        check_status: "failed".to_string(),
        merge_state_status: "CLEAN".to_string(),
        queue_state: "not_queued".to_string(),
        comment_count,
        merged: false,
        draft: false,
    }
}

fn test_identity(
    provider: crate::remote::ProviderKind,
    host: &str,
    project_path: &str,
    native_id: &str,
) -> crate::remote::CanonicalChangeRequestIdentity {
    let repository = crate::remote::RemoteRepositoryId::new(
        provider,
        crate::remote::HostIdentity::new(host, None).unwrap(),
        project_path,
    )
    .unwrap();
    crate::remote::CanonicalChangeRequestIdentity::new(
        &repository,
        &crate::remote::NativeChangeRequestId::new(native_id).unwrap(),
        &repository,
        &repository,
    )
}

fn test_session(branch: &str, pr: PrCache) -> Session {
    Session {
        repo_index: 0,
        repo_label: "repo".to_string(),
        repo_key: None,
        path: PathBuf::from("/tmp").join(branch),
        worktree_session_id: format!("test-{branch}"),
        incarnation: String::new(),
        path_display: format!("/tmp/{branch}"),
        branch: branch.to_string(),
        prompt_summary: String::new(),
        classification: crate::session::SessionClassification::Work,
        visibility: 0,
        adopted: false,
        hidden: false,
        status_label: String::new(),
        agent_state: crate::agent::AgentState::Idle,
        opencode_status: None,
        pr,
        wt_columns: BTreeMap::new(),
        unseen_comments: false,
    }
}
