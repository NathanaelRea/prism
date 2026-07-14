use crate::agent::AgentState;
use crate::agent_session::{AgentSessionSlot, AgentSessionWarmupKey, AgentSessionWarmupResult};
use crate::auto_flow::{AutoStepKey, load_auto_run};
use crate::config::{Checks, Config, EscapeKey, MergeMethod};
use crate::github::{PrCache, PrComment, PrDetails, PrSummary, pr_summary_or_error};
use crate::opencode::{OpencodeState, OpencodeStatus, parse_event_payload};
use crate::repo::Repository;
use crate::session::Session;
use crate::tui::{OpencodeEventResult, OpencodePollKey, OpencodePollResult, Tui};

use super::{
    archived_picker_overflow_message, discover_wt_columns, pr_target_choice_list,
    pr_target_repo_for_choice, remote_pr_choice_keys, remote_pr_worktree_branch,
    run_browser_opener, should_prompt_pr_target_choice, status_label_with_behind,
};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[test]
fn browser_opener_invokes_first_available_candidate() {
    let temp = unique_temp_dir("prism-browser-opener-test");
    fs::create_dir_all(&temp).unwrap();
    let log = temp.join("open.log");
    let opener = temp.join("opener");
    fs::write(
        &opener,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$@" > '{}'
exit 0
"#,
            log.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&opener).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&opener, permissions).unwrap();
    let opener = opener.display().to_string();

    let no_args: &[&str] = &[];
    let flag_args: &[&str] = &["--flag"];
    let candidates = [
        ("/definitely/missing", no_args),
        (opener.as_str(), flag_args),
    ];

    let used = run_browser_opener(&candidates, "https://example.test/pr/42").unwrap();

    assert_eq!(used, opener);
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "--flag\nhttps://example.test/pr/42\n"
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn discover_wt_columns_flattens_available_primitive_values() {
    let columns = discover_wt_columns(
        r#"{
            "path":"/repo/feature",
            "url":"https://example.test/pr/42",
            "url_active":true,
            "ci":{"status":"success","number":42},
            "vars":{"localdev":"on"},
            "empty":"",
            "labels":["bug"]
        }"#,
    );

    assert_eq!(
        columns.get("url").map(String::as_str),
        Some("https://example.test/pr/42")
    );
    assert_eq!(columns.get("url_active").map(String::as_str), Some("true"));
    assert_eq!(
        columns.get("ci.status").map(String::as_str),
        Some("success")
    );
    assert_eq!(columns.get("ci.number").map(String::as_str), Some("42"));
    assert_eq!(columns.get("vars.localdev").map(String::as_str), Some("on"));
    assert!(!columns.contains_key("path"));
    assert!(!columns.contains_key("empty"));
    assert!(!columns.contains_key("labels"));
}

#[test]
fn pr_target_choices_offer_upstream_and_origin() {
    let choices = pr_target_choice_list("me/repo", "org/repo");

    assert_eq!(choices.title, "Create Pull Request Target");
    assert_eq!(choices.choices.len(), 2);
    assert_eq!(choices.choices[0].key, "u");
    assert_eq!(choices.choices[0].label, "upstream (org/repo)");
    assert_eq!(choices.choices[1].key, "o");
    assert_eq!(choices.choices[1].label, "origin (me/repo)");
    assert_eq!(
        pr_target_repo_for_choice("u", "me/repo", "org/repo"),
        Some("org/repo".to_string())
    );
    assert_eq!(
        pr_target_repo_for_choice("o", "me/repo", "org/repo"),
        Some("me/repo".to_string())
    );
    assert_eq!(pr_target_repo_for_choice("x", "me/repo", "org/repo"), None);
    assert!(should_prompt_pr_target_choice("me/repo", "org/repo"));
    assert!(!should_prompt_pr_target_choice("me/repo", "me/repo"));
}

#[test]
fn remote_pr_picker_uses_stable_keys_and_branch_names() {
    let keys = remote_pr_choice_keys();

    assert_eq!(keys.first().map(String::as_str), Some("1"));
    assert_eq!(keys.get(8).map(String::as_str), Some("9"));
    assert_eq!(keys.get(9).map(String::as_str), Some("a"));
    assert_eq!(remote_pr_worktree_branch(42), "pr/42");
}

