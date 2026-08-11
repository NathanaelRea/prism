use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::agent::AgentState;
use crate::opencode::{OpencodeState, OpencodeStatus, parse_event_payload};
use crate::repo::Repository;
use crate::session::WorktreeRepositoryKey;
use crate::tui_jobs::{CoalescedFacet, JobRegistry};

use super::super::{PrPollKey, Tui, TuiJobKey, TuiJobKind};
use super::support::{test_config, test_session, unique_temp_dir};

#[test]
fn running_agent_does_not_block_quit() {
    let repo = Repository {
        root: PathBuf::from("/tmp/repo"),
    };
    let mut session = test_session(0, "/tmp/repo", "feature");
    session.agent_state = AgentState::Running;
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);

    assert!(tui.confirm_quit().unwrap());
    assert!(tui.dialog.is_none());
}

#[test]
fn shutdown_notification_requests_the_matching_run_loop_exit_path() {
    let notification = crate::tui_signal::ShutdownNotification::for_test();
    assert_eq!(super::super::requested_shutdown(&notification), None);

    notification.request_for_test(crate::tui_signal::ShutdownSignal::Sigterm);

    assert_eq!(
        super::super::requested_shutdown(&notification),
        Some(super::super::ShutdownReason::Sigterm)
    );
}

#[test]
fn opencode_in_flight_clears_after_panic_and_spawn_failure_then_restarts() {
    let _ = crate::observability::take_captured_events();
    let temp = unique_temp_dir("prism-tui-job-recovery-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    let key = super::super::OpencodePollKey::for_repository_session(
        &tui.repos[0].identity,
        &tui.sessions[0],
    );

    tui.opencode_polls_in_flight.insert(key.clone());
    tui.spawn_tui_job(
        TuiJobKind::OpencodePoll,
        TuiJobKey::Opencode(key.clone()),
        key.generation,
        Some(Duration::from_secs(1)),
        "panic-before-result".to_string(),
        |_| panic!("before result"),
    );
    wait_for_opencode_job(&mut tui, &key);
    assert!(!tui.opencode_polls_in_flight.contains(&key));

    tui.opencode_polls_in_flight.insert(key.clone());
    tui.jobs.fail_next_spawn();
    tui.spawn_tui_job(
        TuiJobKind::OpencodePoll,
        TuiJobKey::Opencode(key.clone()),
        key.generation,
        Some(Duration::from_secs(1)),
        "spawn-failure".to_string(),
        |_| Ok(None),
    );
    wait_for_opencode_job(&mut tui, &key);
    assert!(!tui.opencode_polls_in_flight.contains(&key));

    tui.opencode_polls_in_flight.insert(key.clone());
    tui.spawn_tui_job(
        TuiJobKind::OpencodePoll,
        TuiJobKey::Opencode(key.clone()),
        key.generation,
        Some(Duration::from_secs(1)),
        "restart-after-failure".to_string(),
        |_| Ok(None),
    );
    wait_for_opencode_job(&mut tui, &key);
    assert!(!tui.opencode_polls_in_flight.contains(&key));

    let deadline = Instant::now() + Duration::from_secs(1);
    let mut terminal_events = Vec::new();
    while terminal_events.len() < 3 && Instant::now() < deadline {
        terminal_events.extend(
            crate::observability::take_captured_events()
                .into_iter()
                .filter(|event| event.target == "tui_job" && event.action == "terminal")
                .filter_map(|event| event.data_json)
                .map(|data| serde_json::from_str::<serde_json::Value>(&data).unwrap())
                .filter(|data| data["kind"] == "opencode_poll")
                .filter(|data| {
                    data["key"]
                        .as_str()
                        .is_some_and(|key| key.contains(&temp.display().to_string()))
                }),
        );
        if terminal_events.len() < 3 {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    for (job_id, outcome) in [(1, "panicked"), (2, "spawn_failed"), (3, "completed")] {
        let matching = terminal_events
            .iter()
            .filter(|data| data["job_id"] == job_id && data["outcome"] == outcome)
            .count();
        assert_eq!(matching, 1, "job {job_id} outcome {outcome}");
    }

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn tui_tick_terminal_budget_retains_every_remaining_outcome() {
    let repo = Repository {
        root: PathBuf::from("/tmp/repo"),
    };
    let mut tui = Tui::new_single(repo, test_config(), Vec::new());
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(101));
    let completed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for index in 0..100 {
        let barrier = barrier.clone();
        let completed = completed.clone();
        tui.spawn_tui_job(
            TuiJobKind::WorkflowMaintenance,
            TuiJobKey::None,
            0,
            None,
            format!("budget-{index}"),
            move |_| {
                barrier.wait();
                completed.fetch_add(1, std::sync::atomic::Ordering::Release);
                Ok(None)
            },
        );
    }
    barrier.wait();
    while completed.load(std::sync::atomic::Ordering::Acquire) != 100 {
        std::thread::yield_now();
    }
    while !tui.jobs.active_metadata().is_empty() {
        tui.jobs.collect_finished();
        std::thread::yield_now();
    }

    tui.route_tui_job_messages();

    assert_eq!(
        tui.jobs.queue_stats().terminal_depth,
        100 - super::super::TUI_TICK_ITEM_BUDGET
    );
}

#[test]
fn opencode_snapshot_burst_converges_through_bounded_coalesced_slots() {
    let temp = unique_temp_dir("prism-opencode-coalesced-burst-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    session.agent_state = AgentState::Running;
    session.opencode_status = Some(OpencodeStatus {
        server_url: Some("http://127.0.0.1:1".to_string()),
        session_id: Some("ses_1".to_string()),
        title: None,
        state: OpencodeState::Busy,
        detail: None,
        latest_message: None,
        latest_user_message: None,
        recent_messages: Vec::new(),
        active_tool: None,
        todos: Vec::new(),
        last_updated_unix_ms: None,
    });
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    tui.jobs = JobRegistry::with_event_capacity(2);
    let worktree = tui.sessions[0].identity_key(&tui.repos[0].identity);
    let stream = super::super::OpencodeListenerKey {
        worktree: worktree.clone(),
        generation: 0,
        session_id: "ses_1".to_string(),
        server_url: "http://127.0.0.1:1".to_string(),
    };
    tui.opencode_listeners.insert(stream.clone());
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
    let job_stream = stream.clone();
    let job_id = tui.jobs.spawn(
            TuiJobKind::OpencodeListener,
            TuiJobKey::OpencodeListener(stream),
            0,
            None,
            "coalesced-listener".to_string(),
            move |context| {
                let send = |context: &crate::tui_jobs::JobContext<_, _, _>, facet, event| {
                    context.send_coalesced(
                        facet,
                        super::super::TuiJobPayload::OpencodeEvent(super::super::OpencodeEventResult {
                            stream: job_stream.clone(),
                            received_at: Instant::now(),
                            event: Ok(event),
                        }),
                    )
                };
                send(
                    &context,
                    CoalescedFacet::Status,
                    parse_event_payload(
                        r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":"busy"}}"#,
                    )
                    .unwrap(),
                )?;
                send(
                    &context,
                    CoalescedFacet::Message,
                    parse_event_payload(
                        r#"{"type":"message.part.updated","properties":{"sessionID":"ses_1","role":"assistant","text":"initial"}}"#,
                    )
                    .unwrap(),
                )?;
                for index in 0..100 {
                    let state = if index == 99 { "retry" } else { "busy" };
                    send(
                        &context,
                        CoalescedFacet::Status,
                        parse_event_payload(&format!(
                            r#"{{"type":"session.status","properties":{{"sessionID":"ses_1","status":"{state}"}}}}"#
                        ))
                        .unwrap(),
                    )?;
                    send(
                        &context,
                        CoalescedFacet::Message,
                        parse_event_payload(&format!(
                            r#"{{"type":"message.part.updated","properties":{{"sessionID":"ses_1","role":"assistant","text":"message-{index}"}}}}"#
                        ))
                        .unwrap(),
                    )?;
                }
                ready_tx.send(()).unwrap();
                while !context.wait(Duration::from_secs(60)) {}
                Ok(None)
            },
        );
    ready_rx.recv().unwrap();

    tui.route_tui_job_messages();

    let status = tui.sessions[0].opencode_status.as_ref().unwrap();
    assert_eq!(status.state, OpencodeState::Retry);
    assert_eq!(status.latest_message.as_deref(), Some("message-99"));
    assert!(tui.opencode_reconcile_requested.contains_key(&worktree));
    let stats = tui.jobs.queue_stats();
    assert_eq!(stats.event_capacity, 2);
    assert_eq!(stats.event_depth, 0);
    assert_eq!(stats.coalesced_depth, 0);
    assert_eq!(stats.coalesced_capacity, 2);
    assert_eq!(stats.overflow_total, 200);
    assert_eq!(stats.coalesced_total, 198);

    tui.jobs.cancel(job_id);
    while tui.jobs.has_jobs() {
        tui.route_tui_job_messages();
        std::thread::yield_now();
    }
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn opencode_overflow_requests_full_reconciliation_and_stale_events_cannot_regress_it() {
    let temp = unique_temp_dir("prism-opencode-overflow-reconcile-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    session.agent_state = AgentState::Running;
    session.opencode_status = Some(OpencodeStatus {
        server_url: Some("http://127.0.0.1:1".to_string()),
        session_id: Some("ses_1".to_string()),
        title: None,
        state: OpencodeState::Busy,
        detail: None,
        latest_message: None,
        latest_user_message: None,
        recent_messages: Vec::new(),
        active_tool: None,
        todos: Vec::new(),
        last_updated_unix_ms: None,
    });
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    let worktree = tui.sessions[0].identity_key(&tui.repos[0].identity);
    let stream = super::super::OpencodeListenerKey {
        worktree: worktree.clone(),
        generation: 0,
        session_id: "ses_1".to_string(),
        server_url: "http://127.0.0.1:1".to_string(),
    };
    tui.opencode_listeners.insert(stream.clone());
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    let job_stream = stream.clone();
    tui.spawn_tui_job(
            TuiJobKind::OpencodeListener,
            TuiJobKey::OpencodeListener(stream),
            0,
            None,
            "overflow-listener".to_string(),
            move |context| {
                for _ in 0..1_000 {
                    context.send(super::super::TuiJobPayload::OpencodeEvent(
                        super::super::OpencodeEventResult {
                            stream: job_stream.clone(),
                            received_at: Instant::now(),
                            event: Ok(parse_event_payload(
                                r#"{"type":"todo.updated","properties":{"sessionID":"ses_1","todos":[{"content":"ordered","status":"pending"}]}}"#,
                            )
                            .unwrap()),
                        },
                    ))?;
                }
                context.send_coalesced(
                    CoalescedFacet::Status,
                    super::super::TuiJobPayload::OpencodeEvent(super::super::OpencodeEventResult {
                        stream: job_stream.clone(),
                        received_at: Instant::now(),
                        event: Ok(parse_event_payload(
                            r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":"error"}}"#,
                        )
                        .unwrap()),
                    }),
                )?;
                context.send_coalesced(
                    CoalescedFacet::Message,
                    super::super::TuiJobPayload::OpencodeEvent(super::super::OpencodeEventResult {
                        stream: job_stream.clone(),
                        received_at: Instant::now(),
                        event: Ok(parse_event_payload(
                            r#"{"type":"message.part.updated","properties":{"sessionID":"ses_1","role":"assistant","text":"stale message"}}"#,
                        )
                        .unwrap()),
                    }),
                )?;
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(None)
            },
        );
    ready_rx.recv().unwrap();

    tui.route_tui_job_messages();
    let requested_at = *tui.opencode_reconcile_requested.get(&worktree).unwrap();
    let stats = tui.jobs.queue_stats();
    assert_eq!(stats.overflow_total, 1_002 - stats.event_capacity as u64);
    assert!(stats.event_depth <= stats.event_capacity);
    assert_eq!(stats.coalesced_depth, 2);

    let poll_key = super::super::OpencodePollKey::for_repository_session(
        &tui.repos[0].identity,
        &tui.sessions[0],
    );
    tui.opencode_poll_tx
        .send(super::super::OpencodePollResult {
            key: poll_key,
            started_at: requested_at + Duration::from_nanos(1),
            status: Ok(OpencodeStatus {
                state: OpencodeState::Done,
                latest_message: Some("fresh poll message".to_string()),
                ..tui.sessions[0].opencode_status.clone().unwrap()
            }),
        })
        .unwrap();
    tui.poll_opencode_status();
    assert!(!tui.opencode_reconcile_requested.contains_key(&worktree));

    for _ in 0..16 {
        tui.route_tui_job_messages();
        tui.poll_opencode_events();
    }
    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().state,
        OpencodeState::Done
    );
    assert_eq!(
        tui.sessions[0]
            .opencode_status
            .as_ref()
            .unwrap()
            .latest_message
            .as_deref(),
        Some("fresh poll message")
    );
    assert_eq!(tui.jobs.queue_stats().coalesced_depth, 0);

    release_tx.send(()).unwrap();
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn stale_opencode_job_payload_is_rejected_after_generation_changes() {
    let temp = unique_temp_dir("prism-tui-job-generation-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    let session_key = tui.sessions[0].identity_key(&tui.repos[0].identity);
    let key = super::super::OpencodePollKey::for_repository_session(
        &tui.repos[0].identity,
        &tui.sessions[0],
    );
    tui.opencode_polls_in_flight.insert(key.clone());
    let payload_key = key.clone();
    tui.spawn_tui_job(
        TuiJobKind::OpencodePoll,
        TuiJobKey::Opencode(key.clone()),
        key.generation,
        Some(Duration::from_secs(1)),
        "stale-opencode-poll".to_string(),
        move |_| {
            Ok(Some(super::super::TuiJobPayload::OpencodePoll(
                super::super::OpencodePollResult {
                    key: payload_key,
                    started_at: Instant::now(),
                    status: Ok(crate::opencode::OpencodeStatus {
                        server_url: None,
                        session_id: None,
                        title: None,
                        state: crate::opencode::OpencodeState::Busy,
                        detail: None,
                        latest_message: None,
                        latest_user_message: None,
                        recent_messages: Vec::new(),
                        active_tool: None,
                        todos: Vec::new(),
                        last_updated_unix_ms: None,
                    }),
                },
            )))
        },
    );
    *tui.worktree_generations.get_mut(&session_key).unwrap() = 1;

    wait_for_opencode_job(&mut tui, &key);
    tui.poll_opencode_status();

    assert!(tui.sessions[0].opencode_status.is_none());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn cleanup_cancels_and_joins_listener_job() {
    let _ = crate::observability::take_captured_events();
    let (mut tui, stopped_rx) = tui_with_active_listener("user-quit");

    tui.cleanup_tui_jobs(super::super::ShutdownReason::UserQuit)
        .unwrap();

    stopped_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(!tui.jobs.has_jobs());
    assert!(tui.opencode_listeners.is_empty());
    let cleanup = crate::observability::take_captured_events()
        .into_iter()
        .filter(|event| event.target == "tui" && event.action == "shutdown_cleanup")
        .filter_map(|event| event.data_json)
        .map(|data| serde_json::from_str::<serde_json::Value>(&data).unwrap())
        .find(|data| data["reason"] == "user_quit" && data["active_jobs"] == 1)
        .unwrap();
    assert_eq!(cleanup["reason"], "user_quit");
    assert_eq!(cleanup["active_jobs"], 1);
    assert_eq!(cleanup["unfinished_jobs"], 0);
}

#[test]
fn run_error_path_cleans_up_active_listener() {
    let (mut tui, stopped_rx) = tui_with_active_listener("run-error");

    let error = tui
        .finish_run(Ok(Err("injected draw error".to_string())), None)
        .unwrap_err();

    assert_eq!(error, "injected draw error");
    stopped_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(!tui.jobs.has_jobs());
    assert!(tui.opencode_listeners.is_empty());
}

#[test]
fn sigterm_exit_path_cleans_up_active_listener() {
    let (mut tui, stopped_rx) = tui_with_active_listener("sigterm");
    let notification = crate::tui_signal::ShutdownNotification::for_test();
    notification.request_for_test(crate::tui_signal::ShutdownSignal::Sigterm);
    assert_eq!(
        super::super::requested_shutdown(&notification),
        Some(super::super::ShutdownReason::Sigterm)
    );

    tui.finish_run(
        Ok(Err("interactive subprocess canceled".to_string())),
        notification.signal(),
    )
    .unwrap();

    stopped_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(!tui.jobs.has_jobs());
    assert!(tui.opencode_listeners.is_empty());
}

fn tui_with_active_listener(label: &str) -> (Tui, std::sync::mpsc::Receiver<()>) {
    let repo = Repository {
        root: PathBuf::from(format!("/tmp/prism-cleanup-{label}")),
    };
    let session = test_session(
        0,
        &format!("/tmp/prism-cleanup-{label}/worktree"),
        "feature",
    );
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    let key = tui.sessions[0].identity_key(&tui.repos[0].identity);
    let stream = super::super::OpencodeListenerKey {
        worktree: key,
        generation: 0,
        session_id: "ses_1".to_string(),
        server_url: "http://127.0.0.1:41000".to_string(),
    };
    let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
    tui.opencode_listeners.insert(stream.clone());
    tui.spawn_tui_job(
        TuiJobKind::OpencodeListener,
        TuiJobKey::OpencodeListener(stream),
        0,
        None,
        "cleanup-listener".to_string(),
        move |context| {
            while !context.wait(Duration::from_secs(60)) {}
            stopped_tx.send(()).unwrap();
            Ok(None)
        },
    );

    (tui, stopped_rx)
}

fn wait_for_opencode_job(tui: &mut Tui, key: &super::super::OpencodePollKey) {
    let started = Instant::now();
    while tui.opencode_polls_in_flight.contains(key) {
        tui.route_tui_job_messages();
        assert!(started.elapsed() < Duration::from_secs(1));
        std::thread::yield_now();
    }
}

#[test]
fn pr_poll_identity_uses_repository_and_worktree_generation_not_repo_order() {
    let repository = WorktreeRepositoryKey::new(PathBuf::from("/tmp/repo"));
    let mut session = test_session(0, "/tmp/repo", "feature");
    let first = PrPollKey::for_repository_session_generation(&repository, &session, 3);

    session.repo_index = 9;
    let reordered = PrPollKey::for_repository_session_generation(&repository, &session, 3);
    let recreated = PrPollKey::for_repository_session_generation(&repository, &session, 4);

    assert_eq!(first, reordered);
    assert_ne!(first, recreated);
}
