use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::text::Line;

use crate::agent::AgentState;
use crate::agent_session::{AgentSessionSlot, AgentSessionWarmupKey, AgentSessionWarmupResult};
use crate::auto_flow::stabilization_model::StabilizationBlocker;
use crate::config::Config;
use crate::opencode::{OpencodeState, OpencodeStatus};
use crate::plan_run::{PlanOutputKind, PlanOutputLine};
use crate::remote::{PrCache, PrDetails, PrReviewComment};
use crate::repo::Repository;

use super::super::{
    OpencodePollKey, OpencodePollResult, PanelFocus, PrPollKey, PrPollResult,
    PrSummarySessionResult, TmuxPortalCapture, TmuxPortalResult, TmuxPortalSnapshot, Tui,
};
use super::support::{
    test_auto_run, test_change_request_identity, test_config, test_plan_run_with_steps,
    test_pr_summary, test_session, unique_temp_dir,
};

#[test]
fn workflow_polling_does_not_access_database_on_tui_thread() {
    let temp = unique_temp_dir("prism-tui-database-poll-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    crate::observability::with_writable_db(&repo, |_| Ok(())).unwrap();
    let mut tui = Tui::new_single(repo, test_config(), Vec::new());

    crate::observability::deny_database_access_on_current_thread(|| {
        tui.tick_tui_action_jobs();
    });

    assert_eq!(tui.workflow_polls_in_flight.len(), 1);

    drop(tui);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn applying_pr_poll_result_does_no_io_on_tui_thread() {
    let temp = unique_temp_dir("prism-tui-pr-result-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    crate::observability::with_writable_db(&repo, |_| Ok(())).unwrap();
    let session = test_session(0, &temp.display().to_string(), "feature");
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    let repository = tui.repos[0].identity.clone();
    let session_key = tui.sessions[0].identity_key(&repository);
    let poll_started_at = Instant::now();
    tui.sessions[0].pr.begin_summary_poll(poll_started_at);
    tui.repos[0].pr_summary_last_polled = Some(poll_started_at);
    tui.repos[0].pr_summary_poll_in_flight = true;
    tui.pr_poll_tx
        .send(PrPollResult::Summary {
            repository: repository.clone(),
            sessions: vec![session_key.clone()],
            github_remote_configured: true,
            capabilities: Some(crate::remote::Capabilities::for_provider(
                crate::remote::ProviderKind::GitHub,
            )),
            summaries: Ok(vec![test_pr_summary(false)]),
            observations: Ok(vec![PrSummarySessionResult {
                key: session_key,
                summary: Some(test_pr_summary(false)),
            }]),
            remote_branch_heads: BTreeMap::new(),
            refreshed: "now".to_string(),
            poll_started_at,
        })
        .unwrap();
    let tmux_slot = AgentSessionSlot::for_repository_session(&repository, &tui.sessions[0]);
    tui.tmux_generations.insert(tmux_slot.clone(), 0);
    tui.tmux_warmup_tx
        .send(AgentSessionWarmupResult {
            key: AgentSessionWarmupKey::new(tmux_slot, 0),
            running: Some(true),
            error: None,
        })
        .unwrap();
    let opencode_key =
        OpencodePollKey::for_repository_session_generation(&repository, &tui.sessions[0], 0);
    tui.opencode_poll_tx
        .send(OpencodePollResult {
            key: opencode_key,
            started_at: Instant::now(),
            status: Ok(OpencodeStatus {
                server_url: Some("http://127.0.0.1:41000".to_string()),
                session_id: Some("ses_1".to_string()),
                title: None,
                state: OpencodeState::Busy,
                detail: None,
                latest_message: None,
                latest_user_message: None,
                recent_messages: Vec::new(),
                active_tool: None,
                todos: Vec::new(),
                last_updated_unix_ms: Some(1),
            }),
        })
        .unwrap();

    let changes = crate::flight_recorder::deny_external_calls_on_current_thread(|| {
        crate::observability::deny_database_access_on_current_thread(|| tui.tick_tui_action_jobs())
    });

    assert!(changes.pull_requests);
    assert_eq!(tui.sessions[0].pr.summary().unwrap().number, 1);
    assert_eq!(tui.sessions[0].agent_state, AgentState::Running);

    drop(tui);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn remote_review_update_replans_auto_flow_and_refreshes_worktree_status() {
    let temp = unique_temp_dir("prism-tui-remote-gate-replan-test");
    let worktree = temp.join("feature");
    fs::create_dir_all(&worktree).unwrap();
    let git = temp.join("git");
    fs::write(
        &git,
        "#!/bin/sh\ncase \"$*\" in *\"status --short --branch\"*) printf '## feature...origin/feature [ahead 1]\\n' ;; esac\n",
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
    let mut session = test_session(0, &temp.display().to_string(), "feature");
    session.status_label = "clean".to_string();
    let identity = test_change_request_identity(crate::remote::ProviderKind::GitHub);
    let mut requested_changes = test_pr_summary(false);
    requested_changes.change_request_identity = Some(identity.clone());
    requested_changes.review_decision = "CHANGES_REQUESTED".to_string();
    requested_changes.check_status = "passed".to_string();
    session.pr = PrCache::observed(
        requested_changes,
        Some(PrDetails {
            review_comments: vec![PrReviewComment {
                thread_id: "thread-1".to_string(),
                body: "please fix this".to_string(),
                resolved: false,
                ..PrReviewComment::default()
            }],
            ..PrDetails::default()
        }),
    );
    let mut tui = Tui::new_single(repo.clone(), config, vec![session]);

    let mut run = test_auto_run("auto", &worktree.display().to_string(), 1);
    run.run.repo_root = temp.display().to_string();
    run.run.branch = "feature".to_string();
    run.run.stabilization_blocker = Some(StabilizationBlocker::ReviewFeedbackFound);
    crate::observability::with_writable_db(&repo, |path| {
        let store = crate::auto_flow::AutoFlowStore::open(path);
        crate::auto_flow::save_auto_run(&store, &mut run)
    })
    .unwrap();
    tui.remember_auto_run(run);

    let repository = tui.repos[0].identity.clone();
    let session_key = tui.sessions[0].identity_key(&repository);
    let poll_started_at = Instant::now();
    tui.sessions[0].pr.begin_summary_poll(poll_started_at);
    let mut approved = test_pr_summary(false);
    approved.change_request_identity = Some(identity);
    approved.review_decision = "APPROVED".to_string();
    approved.check_status = "passed".to_string();
    tui.pr_poll_tx
        .send(PrPollResult::Summary {
            repository: repository.clone(),
            sessions: vec![session_key.clone()],
            github_remote_configured: true,
            capabilities: None,
            summaries: Ok(vec![approved.clone()]),
            observations: Ok(vec![PrSummarySessionResult {
                key: session_key,
                summary: Some(approved),
            }]),
            remote_branch_heads: BTreeMap::new(),
            refreshed: "now".to_string(),
            poll_started_at,
        })
        .unwrap();

    tui.drain_pr_poll_results();
    let wait_started = Instant::now();
    while (!tui.pr_persistence_in_flight.is_empty() || !tui.pr_persistence_pending.is_empty())
        && wait_started.elapsed() < Duration::from_secs(3)
    {
        std::thread::sleep(Duration::from_millis(10));
        tui.drain_pr_poll_results();
    }

    let generation = tui.worktree_generations[&tui.sessions[0].identity_key(&repository)];
    let key =
        PrPollKey::for_repository_session_generation(&repository, &tui.sessions[0], generation);
    let mut resolved_details = tui.sessions[0].pr.begin_details_poll();
    resolved_details.replace_details_for_test(PrDetails::default());
    tui.pr_poll_tx
        .send(PrPollResult::Details {
            key,
            cache: Box::new(resolved_details),
        })
        .unwrap();
    tui.drain_pr_poll_results();
    let wait_started = Instant::now();
    while (!tui.pr_persistence_in_flight.is_empty() || !tui.pr_persistence_pending.is_empty())
        && wait_started.elapsed() < Duration::from_secs(3)
    {
        std::thread::sleep(Duration::from_millis(10));
        tui.drain_pr_poll_results();
    }

    assert_eq!(tui.sessions[0].status_label, "ahead 1");
    assert_ne!(
        tui.auto_runs["auto"].run.stabilization_blocker,
        Some(StabilizationBlocker::ReviewFeedbackFound)
    );

    drop(tui);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn failed_pr_details_respect_retry_backoff_on_tui_tick() {
    let temp = unique_temp_dir("prism-tui-pr-details-backoff-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut tui = Tui::new_single(
        repo,
        test_config(),
        vec![test_session(0, &temp.display().to_string(), "feature")],
    );
    tui.focused_panel = PanelFocus::Worktrees;
    tui.sessions[0].pr = PrCache::observed(test_pr_summary(false), None);
    let mut failed_poll = tui.sessions[0].pr.begin_details_poll();
    crate::remote::dispatcher::refresh_change_request_details_state(
        "feature",
        &mut failed_poll,
        &tui.sessions[0].path,
        &tui.repos[0].config,
    );
    let repository = tui.repos[0].identity.clone();
    let generation = tui.worktree_generations[&tui.sessions[0].identity_key(&repository)];
    let key =
        PrPollKey::for_repository_session_generation(&repository, &tui.sessions[0], generation);
    tui.repos[0].pr_summary_last_polled = Some(Instant::now());
    tui.repos[0].pr_summary_poll_in_flight = true;
    tui.pr_poll_tx
        .send(PrPollResult::Details {
            key,
            cache: Box::new(failed_poll),
        })
        .unwrap();

    crate::flight_recorder::deny_external_calls_on_current_thread(|| {
        crate::observability::deny_database_access_on_current_thread(|| {
            tui.tick_tui_action_jobs();
        });
    });

    assert!(tui.pr_polls_in_flight.is_empty());
    assert!(tui.sessions[0].pr.details().is_none());

    drop(tui);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn repeated_dashboard_rendering_uses_only_cached_output() {
    let temp = unique_temp_dir("prism-tui-dashboard-database-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    crate::observability::with_writable_db(&repo, |_| Ok(())).unwrap();
    let session = test_session(0, &temp.display().to_string(), "feature");
    let scope_path = session.path.clone();
    let mut run = test_plan_run_with_steps("plan", &scope_path.display().to_string(), 1);
    run.run.repo_root = temp.display().to_string();
    crate::observability::with_writable_db(&repo, |path| {
        let store = crate::plan_run::PlanRunStore::open(path);
        crate::plan_run::save_plan_run(&store, &run)?;
        crate::plan_run::append_output_line(
            &store,
            &PlanOutputLine {
                run_id: "plan".to_string(),
                step: 1,
                line_number: 1,
                time_unix_ms: 1,
                kind: PlanOutputKind::Assistant,
                text: "cached output".to_string(),
                block_id: None,
            },
            100,
        )
    })
    .unwrap();
    let mut tui = Tui::new_single(repo.clone(), test_config(), vec![session]);
    tui.focused_panel = PanelFocus::Worktrees;
    tui.remember_plan_run(run);
    tui.plan_output_cache.borrow_mut().insert(
        ("plan".to_string(), 1),
        vec![PlanOutputLine {
            run_id: "plan".to_string(),
            step: 1,
            line_number: 1,
            time_unix_ms: 1,
            kind: PlanOutputKind::Assistant,
            text: "cached output".to_string(),
            block_id: None,
        }],
    );

    let dashboards = crate::observability::deny_database_access_on_current_thread(|| {
        (0..3)
            .map(|_| tui.current_plan_dashboard())
            .collect::<Vec<_>>()
    });

    assert!(
        dashboards.iter().all(|dashboard| {
            dashboard.as_ref().unwrap().output_lines[0].text == "cached output"
        })
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn returning_from_tmux_does_not_wait_for_worktree_refresh() {
    let temp = unique_temp_dir("prism-tmux-return-refresh-test");
    let worktree = temp.join("feature");
    fs::create_dir_all(&worktree).unwrap();
    fs::write(worktree.join(".git"), "gitdir: /tmp/gitdir\n").unwrap();
    let git = temp.join("git");
    let refresh_gate = temp.join("allow-refresh");
    fs::write(
        &git,
        format!(
            r#"#!/bin/sh
case "$*" in
  *"worktree list --porcelain"*)
    while [ ! -f {:?} ]; do sleep 0.1; done
    printf 'worktree {}\nHEAD abc\nbranch refs/heads/feature\n\n'
    ;;
  *"status --short --branch"*) printf '## feature\n' ;;
  *"remote get-url origin"*) printf 'git@github.com:owner/repo.git\n' ;;
esac
"#,
            refresh_gate.display().to_string(),
            worktree.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&git).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&git, permissions).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    fs::create_dir_all(repo.prism_dir()).unwrap();
    fs::write(
        repo.prism_dir().join("config.toml"),
        format!("[tools]\ngit = {:?}\n", git.display().to_string()),
    )
    .unwrap();
    let config = Config::load(&repo);
    let session = test_session(0, &temp.display().to_string(), "feature");
    let mut tui = Tui::new_single(repo, config, vec![session]);
    tui.focused_panel = PanelFocus::Worktrees;
    tui.tmux_portal_size = Some((72, 18));

    let started = Instant::now();
    crate::flight_recorder::deny_external_calls_on_current_thread(|| {
        crate::observability::deny_database_access_on_current_thread(|| {
            tui.refresh_sessions_after_tmux().unwrap();
            tui.refresh_sessions_after_tmux().unwrap();
            tui.poll_tmux_portal();
        });
    });
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(250),
        "returning from tmux waited for refresh for {elapsed:?}"
    );

    fs::write(refresh_gate, "").unwrap();
    let wait_started = Instant::now();
    while tui.session_refresh_in_flight && wait_started.elapsed() < Duration::from_secs(3) {
        crate::flight_recorder::deny_external_calls_on_current_thread(|| {
            crate::observability::deny_database_access_on_current_thread(|| {
                tui.poll_session_refresh();
            });
        });
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!tui.session_refresh_in_flight);
    assert!(!tui.session_refresh_pending);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn tmux_portal_rejects_capture_from_previous_generation() {
    let repo = Repository {
        root: PathBuf::from("/tmp/repo"),
    };
    let mut tui = Tui::new_single(
        repo,
        test_config(),
        vec![test_session(0, "/tmp/repo", "feature")],
    );
    tui.focused_panel = PanelFocus::Worktrees;
    tui.tmux_portal_size = Some((72, 18));
    tui.refresh_worktree_harness_configs();
    let slot = AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
    let stale_key = AgentSessionWarmupKey::new(slot.clone(), 0);
    let current_key = AgentSessionWarmupKey::new(slot.clone(), 1);
    tui.tmux_generations.insert(slot, 1);
    tui.tmux_portal_last_polled
        .insert(current_key.clone(), Instant::now());
    tui.tmux_portal_tx
        .send(TmuxPortalResult {
            key: stale_key,
            started_at: Instant::now(),
            capture: Ok(vec![Line::from("stale output")]),
            resized_size: None,
        })
        .unwrap();

    assert!(tui.poll_tmux_portal());
    assert_eq!(
        tui.tmux_portal.as_ref().map(|portal| &portal.key),
        Some(&current_key),
    );
    assert_eq!(
        tui.tmux_portal
            .as_ref()
            .and_then(|portal| portal.capture.as_ref()),
        None,
    );
}

#[test]
fn tmux_portal_starts_capture_immediately_after_selection() {
    let repo = Repository {
        root: PathBuf::from("/tmp/repo"),
    };
    let mut tui = Tui::new_single(
        repo,
        test_config(),
        vec![test_session(0, "/tmp/repo", "feature")],
    );
    tui.focused_panel = PanelFocus::Worktrees;
    tui.tmux_portal_size = Some((72, 18));
    tui.refresh_worktree_harness_configs();
    let slot = AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
    tui.tmux_generations.insert(slot, 0);

    assert!(tui.poll_tmux_portal());
    assert!(
        !tui.tmux_portal_polls_in_flight.is_empty(),
        "selecting a worktree should immediately start an asynchronous tmux capture"
    );
}

#[test]
fn workflow_database_writer_does_not_block_tmux_portal_polling() {
    let temp = unique_temp_dir("prism-tmux-portal-database-test");
    fs::create_dir_all(&temp).unwrap();
    let tmux = temp.join("tmux");
    fs::write(&tmux, "#!/bin/sh\nexit 1\n").unwrap();
    let mut permissions = fs::metadata(&tmux).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&tmux, permissions).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    config
        .tools
        .insert("tmux".to_string(), tmux.display().to_string());
    let session = test_session(0, &temp.display().to_string(), "feature");
    crate::session::worktree_harness(&repo, &session).unwrap();
    let mut tui = Tui::new_single(repo.clone(), config, vec![session]);
    tui.focused_panel = PanelFocus::Worktrees;
    tui.tmux_portal_size = Some((72, 18));
    tui.refresh_worktree_harness_configs();
    let slot = AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
    tui.tmux_generations.insert(slot, 0);
    let mut blocker = crate::persistence::database::TestConnection::open_writable(
        &crate::observability::db_path(&repo),
    )
    .unwrap();
    // Transaction mechanics belong to the test harness rather than a domain SQL file.
    blocker.execute_batch("begin exclusive").unwrap();

    let started = Instant::now();
    tui.tick_tui_action_jobs();
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(250),
        "worktree polling blocked input for {elapsed:?}"
    );

    drop(blocker);
    let _ = tui.tmux_portal_rx.recv_timeout(Duration::from_secs(1));
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn tmux_portal_resizes_once_for_unchanged_target_and_size() {
    let temp = unique_temp_dir("prism-tmux-portal-resize-test");
    fs::create_dir_all(&temp).unwrap();
    let log = temp.join("tmux.log");
    let tmux = temp.join("tmux");
    fs::write(
            &tmux,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {:?}\nif [ \"$1\" = capture-pane ]; then printf 'output\\n'; fi\nexit 0\n",
                log.display().to_string()
            ),
        )
        .unwrap();
    let mut permissions = fs::metadata(&tmux).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&tmux, permissions).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    config
        .tools
        .insert("tmux".to_string(), tmux.display().to_string());
    let session = test_session(0, &temp.display().to_string(), "feature");
    let mut tui = Tui::new_single(repo, config, vec![session]);
    tui.focused_panel = PanelFocus::Worktrees;
    tui.tmux_portal_size = Some((72, 18));
    tui.refresh_worktree_harness_configs();
    let slot = AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
    let key = AgentSessionWarmupKey::new(slot.clone(), 0);
    tui.tmux_generations.insert(slot, 0);

    tui.poll_tmux_portal();
    let started = Instant::now();
    loop {
        tui.route_tui_job_messages_with_budget(1, Instant::now());
        if tui.jobs.queue_stats().latest_depth == 1 {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        std::thread::yield_now();
    }

    // Model a loaded TUI tick whose job-routing budget expires after consuming the terminal
    // outcome but before routing its payload. The pending payload still owns this poll slot.
    tui.tui_tick_active = true;
    tui.poll_tmux_portal();
    tui.tui_tick_active = false;
    wait_for_tmux_portal_job(&mut tui);
    tui.tmux_portal_last_polled
        .insert(key, Instant::now() - Duration::from_secs(1));
    tui.poll_tmux_portal();
    wait_for_tmux_portal_job(&mut tui);

    let commands = fs::read_to_string(log).unwrap();
    assert_eq!(commands.matches("resize-window").count(), 1);
    assert_eq!(commands.matches("capture-pane").count(), 2);

    let _ = fs::remove_dir_all(temp);
}

fn wait_for_tmux_portal_job(tui: &mut Tui) {
    let started = Instant::now();
    while !tui.tmux_portal_polls_in_flight.is_empty() {
        tui.poll_tmux_portal();
        assert!(started.elapsed() < Duration::from_secs(1));
        std::thread::yield_now();
    }
}

#[test]
fn tmux_portal_keeps_previous_capture_while_new_selection_loads() {
    let repo = Repository {
        root: PathBuf::from("/tmp/repo"),
    };
    let mut tui = Tui::new_single(
        repo,
        test_config(),
        vec![
            test_session(0, "/tmp/repo-a", "feature-a"),
            test_session(0, "/tmp/repo-b", "feature-b"),
        ],
    );
    tui.focused_panel = PanelFocus::Worktrees;
    tui.tmux_portal_size = Some((72, 18));
    tui.refresh_worktree_harness_configs();
    let previous_slot =
        AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
    let selected_slot =
        AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[1]);
    tui.tmux_generations.insert(previous_slot.clone(), 0);
    tui.tmux_generations.insert(selected_slot.clone(), 0);
    tui.tmux_portal = Some(TmuxPortalSnapshot {
        key: AgentSessionWarmupKey::new(previous_slot.clone(), 0),
        capture: Some(TmuxPortalCapture {
            key: AgentSessionWarmupKey::new(previous_slot, 0),
            result: Ok(vec![Line::from("previous capture")]),
        }),
    });
    tui.select_worktree(1);

    let model = tui.tmux_portal_model().expect("tmux portal model");
    let crate::view::TmuxPortalState::Ready(lines) = model.state else {
        panic!("previous capture should survive the selection redraw");
    };
    assert_eq!(model.branch, "feature-a");
    assert_eq!(lines, &[Line::from("previous capture")]);

    assert!(tui.poll_tmux_portal());
    assert_eq!(
        tui.tmux_portal.as_ref().map(|portal| &portal.key.slot),
        Some(&selected_slot)
    );
    assert_eq!(
        tui.tmux_portal
            .as_ref()
            .and_then(|portal| portal.capture.as_ref())
            .and_then(|capture| capture.result.as_ref().ok()),
        Some(&vec![Line::from("previous capture")])
    );
    assert!(
        tui.tmux_portal_polls_in_flight
            .contains_key(&AgentSessionWarmupKey::new(selected_slot, 0))
    );
}

#[test]
fn tmux_portal_waits_for_running_capture_after_selection_changes() {
    let repo = Repository {
        root: PathBuf::from("/tmp/repo"),
    };
    let mut tui = Tui::new_single(
        repo,
        test_config(),
        vec![
            test_session(0, "/tmp/repo-a", "feature-a"),
            test_session(0, "/tmp/repo-b", "feature-b"),
        ],
    );
    tui.focused_panel = PanelFocus::Worktrees;
    tui.tmux_portal_size = Some((72, 18));
    tui.refresh_worktree_harness_configs();
    let first_slot =
        AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
    let second_slot =
        AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[1]);
    let first_key = AgentSessionWarmupKey::new(first_slot.clone(), 0);
    let second_key = AgentSessionWarmupKey::new(second_slot.clone(), 0);
    tui.tmux_generations.insert(first_slot, 0);
    tui.tmux_generations.insert(second_slot, 0);
    tui.tmux_portal_polls_in_flight
        .insert(first_key.clone(), Instant::now());
    tui.select_worktree(1);

    tui.poll_tmux_portal();

    assert!(tui.tmux_portal_polls_in_flight.contains_key(&first_key));
    assert!(!tui.tmux_portal_polls_in_flight.contains_key(&second_key));
}

#[test]
fn tmux_portal_tracks_in_flight_capture_when_inactive() {
    let repo = Repository {
        root: PathBuf::from("/tmp/repo"),
    };
    let mut tui = Tui::new_single(
        repo,
        test_config(),
        vec![test_session(0, "/tmp/repo", "feature")],
    );
    let slot = AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
    let key = AgentSessionWarmupKey::new(slot, 0);
    tui.tmux_portal_polls_in_flight
        .insert(key.clone(), Instant::now());

    assert!(!tui.poll_tmux_portal());
    assert!(tui.tmux_portal_polls_in_flight.contains_key(&key));
}

#[test]
fn tmux_portal_ignores_superseded_capture_for_same_key() {
    let repo = Repository {
        root: PathBuf::from("/tmp/repo"),
    };
    let mut tui = Tui::new_single(
        repo,
        test_config(),
        vec![test_session(0, "/tmp/repo", "feature")],
    );
    tui.focused_panel = PanelFocus::Worktrees;
    tui.tmux_portal_size = Some((72, 18));
    tui.refresh_worktree_harness_configs();
    let slot = AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
    let key = AgentSessionWarmupKey::new(slot.clone(), 0);
    tui.tmux_generations.insert(slot, 0);
    let previous_started_at = Instant::now();
    let current_started_at = previous_started_at + Duration::from_millis(1);
    tui.tmux_portal_polls_in_flight
        .insert(key.clone(), current_started_at);
    tui.tmux_portal_last_polled
        .insert(key.clone(), current_started_at);
    tui.tmux_portal_tx
        .send(TmuxPortalResult {
            key: key.clone(),
            started_at: previous_started_at,
            capture: Ok(vec![Line::from("superseded output")]),
            resized_size: None,
        })
        .unwrap();

    assert!(tui.poll_tmux_portal());
    assert_eq!(
        tui.tmux_portal_polls_in_flight.get(&key),
        Some(&current_started_at)
    );
    assert_eq!(
        tui.tmux_portal
            .as_ref()
            .and_then(|portal| portal.capture.as_ref()),
        None
    );
}