#[test]
fn review_fix_refreshes_pr_details_before_sending_prompt() {
    let temp = unique_temp_dir("prism-review-fix-refresh-test");
    let repo_root = temp.join("repo");
    let worktree = repo_root.join("feature");
    fs::create_dir_all(&worktree).unwrap();
    let gh = temp.join("gh");
    let git = temp.join("git");

    fs::write(
        &gh,
        r#"#!/bin/sh
case "$*" in
  "pr view feature --json comments,reviews,files,statusCheckRollup")
cat <<'JSON'
{"comments":[{"id":"PRC_fresh","author":{"login":"reviewer"},"body":"fresh top-level comment","createdAt":"2026-06-14T12:00:00Z"}],"reviews":[{"id":"PRR_fresh","author":{"login":"bot"},"state":"CHANGES_REQUESTED","body":"fresh review body","submittedAt":"2026-06-14T12:01:00Z"}],"files":[],"statusCheckRollup":{"contexts":{"nodes":[]}}}
JSON
;;
  api\ graphql*)
cat <<'JSON'
{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"id":"PRRT_fresh","isResolved":false,"comments":{"nodes":[{"id":"PRRC_fresh","path":"src/lib.rs","originalLine":12,"body":"fresh inline comment","createdAt":"2026-06-14T12:01:30Z","author":{"login":"reviewer"}}]}}]}}}}}
JSON
;;
  *)
cat <<'JSON'
{"number":42,"title":"Review refresh","body":"","url":"https://github.com/example/repo/pull/42","state":"OPEN","reviewDecision":"CHANGES_REQUESTED","reviewRequests":{"nodes":[]},"headRefName":"feature","baseRefName":"main","headRefOid":"abc123","updatedAt":"2026-06-14T12:02:00Z","comments":{"totalCount":2},"statusCheckRollup":{"contexts":{"nodes":[]}},"isDraft":false}
JSON
;;
esac
"#,
    )
    .unwrap();
    fs::write(
        &git,
        r#"#!/bin/sh
case "$*" in
  *"remote get-url origin"*)
