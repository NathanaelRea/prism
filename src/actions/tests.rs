use crate::agent::AgentState;
use crate::agent_session::{AgentSessionSlot, AgentSessionWarmupKey, AgentSessionWarmupResult};
use crate::config::Config;
use crate::opencode::{OpencodeState, OpencodeStatus, parse_event_payload};
use crate::platform::CommandCandidate;
use crate::remote::{PrCache, PrDetails, PrSummary, pr_summary_or_error};
use crate::repo::Repository;
use crate::session::{DeleteWorktreeOutcome, Session};
use crate::tui::{
    DefaultBranchPollResult, DeleteSessionKey, DeleteSessionResult, OpencodeEventResult,
    OpencodeListenerKey, OpencodePollKey, OpencodePollResult, PanelFocus, PrPollKey, Tui,
    TuiJobKey, TuiJobKind, WtObservation, WtPollResult,
};

use super::worktrees::development_url_opened_message;
use super::{
    apply_bulk_review_resolution, archived_picker_overflow_message, create_change_request_id,
    discover_wt_columns, open_http_url_in_browser, pr_target_choice_list, push_request_id,
    remote_create_mutation_target, remote_pr_choice_keys, remote_pr_worktree_branch,
    resolve_review_request_id, run_browser_opener, status_label_with_behind,
    unresolved_review_thread_ids, worktree_column_choices,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[test]
fn declined_bulk_review_resolution_does_not_resolve_threads() {
    let resolved = RefCell::new(Vec::new());

    let count = apply_bulk_review_resolution(
        false,
        &["thread-1".to_string(), "thread-2".to_string()],
        |thread_id| {
            resolved.borrow_mut().push(thread_id.to_string());
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(count, 0);
    assert!(resolved.borrow().is_empty());
}

#[test]
fn confirmed_bulk_review_resolution_resolves_each_thread_once() {
    let resolved = RefCell::new(Vec::new());

    let count = apply_bulk_review_resolution(
        true,
        &[
            "thread-2".to_string(),
            "thread-1".to_string(),
            "thread-2".to_string(),
        ],
        |thread_id| {
            resolved.borrow_mut().push(thread_id.to_string());
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(count, 2);
    assert_eq!(resolved.into_inner(), vec!["thread-1", "thread-2"]);
}

#[test]
fn review_resolution_uses_only_unresolved_threads_in_the_observed_details() {
    let details = PrDetails {
        review_comments: vec![
            crate::remote::PrReviewComment {
                thread_id: "thread-2".to_string(),
                resolved: false,
                ..crate::remote::PrReviewComment::default()
            },
            crate::remote::PrReviewComment {
                thread_id: "thread-1".to_string(),
                resolved: false,
                ..crate::remote::PrReviewComment::default()
            },
            crate::remote::PrReviewComment {
                thread_id: "thread-2".to_string(),
                resolved: true,
                ..crate::remote::PrReviewComment::default()
            },
        ],
        ..PrDetails::default()
    };

    assert_eq!(
        unresolved_review_thread_ids(&details),
        vec!["thread-1", "thread-2"]
    );
}

#[test]
fn review_resolution_request_identity_covers_the_complete_canonical_operation() {
    let operation = |repository: &str, thread_ids: Vec<String>| {
        let summary = phase_1_pr_summary("head");
        crate::workflow::remote_operation::RemoteMutationOperation::TuiResolveReviewThreads(
            crate::workflow::remote_operation::TuiRemoteResolvePayload {
                repository: repository.into(),
                worktree: format!("{repository}/worktree").into(),
                summary,
                thread_ids,
            },
        )
    };
    let first = resolve_review_request_id(
        &operation(
            "/repo",
            vec!["thread-2".to_string(), "thread-1".to_string()],
        ),
        "/repo#42",
    )
    .unwrap();
    let reordered = resolve_review_request_id(
        &operation(
            "/repo",
            vec![
                "thread-1".to_string(),
                "thread-2".to_string(),
                "thread-1".to_string(),
            ],
        ),
        "/repo#42",
    )
    .unwrap();
    let different_threads = resolve_review_request_id(
        &operation("/repo", vec!["thread-3".to_string()]),
        "/repo#42",
    )
    .unwrap();
    let different_repository = resolve_review_request_id(
        &operation(
            "/other",
            vec!["thread-1".to_string(), "thread-2".to_string()],
        ),
        "/other#42",
    )
    .unwrap();

    assert_eq!(first, reordered);
    assert_ne!(first, different_threads);
    assert_ne!(first, different_repository);
}

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
        CommandCandidate {
            program: "/definitely/missing",
            args: no_args,
        },
        CommandCandidate {
            program: opener.as_str(),
            args: flag_args,
        },
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
fn development_browser_rejects_non_http_urls_without_exposing_the_value() {
    let secret_url = "file:///tmp/private-token-123";
    let error = open_http_url_in_browser(secret_url).unwrap_err();
    assert_eq!(error, "development URL must use http or https");
    assert!(!error.contains(secret_url));
}

#[test]
fn worktree_url_column_choices_are_unconditional_and_semantically_deduplicated() {
    let unconditional = worktree_column_choices(&[], &[], 0);
    assert_eq!(
        unconditional
            .iter()
            .map(|choice| choice.id.as_str())
            .collect::<Vec<_>>(),
        vec!["url", "url_active"]
    );

    let temp = unique_temp_dir("prism-column-alias-test");
    let mut session = test_session(temp.join("feature"), "feature");
    for key in [
        "url",
        "dev_server.url",
        "url_active",
        "dev_server.listening",
    ] {
        session
            .wt_columns
            .insert(key.to_string(), "value".to_string());
    }
    let choices = worktree_column_choices(&["dev_server.url".to_string()], &[session], 0);
    assert_eq!(
        choices
            .iter()
            .map(|choice| choice.id.as_str())
            .collect::<Vec<_>>(),
        vec!["dev_server.url", "url_active"]
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
fn remote_pr_picker_uses_stable_keys_and_preserves_branch_names() {
    let keys = remote_pr_choice_keys();

    assert_eq!(keys.first().map(String::as_str), Some("1"));
    assert_eq!(keys.get(8).map(String::as_str), Some("9"));
    assert_eq!(keys.get(9).map(String::as_str), Some("a"));
    assert_eq!(
        remote_pr_worktree_branch("feature/exact-name"),
        "feature/exact-name"
    );
}

#[test]
fn push_without_change_request_prepares_create_target_and_dialog() {
    let source = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.com", None).unwrap(),
        "contributor/repo",
    )
    .unwrap();
    let target = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.com", None).unwrap(),
        "upstream/repo",
    )
    .unwrap();
    let preparation = crate::workflow::remote_operation::TuiRemoteCreatePreparation {
        source_push: crate::remote::dispatcher::PushGuard {
            repository: source,
            remote: "origin".into(),
            remote_branch: "feature".into(),
            local_branch: "feature".into(),
            expected_head_sha: "abc123".into(),
            set_upstream: true,
        },
        origin_repository: target.clone(),
        upstream_repository: None,
    };

    let mutation = remote_create_mutation_target(&preparation, &target, "main");
    assert!(matches!(
        mutation,
        crate::tui::RemoteMutationTarget::Create {
            source_branch,
            expected_head_sha,
            target_branch,
            ..
        } if source_branch == "feature"
            && expected_head_sha == "abc123"
            && target_branch == "main"
    ));
    let choices = pr_target_choice_list("origin/repo", "upstream/repo");
    assert_eq!(choices.choices[0].key, "u");
    assert_eq!(choices.choices[1].key, "o");
}

#[test]
fn create_request_identity_covers_target_and_body() {
    let repository = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.com", None).unwrap(),
        "contributor/repo",
    )
    .unwrap();
    let target = |project: &str| {
        crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::new("github.com", None).unwrap(),
            project,
        )
        .unwrap()
    };
    let operation = |project: &str, body: &str| {
        crate::workflow::remote_operation::RemoteMutationOperation::TuiCreateChangeRequest(
            crate::workflow::remote_operation::TuiRemoteCreatePayload {
                repository: "/repo".into(),
                worktree: "/repo/worktree".into(),
                branch: "feature".into(),
                body: body.into(),
                target_repository: target(project),
                source_push: crate::remote::dispatcher::PushGuard {
                    repository: repository.clone(),
                    remote: "origin".into(),
                    remote_branch: "feature".into(),
                    local_branch: "feature".into(),
                    expected_head_sha: "abc123".into(),
                    set_upstream: false,
                },
            },
        )
    };

    let first =
        create_change_request_id(&operation("upstream/repo", "first"), "/repo:feature").unwrap();
    let different_target =
        create_change_request_id(&operation("fork/repo", "first"), "/repo:feature").unwrap();
    let different_body =
        create_change_request_id(&operation("upstream/repo", "second"), "/repo:feature").unwrap();

    assert_ne!(first, different_target);
    assert_ne!(first, different_body);
}

#[test]
fn push_request_identity_covers_the_complete_destination() {
    let repository = |project: &str| {
        crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::new("github.com", None).unwrap(),
            project,
        )
        .unwrap()
    };
    let operation = |project: &str, remote_branch: &str| {
        crate::workflow::remote_operation::RemoteMutationOperation::TuiPushBranch(
            crate::workflow::remote_operation::TuiRemotePushPayload {
                repository: "/repo".into(),
                worktree: "/repo/worktree".into(),
                branch: "feature".into(),
                expected: crate::remote::dispatcher::PushGuard {
                    repository: repository(project),
                    remote: "origin".into(),
                    remote_branch: remote_branch.into(),
                    local_branch: "feature".into(),
                    expected_head_sha: "abc123".into(),
                    set_upstream: false,
                },
            },
        )
    };

    let first = push_request_id(&operation("example/repo", "feature"), "/repo:feature").unwrap();
    let different_repository =
        push_request_id(&operation("fork/repo", "feature"), "/repo:feature").unwrap();
    let different_branch =
        push_request_id(&operation("example/repo", "other"), "/repo:feature").unwrap();

    assert_ne!(first, different_repository);
    assert_ne!(first, different_branch);
}

#[test]
fn pr_summary_or_error_returns_refresh_error() {
    let cache = PrCache::stale_for_test(None, "gh pr view: authentication failed");

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
    let mut config = test_config();
    config.notifications.enabled = true;
    config.notifications.completed = true;
    let mut tui = Tui::new_single(repo, config, vec![session]);
    let (notifier, notifications) = crate::desktop_notification::DesktopNotifier::recording();
    tui.desktop_notifier = notifier;
    tui.reseed_desktop_notifications();

    tui.opencode_poll_tx
        .send(OpencodePollResult {
            key: OpencodePollKey::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]),
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
            stream: test_opencode_stream(&tui),
            received_at: Instant::now(),
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
    tui.desktop_notifier.flush();
    assert_eq!(notifications.lock().unwrap().len(), 1);

    tui.opencode_event_tx
        .send(OpencodeEventResult {
            stream: test_opencode_stream(&tui),
            received_at: Instant::now(),
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
            key: OpencodePollKey::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]),
            started_at: poll_started_at,
            status: Ok(test_opencode_status(OpencodeState::Busy)),
        })
        .unwrap();
    tui.opencode_event_tx
        .send(OpencodeEventResult {
            stream: test_opencode_stream(&tui),
            received_at: Instant::now(),
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
    tui.desktop_notifier.flush();
    assert_eq!(notifications.lock().unwrap().len(), 2);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn opencode_poll_does_not_mark_reconnected_running_session_done_before_completed_message() {
    let temp = unique_temp_dir("prism-opencode-reconnected-status-order-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(temp.join("worktree"), "feature");
    session.agent_state = AgentState::Running;
    session.opencode_status = Some(test_opencode_status(OpencodeState::Unknown));
    let mut config = test_config();
    config.notifications.enabled = true;
    let mut tui = Tui::new_single(repo, config, vec![session]);
    let (notifier, notifications) = crate::desktop_notification::DesktopNotifier::recording();
    tui.desktop_notifier = notifier;
    tui.reseed_desktop_notifications();

    tui.opencode_poll_tx
        .send(OpencodePollResult {
            key: OpencodePollKey::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]),
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
            stream: test_opencode_stream(&tui),
            received_at: Instant::now(),
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
    tui.desktop_notifier.flush();
    assert!(notifications.lock().unwrap().is_empty());

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn opencode_permission_event_marks_session_as_needing_input() {
    let temp = unique_temp_dir("prism-opencode-permission-status-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(temp.join("worktree"), "feature");
    session.agent_state = AgentState::Running;
    session.opencode_status = Some(test_opencode_status(OpencodeState::Busy));
    let mut config = test_config();
    config.notifications.enabled = true;
    let mut tui = Tui::new_single(repo, config, vec![session]);
    let (notifier, notifications) = crate::desktop_notification::DesktopNotifier::recording();
    tui.desktop_notifier = notifier;
    tui.reseed_desktop_notifications();

    tui.opencode_event_tx
        .send(OpencodeEventResult {
            stream: test_opencode_stream(&tui),
            received_at: Instant::now(),
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
    tui.desktop_notifier.flush();
    assert_eq!(
        notifications.lock().unwrap().as_slice(),
        ["repo: feature is waiting for input"]
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn opencode_event_from_stale_generation_is_rejected() {
    let temp = unique_temp_dir("prism-opencode-stale-generation-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(temp.join("worktree"), "feature");
    session.agent_state = AgentState::Running;
    session.opencode_status = Some(test_opencode_status(OpencodeState::Busy));
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    let mut stream = test_opencode_stream(&tui);
    stream.generation = stream.generation.saturating_add(1);

    assert!(!tui.apply_opencode_event_result(OpencodeEventResult {
        stream,
        received_at: Instant::now(),
        event: Ok(parse_event_payload(
            r#"{"type":"message.updated","properties":{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"finish":"stop"}}}"#,
        )
        .unwrap()),
    }));
    assert_eq!(tui.sessions[0].agent_state, AgentState::Running);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn opencode_listener_replaces_reused_url_when_stream_identity_changes() {
    let temp = unique_temp_dir("prism-opencode-listener-identity-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(temp.join("worktree"), "feature");
    session.opencode_status = Some(test_opencode_status(OpencodeState::Busy));
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    let old = test_opencode_stream(&tui);
    let mut current = old.clone();
    current.session_id = "ses_2".to_string();
    tui.sessions[0].opencode_status.as_mut().unwrap().session_id = Some(current.session_id.clone());
    tui.opencode_listeners.insert(old.clone());
    tui.spawn_tui_job(
        TuiJobKind::OpencodeListener,
        TuiJobKey::OpencodeListener(old.clone()),
        old.generation,
        None,
        "obsolete-opencode-listener".to_string(),
        |context| {
            while !context.wait(Duration::from_secs(60)) {}
            Ok(None)
        },
    );

    let to_start = tui.reconcile_opencode_listener_jobs(&BTreeSet::from([current.clone()]));

    assert_eq!(to_start, BTreeSet::from([current.clone()]));
    tui.opencode_listeners.insert(current.clone());
    let started = Instant::now();
    while tui.opencode_listeners.contains(&old) {
        tui.route_tui_job_messages();
        assert!(started.elapsed() < Duration::from_secs(1));
        std::thread::yield_now();
    }
    assert!(tui.opencode_listeners.contains(&current));

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
fn idle_pr_polling_does_not_launch_git_for_worktree_sessions() {
    let temp = unique_temp_dir("prism-idle-pr-poll-test");
    fs::create_dir_all(&temp).unwrap();
    let git_log = temp.join("git.log");
    let git = temp.join("git");
    fs::write(
        &git,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf 'git@github.com:owner/repo.git\\n'\n",
            git_log.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&git).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&git, permissions).unwrap();

    let mut config = test_config();
    config.default_base = Some("main".to_string());
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let sessions = vec![
        test_session(temp.join("main"), "main"),
        test_session(temp.join("feature-a"), "feature-a"),
        test_session(temp.join("feature-b"), "feature-b"),
    ];
    let mut tui = Tui::new_single(repo, config, sessions);
    tui.repos[0].pr_summary_last_polled = Some(Instant::now());

    assert!(!tui.poll_pull_requests(false));

    assert_eq!(fs::read_to_string(&git_log).unwrap_or_default(), "");
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn delete_session_does_not_block_input_loop() {
    let temp = unique_temp_dir("prism-delete-nonblocking-test");
    fs::create_dir_all(&temp).unwrap();
    let git_log = temp.join("git.log");
    let git = temp.join("git");
    let wt_log = temp.join("wt.log");
    let wt = temp.join("wt");
    let tmux = temp.join("tmux");
    let worktree = temp.join("worktree");
    fs::write(
        &git,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"rev-parse --verify refs/heads/feature/delete"*)
    echo branch-oid
    exit 0
    ;;
  *"worktree remove --force"*)
    sleep 1
    exit 0
    ;;
  *"worktree list --porcelain"*)
    if [ -d '{}' ]; then
      printf 'worktree %s\nHEAD branch-oid\nbranch refs/heads/feature/delete\n\n' '{}'
    fi
    exit 0
    ;;
  *"branch -D -- feature/delete"*)
    exit 0
    ;;
esac
exit 0
"#,
            git_log.display(),
            worktree.display(),
            worktree.display()
        ),
    )
    .unwrap();
    fs::write(
        &wt,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nsleep 1\nrm -rf '{}'\nprintf '%s' '[{{\"branch\":\"feature/delete\",\"branch_deleted\":false,\"kind\":\"worktree\",\"path\":\"{}\"}}]'\n",
            wt_log.display(),
            worktree.display(),
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
    for executable in [&git, &wt, &tmux] {
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
    config
        .tools
        .insert("wt".to_string(), wt.display().to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(worktree, "feature/delete");
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
    assert!(
        fs::read_to_string(&wt_log)
            .unwrap()
            .contains("--no-delete-branch")
    );
    assert!(
        fs::read_to_string(&git_log)
            .unwrap()
            .contains("branch -D -- feature/delete")
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn completed_delete_schedules_inventory_refresh_without_tui_thread_io() {
    let temp = unique_temp_dir("prism-delete-refresh-boundary-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let cache = PrCache::observed(phase_1_pr_summary("abc123"), None);
    crate::remote::save_pr_cache(&repo, "feature/delete", &cache).unwrap();
    let mut session = test_session(temp.join("worktree"), "feature/delete");
    session.pr = cache;
    let mut tui = Tui::new_single(repo.clone(), test_config(), vec![session]);
    let worktree = tui.sessions[0].identity_key(&tui.repos[0].identity);
    let pr_key = PrPollKey::for_repository_session_generation(
        &tui.repos[0].identity,
        &tui.sessions[0],
        tui.worktree_generations[&worktree],
    );
    tui.pr_persistence_in_flight.insert(pr_key.clone());
    let key = DeleteSessionKey {
        generation: tui.worktree_generations[&worktree],
        worktree: worktree.clone(),
    };
    tui.delete_session_tx
        .send(DeleteSessionResult {
            key,
            delivery_id: 1,
            result: Ok(DeleteWorktreeOutcome::Deleted),
        })
        .unwrap();

    let changed = crate::flight_recorder::deny_external_calls_on_current_thread(|| {
        crate::observability::deny_database_access_on_current_thread(|| tui.poll_delete_sessions())
    });

    assert!(changed);
    assert!(tui.sessions.is_empty());
    assert!(tui.session_refresh_in_flight);
    assert!(tui.pr_persistence_pending.contains_key(&pr_key));

    tui.pr_persistence_in_flight.remove(&pr_key);
    wait_for_pr_persistence(&mut tui);
    assert!(
        crate::remote::load_pr_cache(&repo, "feature/delete")
            .summary()
            .is_none()
    );

    drop(tui);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn failed_async_delete_restores_hidden_worktree() {
    let temp = unique_temp_dir("prism-delete-restore-test");
    fs::create_dir_all(&temp).unwrap();
    let worktree = temp.join("worktree");
    fs::create_dir_all(&worktree).unwrap();
    let git = temp.join("git");
    let wt = temp.join("wt");
    let tmux = temp.join("tmux");
    fs::write(
        &git,
        format!(
            r#"#!/bin/sh
case "$*" in
  *"rev-parse --verify refs/heads/feature/delete"*)
    echo branch-oid
    exit 0
    ;;
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
        &wt,
        "#!/bin/sh\nprintf '%s\\n' 'pre-remove hook failed' >&2\nexit 1\n",
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
    for executable in [&git, &wt, &tmux] {
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
    config
        .tools
        .insert("wt".to_string(), wt.display().to_string());
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
fn phase_1_branch_delete_failure_reconciles_without_vanished_worktree_path() {
    let temp = unique_temp_dir("prism-phase-1-delete-reconcile-test");
    fs::create_dir_all(&temp).unwrap();
    let worktree = temp.join("worktree");
    fs::create_dir_all(&worktree).unwrap();
    let git = temp.join("git");
    let wt = temp.join("wt");
    let tmux = temp.join("tmux");
    fs::write(
        &git,
        format!(
            r#"#!/bin/sh
case "$*" in
  *"rev-parse --verify refs/heads/feature/delete"*) echo branch-oid; exit 0 ;;
  *"worktree remove --force"*) exit 0 ;;
  *"branch -D -- feature/delete"*) exit 1 ;;
  *"worktree list --porcelain"*)
    if [ -d '{}' ]; then
      printf 'worktree %s\nHEAD branch-oid\nbranch refs/heads/feature/delete\n\n' '{}'
    fi
    exit 0
    ;;
esac
exit 0
"#,
            worktree.display(),
            worktree.display()
        ),
    )
    .unwrap();
    fs::write(
        &wt,
        format!(
            "#!/bin/sh\nrm -rf '{}'\nprintf '%s' '[{{\"branch\":\"feature/delete\",\"branch_deleted\":false,\"kind\":\"worktree\",\"path\":\"{}\"}}]'\n",
            worktree.display(),
            worktree.display()
        ),
    )
    .unwrap();
    fs::write(
        &tmux,
        r#"#!/bin/sh
case "$1" in
  list-sessions|kill-session) exit 0 ;;
esac
exit 0
"#,
    )
    .unwrap();
    for executable in [&git, &wt, &tmux] {
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
    config
        .tools
        .insert("wt".to_string(), wt.display().to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(worktree.clone(), "feature/delete");
    let mut tui = Tui::new_single(repo, config, vec![session]);

    tui.start_delete_session_for_test().unwrap();
    let wait_started = Instant::now();
    while !tui.delete_sessions_in_flight.is_empty() {
        tui.poll_delete_sessions();
        assert!(
            tui.delete_sessions_in_flight.is_empty()
                || wait_started.elapsed() < Duration::from_secs(10),
            "delete did not finish within 10 seconds"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(tui.delete_sessions_in_flight.is_empty());
    let pending = tui
        .sessions
        .iter()
        .find(|session| session.path == worktree)
        .expect("partial deletion remains selectable for retry");
    assert_eq!(pending.status_label, "deletion pending");
    assert_eq!(tui.visible_session_indices(), vec![0]);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn unavailable_remote_preserves_stale_live_and_persisted_display_state() {
    let temp = unique_temp_dir("prism-phase-1-removed-remote-poll-test");
    fs::create_dir_all(&temp).unwrap();
    let git = temp.join("git");
    fs::write(&git, "#!/bin/sh\necho 'origin is missing' >&2\nexit 2\n").unwrap();
    let mut permissions = fs::metadata(&git).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&git, permissions).unwrap();

    let mut config = test_config();
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let summary = phase_1_pr_summary("old-head");
    let cache = PrCache::observed(
        summary,
        Some(PrDetails {
            files: vec!["src/stale.rs".to_string()],
            ..PrDetails::default()
        }),
    );
    crate::remote::persist_pr_cache_snapshot(&repo, "feature", &cache).unwrap();
    let mut session = test_session(temp.join("worktree"), "feature");
    session.pr = cache;
    session.unseen_comments = true;
    let mut tui = Tui::new_single(repo.clone(), config, vec![session]);

    assert!(!tui.poll_pull_requests(true));
    let started = Instant::now();
    let mut changed = false;
    while tui.sessions[0].pr.trusted_summary().is_ok() {
        changed |= tui.poll_pull_requests(false);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "unavailable remote was not observed"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(changed);
    assert!(tui.sessions[0].pr.summary().is_some());
    assert!(tui.sessions[0].pr.details().is_some());
    assert!(tui.sessions[0].pr.trusted_summary().is_err());
    assert!(tui.sessions[0].unseen_comments);
    wait_for_pr_persistence(&mut tui);
    let persisted = crate::remote::load_pr_cache(&repo, "feature");
    assert!(persisted.summary().is_some());
    assert!(persisted.details().is_some());
    assert!(persisted.trusted_summary().is_err());

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn missing_github_remote_clears_hidden_non_pollable_pr_cache_state() {
    let temp = unique_temp_dir("prism-removed-remote-hidden-cache-test");
    fs::create_dir_all(&temp).unwrap();
    let git = temp.join("git");
    fs::write(&git, "#!/bin/sh\nexit 2\n").unwrap();
    let mut permissions = fs::metadata(&git).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&git, permissions).unwrap();
    let mut config = test_config();
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let cache = PrCache::observed(phase_1_pr_summary("old-head"), None);
    crate::remote::save_pr_cache(&repo, "feature", &cache).unwrap();
    let mut session = test_session(temp.join("worktree"), "feature");
    session.hidden = true;
    session.pr = cache;
    let mut tui = Tui::new_single(repo.clone(), config, vec![session]);

    assert!(tui.poll_pull_requests(true));

    assert!(tui.sessions[0].pr.summary().is_none());
    wait_for_pr_persistence(&mut tui);
    assert!(
        crate::remote::load_pr_cache(&repo, "feature")
            .summary()
            .is_none()
    );
    let _ = fs::remove_dir_all(temp);
}

fn wait_for_pr_persistence(tui: &mut Tui) {
    let started = Instant::now();
    while !tui.pr_persistence_in_flight.is_empty() || !tui.pr_persistence_pending.is_empty() {
        tui.poll_pull_requests(false);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "PR cache persistence did not finish"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn default_branch_starts_only_repository_level_remote_polling() {
    let temp = unique_temp_dir("prism-default-branch-pr-poll-test");
    fs::create_dir_all(&temp).unwrap();

    let mut config = test_config();
    config.default_base = Some("main".to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "main");
    let mut tui = Tui::new_single(repo, config, vec![session]);

    let changed = tui.poll_pull_requests(false);

    assert!(!changed);
    assert!(tui.repos[0].pr_summary_poll_in_flight);
    assert!(tui.pr_polls_in_flight.is_empty());

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn tmux_agent_warmup_does_not_block_startup() {
    let temp = unique_temp_dir("prism-tmux-warmup-test");
    fs::create_dir_all(&temp).unwrap();
    let state = temp.join("tmux-state");
    let release = temp.join("tmux-release");
    let timed_out = temp.join("tmux-timed-out");
    let tmux = temp.join("tmux");
    fs::write(
        &tmux,
        format!(
            r#"#!/bin/sh
state="$(cat '{}' 2>/dev/null || echo missing)"
case "$1" in
  has-session)
attempts=0
while [ ! -f '{}' ]; do
  attempts=$((attempts + 1))
  if [ "$attempts" -ge 100 ]; then
    touch '{}'
    break
  fi
  sleep 0.01
done
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
            release.display(),
            timed_out.display(),
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

    tui.start_tmux_agent_warmup();

    assert!(!timed_out.exists(), "tmux warm-up blocked startup");
    assert_eq!(tui.tmux_warmups_in_flight.len(), 1);
    fs::write(&release, "continue").unwrap();

    let wait_started = Instant::now();
    while !tui.tmux_warmups_in_flight.is_empty() {
        tui.poll_tmux_agent_warmup();
        assert!(
            tui.tmux_warmups_in_flight.is_empty()
                || wait_started.elapsed() < Duration::from_secs(10),
            "tmux warm-up did not finish within 10 seconds"
        );
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
    let mut tui = Tui::new_single(repo, config, vec![session]);
    let key = AgentSessionWarmupKey::new(
        AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]),
        0,
    );
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
    assert_eq!(tui.sessions[0].agent_state, AgentState::Attached);
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
    let mut tui = Tui::new_single(repo, config, vec![session]);
    let slot = AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
    let stale_key = AgentSessionWarmupKey::new(slot.clone(), 0);
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
fn tmux_warmup_liveness_does_not_replay_current_notification() {
    let temp = unique_temp_dir("prism-tmux-notification-replay-test");
    fs::create_dir_all(&temp).unwrap();
    let mut config = test_config();
    config.notifications.enabled = true;
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(temp.join("worktree"), "feature");
    session.agent_state = AgentState::NeedsInput;
    let mut tui = Tui::new_single(repo, config, vec![session]);
    let (notifier, notifications) = crate::desktop_notification::DesktopNotifier::recording();
    tui.desktop_notifier = notifier;
    tui.reseed_desktop_notifications();
    let slot = AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
    tui.tmux_generations.insert(slot.clone(), 1);

    assert!(tui.apply_tmux_warmup_result(AgentSessionWarmupResult {
        key: AgentSessionWarmupKey::new(slot, 1),
        running: Some(true),
        error: None,
    }));
    assert_eq!(tui.sessions[0].agent_state, AgentState::Attached);
    assert!(tui.apply_agent_state(0, AgentState::NeedsInput, true));

    tui.desktop_notifier.flush();
    assert!(notifications.lock().unwrap().is_empty());

    assert!(tui.apply_tmux_warmup_result(AgentSessionWarmupResult {
        key: AgentSessionWarmupKey::new(
            AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0],),
            1,
        ),
        running: Some(true),
        error: None,
    }));
    tui.observe_current_agent_state(0);
    assert!(tui.apply_agent_state(0, AgentState::ExitedError, true));
    tui.desktop_notifier.flush();
    assert_eq!(notifications.lock().unwrap().len(), 1);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn tmux_warmup_liveness_can_report_a_distinct_completion() {
    let temp = unique_temp_dir("prism-tmux-completion-notification-test");
    fs::create_dir_all(&temp).unwrap();
    let mut config = test_config();
    config.notifications.enabled = true;
    config.notifications.completed = true;
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(temp.join("worktree"), "feature");
    session.agent_state = AgentState::ExitedError;
    let mut tui = Tui::new_single(repo, config, vec![session]);
    let (notifier, notifications) = crate::desktop_notification::DesktopNotifier::recording();
    tui.desktop_notifier = notifier;
    tui.reseed_desktop_notifications();
    let slot = AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
    tui.tmux_generations.insert(slot.clone(), 1);

    assert!(tui.apply_tmux_warmup_result(AgentSessionWarmupResult {
        key: AgentSessionWarmupKey::new(slot.clone(), 1),
        running: Some(true),
        error: None,
    }));
    assert_eq!(tui.sessions[0].agent_state, AgentState::Attached);
    assert!(tui.apply_tmux_warmup_result(AgentSessionWarmupResult {
        key: AgentSessionWarmupKey::new(slot, 1),
        running: Some(false),
        error: None,
    }));
    assert_eq!(tui.sessions[0].agent_state, AgentState::ExitedOk);

    tui.desktop_notifier.flush();
    assert_eq!(notifications.lock().unwrap().len(), 1);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn worktrunk_columns_reject_deleted_and_recreated_session_result() {
    let temp = unique_temp_dir("prism-wt-recreated-session-result-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(temp.join("worktree"), "feature");
    session.incarnation = "old".to_string();
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    let stale_key = tui.sessions[0].identity_key(&tui.repos[0].identity);
    tui.sessions[0].incarnation = "new".to_string();
    let facts = BTreeMap::from([(
        stale_key,
        crate::worktrunk::WorktrunkWorktreeFacts {
            extra_columns: BTreeMap::from([("ci".to_string(), "passed".to_string())]),
            ..crate::worktrunk::WorktrunkWorktreeFacts::default()
        },
    )]);

    tui.wt_poll_tx
        .send(WtPollResult {
            repository: tui.repos[0].identity.clone(),
            observation: Ok(WtObservation {
                snapshot: crate::worktrunk::WorktrunkSnapshot {
                    schema: crate::worktrunk::WorktrunkSchema::V1,
                    by_path: BTreeMap::new(),
                },
                facts,
                observed_at: std::time::Instant::now(),
            }),
        })
        .unwrap();

    assert!(!tui.poll_wt_columns());
    assert!(tui.sessions[0].wt_columns.is_empty());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn worktrunk_failure_preserves_successful_columns_as_stale() {
    let temp = unique_temp_dir("prism-wt-stale-observation-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "feature");
    let mut config = test_config();
    config
        .tools
        .insert("wt".to_string(), "/definitely/missing/wt".to_string());
    let failure = crate::worktrunk::observe_repository(&repo, &config).unwrap_err();
    let mut tui = Tui::new_single(repo, config, vec![session]);
    let key = tui.sessions[0].identity_key(&tui.repos[0].identity);
    let observed_at = std::time::Instant::now();
    let facts = crate::worktrunk::WorktrunkWorktreeFacts {
        extra_columns: BTreeMap::from([("url".to_string(), "http://localhost:3000".to_string())]),
        ..crate::worktrunk::WorktrunkWorktreeFacts::default()
    };
    tui.wt_poll_tx
        .send(WtPollResult {
            repository: tui.repos[0].identity.clone(),
            observation: Ok(WtObservation {
                snapshot: crate::worktrunk::WorktrunkSnapshot {
                    schema: crate::worktrunk::WorktrunkSchema::V1,
                    by_path: BTreeMap::from([(tui.sessions[0].path.clone(), facts.clone())]),
                },
                facts: BTreeMap::from([(key, facts)]),
                observed_at,
            }),
        })
        .unwrap();
    assert!(tui.poll_wt_columns());

    tui.wt_poll_tx
        .send(WtPollResult {
            repository: tui.repos[0].identity.clone(),
            observation: Err(failure),
        })
        .unwrap();

    assert!(tui.poll_wt_columns());
    assert_eq!(tui.sessions[0].wt_columns["url"], "http://localhost:3000");
    assert!(matches!(
        tui.repos[0].wt_quality,
        crate::worktrunk::ObservationQuality::Stale { last_success, .. }
            if last_success == observed_at
    ));
}

#[test]
fn stale_development_url_reports_generic_feedback_without_the_cached_url() {
    let cached_url = "http://localhost:3000/private-token-123";
    let message = development_url_opened_message(true);
    assert_eq!(
        message,
        "opened development URL from stale Worktrunk observation"
    );
    assert!(!message.contains(cached_url));
}

#[test]
fn worktrunk_refresh_requests_coalesce_while_poll_is_in_flight() {
    let temp = unique_temp_dir("prism-wt-coalesced-refresh-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut tui = Tui::new_single(repo, test_config(), Vec::new());
    tui.repos[0].wt_poll_in_flight = true;

    tui.start_wt_column_poll();
    tui.start_wt_column_poll();

    assert!(tui.repos[0].wt_poll_pending);
    assert!(tui.repos[0].wt_poll_in_flight);
}

#[test]
fn worktrunk_hook_log_inventory_is_repository_scoped_and_preserves_stale_entries() {
    let temp = unique_temp_dir("prism-wt-hook-log-cache-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut tui = Tui::new_single(repo, test_config(), Vec::new());
    assert!(matches!(
        tui.repos[0].wt_hook_logs.quality,
        crate::worktrunk::ObservationQuality::NeverLoaded
    ));
    let repository = tui.repos[0].identity.clone();
    let entry = crate::worktrunk::HookLogEntry {
        path: temp.join(".git/wt/logs/dev.log"),
        branch: "feature/cache".to_string(),
        source: "project".to_string(),
        hook_type: Some("post-start".to_string()),
        name: "dev".to_string(),
        modified_at: "2026-01-01T00:00:00Z".to_string(),
        size: 12,
    };
    tui.wt_hook_log_poll_tx
        .send(crate::tui::WtHookLogPollResult {
            repository,
            observation: Ok(crate::tui::WtHookLogObservation {
                entries: vec![entry.clone()],
                observed_at: Instant::now(),
            }),
        })
        .unwrap();

    assert!(tui.poll_wt_hook_logs());
    assert_eq!(tui.repos[0].wt_hook_logs.entries, vec![entry]);
    assert!(matches!(
        tui.repos[0].wt_hook_logs.quality,
        crate::worktrunk::ObservationQuality::Fresh
    ));

    assert!(tui.mark_wt_hook_logs_stale(0, "refresh failed".to_string()));
    assert_eq!(tui.repos[0].wt_hook_logs.entries.len(), 1);
    assert!(matches!(
        tui.repos[0].wt_hook_logs.quality,
        crate::worktrunk::ObservationQuality::Stale { .. }
    ));
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn worktrunk_hook_log_refresh_coalesces_while_in_flight() {
    let temp = unique_temp_dir("prism-wt-hook-log-coalesce-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut tui = Tui::new_single(repo, test_config(), Vec::new());
    tui.repos[0].wt_hook_logs.refresh_in_flight = true;

    tui.request_wt_hook_log_refresh(0);
    tui.request_wt_hook_log_refresh(0);

    assert!(tui.repos[0].wt_hook_logs.refresh_in_flight);
    assert!(tui.repos[0].wt_hook_logs.refresh_pending);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn default_branch_result_is_rejected_after_default_branch_config_changes() {
    let temp = unique_temp_dir("prism-default-branch-config-result-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    config.default_base = Some("main".to_string());
    let session = test_session(temp.join("worktree"), "main");
    let mut tui = Tui::new_single(repo, config, vec![session]);
    let key = tui.sessions[0].identity_key(&tui.repos[0].identity);
    tui.repos[0].config.default_base = Some("develop".to_string());

    tui.default_branch_poll_tx
        .send(DefaultBranchPollResult {
            key,
            status_label: Ok("behind 3".to_string()),
        })
        .unwrap();

    assert!(!tui.poll_default_branch_status());
    assert_eq!(tui.sessions[0].status_label, "clean");
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn attach_creates_session_when_pre_attach_resize_finds_it_missing() {
    let temp = unique_temp_dir("prism-tmux-missing-before-attach-test");
    fs::create_dir_all(&temp).unwrap();
    let log = temp.join("tmux.log");
    let state = temp.join("state");
    let tmux = temp.join("tmux");
    fs::write(
        &tmux,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
state="$(cat '{}' 2>/dev/null || echo missing)"
case "$1" in
  resize-window)
    [ "$state" = exists ] || {{
      echo "can't find session: prism-missing" >&2
      exit 1
    }}
    ;;
  has-session)
    [ "$state" = exists ]
    exit $?
    ;;
  new-session)
    echo exists > '{}'
    ;;
  display-message)
    echo opencode
    ;;
  list-windows)
    echo 1
    ;;
esac
exit 0
"#,
            log.display(),
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

    tui.prepare_tmux_session_for_attach(0, (120, 39)).unwrap();
    tui.attach_tmux_session_for_index(0).unwrap();

    let commands = fs::read_to_string(&log).unwrap();
    assert!(commands.contains("resize-window -x 120 -y 39"));
    assert!(commands.contains("new-session -d -s"));
    assert!(commands.contains("attach-session -t"));

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
    tui.focused_panel = PanelFocus::Worktrees;
    tui.tmux_portal_size = Some((72, 18));

    tui.prepare_tmux_session_for_attach(0, (120, 39)).unwrap();
    tui.attach_tmux_session_for_index(0).unwrap();

    let wait_started = Instant::now();
    while !tui.tmux_warmups_in_flight.is_empty() && wait_started.elapsed() < Duration::from_secs(5)
    {
        tui.poll_tmux_agent_warmup();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(tui.tmux_warmups_in_flight.is_empty());
    let commands = fs::read_to_string(&log).unwrap();
    assert!(
        commands.find("resize-window -x 120 -y 39").unwrap()
            < commands.find("attach-session -t").unwrap(),
        "agent window should match the terminal before attach"
    );
    assert!(
        commands.find("attach-session -t").unwrap()
            < commands.rfind("resize-window -x 72 -y 18").unwrap(),
        "portal should resize the agent window immediately after detach"
    );
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
        incarnation: String::new(),
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

fn test_opencode_stream(tui: &Tui) -> OpencodeListenerKey {
    let worktree = tui.sessions[0].identity_key(&tui.repos[0].identity);
    OpencodeListenerKey {
        generation: tui
            .worktree_generations
            .get(&worktree)
            .copied()
            .unwrap_or_default(),
        worktree,
        session_id: "ses_1".to_string(),
        server_url: "http://127.0.0.1:41000".to_string(),
    }
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

fn phase_1_pr_summary(head_sha: &str) -> PrSummary {
    let repository = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.com", None).unwrap(),
        "example/repo",
    )
    .unwrap();
    PrSummary {
        number: 42,
        change_request_identity: Some(crate::remote::CanonicalChangeRequestIdentity::new(
            &repository,
            &crate::remote::NativeChangeRequestId::new("PR_42").unwrap(),
            &repository,
            &repository,
        )),
        native_state_evidence: crate::remote::NativeStateEvidence::default(),
        title: "Phase 1 safety".to_string(),
        author: "author".to_string(),
        body: String::new(),
        url: "https://github.com/example/repo/pull/42".to_string(),
        state: "OPEN".to_string(),
        review_decision: "CHANGES_REQUESTED".to_string(),
        requested_reviewers: Vec::new(),
        head_ref: "feature".to_string(),
        base_ref: "main".to_string(),
        head_sha: head_sha.to_string(),
        updated_at: "2026-07-13T12:00:00Z".to_string(),
        check_status: "passed".to_string(),
        merge_state_status: "CLEAN".to_string(),
        queue_state: "not_queued".to_string(),
        comment_count: 1,
        merged: false,
        draft: false,
    }
}