echo "https://github.com/example/repo.git"
;;
esac
"#,
    )
    .unwrap();
    for executable in [&gh, &git] {
        let mut permissions = fs::metadata(executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(executable, permissions).unwrap();
    }

    let mut config = test_config();
    config.default_base = Some("main".to_string());
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let repo = Repository::with_config_dir_for_test(repo_root.clone(), temp.join("config"));
    let mut session = test_session(worktree, "feature");
    session.pr = PrCache {
        summary: Some(PrSummary {
            number: 42,
            title: "Stale review".to_string(),
            body: String::new(),
            url: "https://github.com/example/repo/pull/42".to_string(),
            state: "OPEN".to_string(),
            review_decision: "CHANGES_REQUESTED".to_string(),
            requested_reviewers: Vec::new(),
            head_ref: "feature".to_string(),
            base_ref: "main".to_string(),
            head_sha: "oldsha".to_string(),
            updated_at: "2026-06-14T11:00:00Z".to_string(),
            check_status: "unknown".to_string(),
            merge_state_status: "CLEAN".to_string(),
            comment_count: 1,
            merged: false,
            draft: false,
        }),
        details: Some(PrDetails {
            comments: vec![PrComment {
                author: "reviewer".to_string(),
                body: "stale cached comment".to_string(),
                ..PrComment::default()
            }],
            ..PrDetails::default()
        }),
        ..PrCache::default()
    };
    let mut tui = Tui::new_single(repo, config, vec![session]);
    tui.prompt_submissions = Some(Vec::new());

    tui.start_review_fix_for_test().unwrap();

    let run_id = tui
        .active_auto_runs
        .get(&tui.sessions[0].path)
        .unwrap()
        .clone();
    let persisted =
        crate::observability::with_writable_db(&tui.repo, |conn| load_auto_run(conn, &run_id))
            .unwrap()
            .unwrap();
    assert_eq!(persisted.steps.len(), 1);
    assert_eq!(persisted.steps[0].step_key, AutoStepKey::FixReview);
    let prompt = persisted.steps[0].reason.as_deref().unwrap();
    assert!(prompt.contains("fresh top-level comment"));
    assert!(prompt.contains("fresh review body"));
    assert!(prompt.contains("fresh inline comment"));
    assert!(!prompt.contains("stale cached comment"));
    assert_eq!(
        persisted.steps[0]
            .work_guard
            .as_ref()
            .unwrap()
            .review_thread_ids,
        vec!["PRRT_fresh"]
    );
    assert_eq!(persisted.run.variant, "repair");

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn ci_fix_sends_prompt_to_agent_session() {
    let temp = unique_temp_dir("prism-ci-fix-send-test");
    let repo_root = temp.join("repo");
    let worktree = repo_root.join("feature");
    fs::create_dir_all(&worktree).unwrap();
    let gh = temp.join("gh");
    let git = temp.join("git");

    fs::write(
        &gh,
        r#"#!/bin/sh
case "$*" in
  "pr view feature --json comments,reviews,files,statusCheckRollup")
cat <<'JSON'
{"comments":[],"reviews":[],"files":[],"statusCheckRollup":{"contexts":{"nodes":[{"name":"test","status":"COMPLETED","conclusion":"FAILURE"}]}}}
JSON
;;
  *)
cat <<'JSON'
{"number":42,"title":"CI refresh","body":"","url":"https://github.com/example/repo/pull/42","state":"OPEN","reviewDecision":"","reviewRequests":{"nodes":[]},"headRefName":"feature","baseRefName":"main","headRefOid":"abc123","updatedAt":"2026-06-14T12:02:00Z","comments":{"totalCount":0},"statusCheckRollup":{"contexts":{"nodes":[{"name":"test","status":"COMPLETED","conclusion":"FAILURE"}]}},"isDraft":false}
JSON
;;
esac
"#,
    )
    .unwrap();
    fs::write(
        &git,
        r#"#!/bin/sh
case "$*" in
  *"remote get-url origin"*)
echo "https://github.com/example/repo.git"
;;
esac
"#,
    )
    .unwrap();
    for executable in [&gh, &git] {
        let mut permissions = fs::metadata(executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(executable, permissions).unwrap();
    }

    let mut config = test_config();
    config.default_base = Some("main".to_string());
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let repo = Repository::with_config_dir_for_test(repo_root.clone(), temp.join("config"));
    let session = test_session(worktree, "feature");
    let mut tui = Tui::new_single(repo, config, vec![session]);
    tui.prompt_submissions = Some(Vec::new());

    tui.start_ci_fix_for_test().unwrap();

    let run_id = tui
        .active_auto_runs
        .get(&tui.sessions[0].path)
        .unwrap()
        .clone();
    let persisted =
        crate::observability::with_writable_db(&tui.repo, |conn| load_auto_run(conn, &run_id))
            .unwrap()
            .unwrap();
    assert_eq!(persisted.steps.len(), 1);
    assert_eq!(persisted.steps[0].step_key, AutoStepKey::FixCi);
    let prompt = persisted.steps[0].reason.as_deref().unwrap();
    assert!(prompt.contains("Here are CI failures on PR 42."));
    assert!(prompt.contains("- test"));
    assert_eq!(persisted.run.variant, "repair");

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn pr_summary_or_error_returns_refresh_error() {
    let cache = PrCache {
        error: Some("gh pr view: authentication failed".to_string()),
        ..PrCache::default()
    };

    let error = pr_summary_or_error(&cache).unwrap_err();

    assert_eq!(error, "gh pr view: authentication failed");
}

#[test]
fn default_branch_status_replaces_stale_behind_count() {
    assert_eq!(status_label_with_behind("clean", 2), "behind 2");
    assert_eq!(status_label_with_behind("dirty 1 behind 9", 0), "dirty 1");
    assert_eq!(
        status_label_with_behind("dirty 1 ahead 3 behind 9", 2),
        "dirty 1 ahead 3 behind 2"
    );
}

#[test]
fn archived_picker_reports_overflow_instead_of_truncating() {
    assert!(archived_picker_overflow_message(35, 35).is_none());

    let message = archived_picker_overflow_message(36, 35).unwrap();

    assert!(message.contains("36 archived worktrees"));
    assert!(message.contains("picker limit 35"));
}

#[test]
fn opencode_poll_does_not_mark_busy_session_done_before_completed_message() {
    let temp = unique_temp_dir("prism-opencode-status-order-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(temp.join("worktree"), "feature");
    session.agent_state = AgentState::Running;
    session.opencode_status = Some(test_opencode_status(OpencodeState::Busy));
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);

    tui.opencode_poll_tx
        .send(OpencodePollResult {
            key: OpencodePollKey::for_session(&tui.sessions[0]),
            started_at: Instant::now(),
            status: Ok(test_opencode_status(OpencodeState::Idle)),
        })
        .unwrap();

    tui.poll_opencode_status();
    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().state,
        OpencodeState::Busy
    );
    assert_eq!(tui.sessions[0].agent_state, AgentState::Running);

    tui.opencode_event_tx
        .send(OpencodeEventResult {
            server_url: "http://127.0.0.1:41000".to_string(),
            event: Ok(parse_event_payload(
                r#"{"type":"message.updated","properties":{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"finish":"stop"}}}"#,
            )
            .unwrap()),
        })
        .unwrap();

    assert!(tui.poll_opencode_events());
    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().state,
        OpencodeState::Done
    );
    assert_eq!(tui.sessions[0].agent_state, AgentState::ExitedOk);

    tui.opencode_event_tx
        .send(OpencodeEventResult {
            server_url: "http://127.0.0.1:41000".to_string(),
            event: Ok(parse_event_payload(
                r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":"busy"}}"#,
            )
            .unwrap()),
        })
        .unwrap();
    assert!(tui.poll_opencode_events());

    let poll_started_at = Instant::now();
    tui.opencode_poll_tx
        .send(OpencodePollResult {
            key: OpencodePollKey::for_session(&tui.sessions[0]),
            started_at: poll_started_at,
            status: Ok(test_opencode_status(OpencodeState::Busy)),
        })
        .unwrap();
    tui.opencode_event_tx
        .send(OpencodeEventResult {
            server_url: "http://127.0.0.1:41000".to_string(),
            event: Ok(parse_event_payload(
                r#"{"type":"message.updated","properties":{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":3,"completed":4},"finish":"stop"}}}"#,
            )
            .unwrap()),
        })
        .unwrap();

    assert!(tui.poll_opencode_events());
    tui.poll_opencode_status();
    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().state,
        OpencodeState::Done
    );
    assert_eq!(tui.sessions[0].agent_state, AgentState::ExitedOk);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn opencode_poll_does_not_mark_reconnected_running_session_done_before_completed_message() {
    let temp = unique_temp_dir("prism-opencode-reconnected-status-order-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(temp.join("worktree"), "feature");
    session.agent_state = AgentState::Running;
    session.opencode_status = Some(test_opencode_status(OpencodeState::Unknown));
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);

    tui.opencode_poll_tx
        .send(OpencodePollResult {
            key: OpencodePollKey::for_session(&tui.sessions[0]),
            started_at: Instant::now(),
            status: Ok(test_opencode_status(OpencodeState::Idle)),
        })
        .unwrap();

    tui.poll_opencode_status();
    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().state,
        OpencodeState::Busy
    );
    assert_eq!(tui.sessions[0].agent_state, AgentState::Running);

    tui.opencode_event_tx
        .send(OpencodeEventResult {
            server_url: "http://127.0.0.1:41000".to_string(),
            event: Ok(parse_event_payload(
                r#"{"type":"message.updated","properties":{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"error":{"name":"MessageAbortedError"}}}}"#,
            )
            .unwrap()),
        })
        .unwrap();

    assert!(tui.poll_opencode_events());
    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().state,
        OpencodeState::Done
    );
    assert_eq!(tui.sessions[0].agent_state, AgentState::ExitedOk);
    assert_eq!(
        tui.sessions[0]
            .opencode_status
            .as_ref()
            .unwrap()
            .detail
            .as_deref(),
        Some("MessageAbortedError")
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn opencode_permission_event_marks_session_as_needing_input() {
    let temp = unique_temp_dir("prism-opencode-permission-status-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(temp.join("worktree"), "feature");
    session.agent_state = AgentState::Running;
    session.opencode_status = Some(test_opencode_status(OpencodeState::Busy));
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);

    tui.opencode_event_tx
        .send(OpencodeEventResult {
            server_url: "http://127.0.0.1:41000".to_string(),
            event: Ok(parse_event_payload(
                r#"{"type":"permission.asked","properties":{"sessionID":"ses_1","permission":"bash"}}"#,
            )
            .unwrap()),
        })
        .unwrap();

    assert!(tui.poll_opencode_events());
    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().state,
        OpencodeState::NeedsInput
    );
    assert_eq!(tui.sessions[0].agent_state, AgentState::NeedsInput);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn opencode_prompt_submission_clears_done_status_immediately() {
    let temp = unique_temp_dir("prism-opencode-prompt-status-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    config.default_agent = "opencode".to_string();
    let mut session = test_session(temp.join("worktree"), "feature");
    session.agent_state = AgentState::ExitedOk;
    session.opencode_status = Some(test_opencode_status(OpencodeState::Done));
    session.opencode_status.as_mut().unwrap().detail = Some("MessageAbortedError".to_string());
    let mut tui = Tui::new_single(repo, config, vec![session]);
    tui.prompt_submissions = Some(Vec::new());

    tui.paste_prompt_into_tmux_agent(0, "try again", false)
        .unwrap();

    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().state,
        OpencodeState::Busy
    );
    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().detail,
        None
    );
    assert_eq!(tui.sessions[0].agent_state, AgentState::Running);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn automatic_pr_polling_does_not_block_input_loop() {
    let temp = unique_temp_dir("prism-pr-poll-test");
    fs::create_dir_all(&temp).unwrap();
    let gh = temp.join("gh");
    fs::write(
        &gh,
        r#"#!/bin/sh
sleep 1
echo 'no pull requests found' >&2
exit 1
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&gh).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&gh, permissions).unwrap();

    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "feature");
    let mut tui = Tui::new_single(repo, config, vec![session]);

    let started = Instant::now();
    let changed = tui.poll_pull_requests(false);

    assert!(!changed);
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "automatic PR polling blocked for {:?}",
        started.elapsed()
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn delete_session_does_not_block_input_loop() {
    let temp = unique_temp_dir("prism-delete-nonblocking-test");
    fs::create_dir_all(&temp).unwrap();
    let git_log = temp.join("git.log");
    let git = temp.join("git");
    let tmux = temp.join("tmux");
    fs::write(
        &git,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"worktree remove --force"*)
    sleep 1
    exit 0
    ;;
  *"worktree list --porcelain"*)
    exit 0
    ;;
  *"branch -D feature/delete"*)
    exit 0
    ;;
esac
exit 0
"#,
            git_log.display()
        ),
    )
    .unwrap();
    fs::write(
        &tmux,
        r#"#!/bin/sh
case "$1" in
  list-sessions|kill-session)
    exit 0
    ;;
esac
exit 0
"#,
    )
    .unwrap();
    for executable in [&git, &tmux] {
        let mut permissions = fs::metadata(executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(executable, permissions).unwrap();
    }

    let mut config = test_config();
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    config
        .tools
        .insert("tmux".to_string(), tmux.display().to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "feature/delete");
    let mut tui = Tui::new_single(repo, config, vec![session]);

    let started = Instant::now();
    tui.start_delete_session_for_test().unwrap();

    assert!(
        started.elapsed() < Duration::from_millis(250),
        "delete blocked input loop for {:?}",
        started.elapsed()
    );
    assert_eq!(tui.sessions.len(), 1);
    assert!(tui.sessions[0].hidden);
    assert!(tui.visible_session_indices().is_empty());
    assert_eq!(tui.delete_sessions_in_flight.len(), 1);

    let wait_started = Instant::now();
    while !tui.delete_sessions_in_flight.is_empty()
        && wait_started.elapsed() < Duration::from_secs(3)
    {
        tui.poll_delete_sessions();
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(tui.delete_sessions_in_flight.is_empty());
    assert!(tui.sessions.is_empty());
    let commands = fs::read_to_string(&git_log).unwrap();
    assert!(commands.contains("worktree remove --force"));
    assert!(commands.contains("branch -D feature/delete"));

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn failed_async_delete_restores_hidden_worktree() {
    let temp = unique_temp_dir("prism-delete-restore-test");
    fs::create_dir_all(&temp).unwrap();
    let worktree = temp.join("worktree");
    fs::create_dir_all(&worktree).unwrap();
    let git = temp.join("git");
    let tmux = temp.join("tmux");
    fs::write(
        &git,
        format!(
            r#"#!/bin/sh
case "$*" in
  *"worktree remove --force"*)
    exit 1
    ;;
  *"worktree list --porcelain"*)
    printf 'worktree {}\nHEAD abc\nbranch refs/heads/feature/delete\n\n'
    exit 0
    ;;
esac
exit 0
"#,
            worktree.display()
        ),
    )
    .unwrap();
    fs::write(
        &tmux,
        r#"#!/bin/sh
case "$1" in
  list-sessions|kill-session)
    exit 0
    ;;
esac
exit 0
"#,
    )
    .unwrap();
    for executable in [&git, &tmux] {
        let mut permissions = fs::metadata(executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(executable, permissions).unwrap();
    }

    let mut config = test_config();
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    config
        .tools
        .insert("tmux".to_string(), tmux.display().to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(worktree, "feature/delete");
    let mut tui = Tui::new_single(repo, config, vec![session]);

    tui.start_delete_session_for_test().unwrap();

    assert!(tui.sessions[0].hidden);
    assert!(tui.visible_session_indices().is_empty());

    let wait_started = Instant::now();
    while !tui.delete_sessions_in_flight.is_empty()
        && wait_started.elapsed() < Duration::from_secs(3)
    {
        tui.poll_delete_sessions();
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(tui.delete_sessions_in_flight.is_empty());
    assert_eq!(tui.sessions.len(), 1);
    assert!(!tui.sessions[0].hidden);
    assert_eq!(tui.visible_session_indices(), vec![0]);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn default_branch_does_not_start_pr_polling() {
    let temp = unique_temp_dir("prism-default-branch-pr-poll-test");
    fs::create_dir_all(&temp).unwrap();

    let mut config = test_config();
    config.default_base = Some("main".to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "main");
    let mut tui = Tui::new_single(repo, config, vec![session]);

    let changed = tui.poll_pull_requests(false);

    assert!(!changed);
    assert!(!tui.repos[0].pr_summary_poll_in_flight);
    assert!(tui.pr_polls_in_flight.is_empty());

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn tmux_agent_warmup_does_not_block_startup() {
    let temp = unique_temp_dir("prism-tmux-warmup-test");
    fs::create_dir_all(&temp).unwrap();
    let state = temp.join("tmux-state");
    let tmux = temp.join("tmux");
    fs::write(
        &tmux,
        format!(
            r#"#!/bin/sh
state="$(cat '{}' 2>/dev/null || echo missing)"
case "$1" in
  has-session)
sleep 1
[ "$state" = exists ]
exit $?
;;
  new-session)
echo exists > '{}'
exit 0
;;
  set-option)
exit 0
;;
  display-message)
echo opencode
exit 0
;;
esac
exit 0
"#,
            state.display(),
            state.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&tmux).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&tmux, permissions).unwrap();

    let mut config = test_config();
    config.default_agent = "custom".to_string();
    config
        .agent_commands
        .insert("custom".to_string(), "opencode".to_string());
    config
        .tools
        .insert("tmux".to_string(), tmux.display().to_string());
    config
        .tools
        .insert("opencode".to_string(), "opencode".to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "feature");
    let mut tui = Tui::new_single(repo, config, vec![session]);

    let started = Instant::now();
    tui.start_tmux_agent_warmup();

    assert!(
        started.elapsed() < Duration::from_millis(250),
        "tmux warm-up blocked startup for {:?}",
        started.elapsed()
    );
    assert_eq!(tui.tmux_warmups_in_flight.len(), 1);

    let wait_started = Instant::now();
    while !tui.tmux_warmups_in_flight.is_empty() && wait_started.elapsed() < Duration::from_secs(3)
    {
        tui.poll_tmux_agent_warmup();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(tui.tmux_warmups_in_flight.is_empty());

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn attach_waits_for_selected_tmux_warmup() {
    let temp = unique_temp_dir("prism-tmux-attach-wait-test");
    fs::create_dir_all(&temp).unwrap();
    let tmux = temp.join("tmux");
    fs::write(
        &tmux,
        r#"#!/bin/sh
case "$1" in
  has-session|set-option|attach-session)
exit 0
;;
  display-message)
echo opencode
exit 0
;;
esac
exit 0
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&tmux).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&tmux, permissions).unwrap();

    let mut config = test_config();
    config.default_agent = "custom".to_string();
    config
        .agent_commands
        .insert("custom".to_string(), "opencode".to_string());
    config
        .tools
        .insert("tmux".to_string(), tmux.display().to_string());
    config
        .tools
        .insert("opencode".to_string(), "opencode".to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "feature");
    let key = AgentSessionWarmupKey::new(AgentSessionSlot::for_session(&session), 0);
    let mut tui = Tui::new_single(repo, config, vec![session]);
    tui.tmux_warmups_in_flight.insert(key.clone());
    let tx = tui.tmux_warmup_tx.clone();

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        let _ = tx.send(AgentSessionWarmupResult {
            key,
            running: Some(true),
            error: None,
        });
    });

    let started = Instant::now();
    tui.attach_selected_tmux_session().unwrap();

    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "attach did not wait for selected warm-up"
    );
    let wait_started = Instant::now();
    while !tui.tmux_warmups_in_flight.is_empty() && wait_started.elapsed() < Duration::from_secs(3)
    {
        tui.poll_tmux_agent_warmup();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(tui.tmux_warmups_in_flight.is_empty());

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn prompt_paste_targets_tmux_agent_session() {
    let temp = unique_temp_dir("prism-tmux-prompt-paste-test");
    fs::create_dir_all(&temp).unwrap();
    let log = temp.join("tmux.log");
    let prompt_file = temp.join("prompt.txt");
    let tmux = temp.join("tmux");
    fs::write(
        &tmux,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  has-session|set-option|move-window|rename-window|new-window)
exit 0
;;
  list-windows)
exit 0
;;
  display-message)
echo opencode
exit 0
;;
  capture-pane)
echo 'Ask anything'
exit 0
;;
  load-buffer)
cat > '{}'
exit 0
;;
  paste-buffer)
exit 0
;;
esac
exit 1
"#,
            log.display(),
            prompt_file.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&tmux).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&tmux, permissions).unwrap();

    let mut config = test_config();
    config.default_agent = "custom".to_string();
    config
        .agent_commands
        .insert("custom".to_string(), "opencode".to_string());
    config
        .tools
        .insert("tmux".to_string(), tmux.display().to_string());
    config
        .tools
        .insert("opencode".to_string(), "opencode".to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "feature");
    let mut tui = Tui::new_single(repo, config, vec![session]);

    tui.paste_prompt_into_tmux_agent(0, "build the thing", false)
        .unwrap();

    assert_eq!(fs::read_to_string(&prompt_file).unwrap(), "build the thing");
    assert_eq!(tui.sessions[0].agent_state, AgentState::NeedsInput);
    let commands = fs::read_to_string(&log).unwrap();
    assert!(commands.contains("load-buffer -b"));
    assert!(commands.contains("paste-buffer -d -b"));

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn stale_tmux_warmup_result_does_not_update_current_generation() {
    let temp = unique_temp_dir("prism-tmux-stale-generation-test");
    fs::create_dir_all(&temp).unwrap();
    let mut config = test_config();
    config.default_agent = "opencode".to_string();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "feature");
    let slot = AgentSessionSlot::for_session(&session);
    let stale_key = AgentSessionWarmupKey::new(slot.clone(), 0);
    let mut tui = Tui::new_single(repo, config, vec![session]);
    tui.tmux_generations.insert(slot, 1);

    let changed = tui.apply_tmux_warmup_result(AgentSessionWarmupResult {
        key: stale_key,
        running: Some(true),
        error: None,
    });

    assert!(!changed);
    assert_eq!(tui.sessions[0].agent_state, AgentState::Idle);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn attach_schedules_delayed_rewarm_after_return() {
    let temp = unique_temp_dir("prism-tmux-delayed-rewarm-test");
    fs::create_dir_all(&temp).unwrap();
    let log = temp.join("tmux.log");
    let count = temp.join("display-count");
    let tmux = temp.join("tmux");
    fs::write(
        &tmux,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  has-session|set-option|attach-session|kill-session|new-session)
exit 0
;;
  display-message)
count="$(cat '{}' 2>/dev/null || echo 0)"
count="$((count + 1))"
echo "$count" > '{}'
if [ "$count" -eq 1 ]; then
  echo opencode
else
  echo bash
fi
exit 0
;;
esac
exit 0
"#,
            log.display(),
            count.display(),
            count.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&tmux).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&tmux, permissions).unwrap();

    let mut config = test_config();
    config.default_agent = "custom".to_string();
    config
        .agent_commands
        .insert("custom".to_string(), "opencode".to_string());
    config
        .tools
        .insert("tmux".to_string(), tmux.display().to_string());
    config
        .tools
        .insert("opencode".to_string(), "opencode".to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "feature");
    let mut tui = Tui::new_single(repo, config, vec![session]);

    tui.attach_selected_tmux_session().unwrap();

    let wait_started = Instant::now();
    while !tui.tmux_warmups_in_flight.is_empty() && wait_started.elapsed() < Duration::from_secs(5)
    {
        tui.poll_tmux_agent_warmup();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(tui.tmux_warmups_in_flight.is_empty());
    let commands = fs::read_to_string(&log).unwrap();
    assert!(commands.contains("kill-session -t"));
    assert!(commands.contains("new-session -d -s"));

    let _ = fs::remove_dir_all(temp);
}

fn test_session(path: PathBuf, branch: &str) -> Session {
    fs::create_dir_all(&path).unwrap();
    Session {
        repo_index: 0,
        repo_label: "repo".to_string(),
        repo_key: None,
        path: path.clone(),
        path_display: path.display().to_string(),
        branch: branch.to_string(),
        prompt_summary: String::new(),
        classification: crate::session::SessionClassification::Work,
        visibility: 0,
        adopted: false,
        hidden: false,
        status_label: "clean".to_string(),
        agent_state: AgentState::Idle,
        opencode_status: None,
        pr: PrCache::default(),
        wt_columns: BTreeMap::new(),
        unseen_comments: false,
    }
}

fn test_opencode_status(state: OpencodeState) -> OpencodeStatus {
    OpencodeStatus {
        server_url: Some("http://127.0.0.1:41000".to_string()),
        session_id: Some("ses_1".to_string()),
        title: None,
        state,
        detail: None,
        latest_message: None,
        latest_user_message: None,
        recent_messages: Vec::new(),
        active_tool: None,
        todos: Vec::new(),
        last_updated_unix_ms: Some(1),
    }
}

fn test_config() -> Config {
    Config {
        default_agent: "ask".to_string(),
        default_base: None,
        plan_dir: "plans".to_string(),
        review_packet_dir: ".agent/review".to_string(),
        worktree_command: "wt".to_string(),
        opencode_port_base: 41_000,
        opencode_port_span: 1_000,
        opencode_shutdown_owned_servers: false,
        opencode_plan_plugin: false,
        escape_key: EscapeKey::EscEsc,
        merge_method: MergeMethod::Squash,
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
        user_path: PathBuf::from("/tmp/prism-user-config.toml"),
        repo_config_path: PathBuf::from("/tmp/prism-repo-config.toml"),
    }
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
}
