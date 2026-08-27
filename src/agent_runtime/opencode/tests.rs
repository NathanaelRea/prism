#[cfg(test)]
// Cross-seam behavioral tests retained as integration coverage.
use crate::agent::AgentState;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::client::*;
use super::registry::*;
use super::server::*;
use super::*;

#[test]
fn server_url_maps_port_to_local_http_url() {
    assert_eq!(server_url(41_234), "http://127.0.0.1:41234");
}

#[test]
fn event_listener_stops_when_canceled_while_receiver_is_idle() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (ready_tx, ready_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 512];
        let _ = stream.read(&mut request).unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
            .unwrap();
        stream.flush().unwrap();
        ready_tx.send(()).unwrap();
        let _ = release_rx.recv();
    });
    let canceled = Arc::new(AtomicBool::new(false));
    let listener_canceled = canceled.clone();
    let (result_tx, result_rx) = mpsc::sync_channel(0);
    std::thread::spawn(move || {
        let result = listen_events_until(
            &url,
            || listener_canceled.load(Ordering::Acquire),
            |_| Ok(()),
        );
        result_tx.send(result).unwrap();
    });

    ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    canceled.store(true, Ordering::Release);
    assert!(
        result_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_ok()
    );
    release_tx.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn event_listener_reports_an_idle_stream_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 512];
        let _ = stream.read(&mut request).unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
            .unwrap();
        stream.flush().unwrap();
        std::thread::sleep(Duration::from_millis(250));
    });

    let started = Instant::now();
    let error = listen_event_payloads_with_stop(
        &url,
        Duration::from_millis(100),
        Duration::from_millis(20),
        Duration::from_millis(80),
        &mut || false,
        &mut |_| Ok(()),
    )
    .unwrap_err();

    assert!(error.contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(1));
    server.join().unwrap();
}

#[test]
fn event_listener_keeps_callback_errors_distinct_from_transport_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut buffer = [0_u8; 256];
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0, "client closed before completing request headers");
            request.extend_from_slice(&buffer[..count]);
        }
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {}\r\n\r\n",
            )
            .unwrap();
    });
    let mut metrics = SseMetrics::default();

    let failure = listen_event_payloads_with_stop_inner(
        &url,
        SseRequest {
            path: "/event",
            connect_timeout: Duration::from_millis(100),
            read_poll_interval: Duration::from_millis(20),
            inactivity_timeout: Duration::from_millis(100),
        },
        &mut || false,
        &mut |_| Err("HTTP invalid closed application error".to_string()),
        &mut metrics,
    )
    .unwrap_err();

    assert_eq!(failure.kind, SseFailureKind::Callback);
    assert_eq!(failure.kind.error_kind(), None);
    assert_eq!(metrics.payload_count, 1);
    server.join().unwrap();
}

#[test]
fn stored_server_args_match_requires_expected_host_and_port() {
    let args = [
        "/home/mockuser/.npm/bin/opencode",
        "serve",
        "--hostname",
        "127.0.0.1",
        "--port",
        "41234",
    ];

    assert!(stored_server_args_match(&args, 41_234));
    assert!(!stored_server_args_match(&args, 41_235));
    assert!(!stored_server_args_match(
        &[
            "/home/mockuser/.npm/bin/opencode",
            "serve",
            "--port",
            "41234"
        ],
        41_234,
    ));
}

#[tokio::test]
async fn stored_server_shutdown_reports_argument_inspection_failure() {
    let runtime = OpencodeRuntime {
        repo_root: "/repo".to_string(),
        harness_id: "opencode".to_string(),
        branch: "feature/test".to_string(),
        worktree_path: "/repo/worktree".to_string(),
        server_port: 41_234,
        server_url: "http://127.0.0.1:41234".to_string(),
        server_pid: Some(42),
        server_process_identity: Some(7),
        opencode_session_id: None,
        generation: 0,
        updated_unix_ms: 0,
    };

    let error = shutdown_stored_server_with(&runtime, |pid| {
        Err(crate::process::ProcessLifecycleError::Inspect {
            pid,
            source: std::io::Error::other("injected argument inspection failure"),
        })
    })
    .await
    .unwrap_err();

    assert!(error.contains("inspect stored opencode server 42 before shutdown"));
    assert!(error.contains("injected argument inspection failure"));
}

#[test]
fn allocate_port_uses_stored_healthy_port() {
    let port = allocate_port(
        "/repo",
        "/repo/wt",
        Some(41_111),
        41_000,
        1_000,
        |candidate| {
            if candidate == 41_111 {
                PortStatus::OpenCode
            } else {
                PortStatus::Free
            }
        },
    )
    .unwrap();

    assert_eq!(port, 41_111);
}

#[test]
fn allocate_port_skips_occupied_stored_port() {
    let derived = allocate_port("/repo", "/repo/wt", None, 41_000, 1_000, |_| {
        PortStatus::Free
    })
    .unwrap();
    let port = allocate_port(
        "/repo",
        "/repo/wt",
        Some(41_111),
        41_000,
        1_000,
        |candidate| {
            if candidate == 41_111 || candidate == derived {
                PortStatus::Occupied
            } else {
                PortStatus::Free
            }
        },
    )
    .unwrap();

    assert_eq!(port, derived + 1);
}

#[test]
fn allocate_port_skips_unstored_open_code_port() {
    let derived = allocate_port("/repo", "/repo/wt", None, 41_000, 1_000, |_| {
        PortStatus::Free
    })
    .unwrap();
    let port = allocate_port("/repo", "/repo/wt", None, 41_000, 1_000, |candidate| {
        if candidate == derived {
            PortStatus::OpenCode
        } else {
            PortStatus::Free
        }
    })
    .unwrap();

    assert_eq!(port, derived + 1);
}

#[test]
fn allocate_port_uses_configured_base_and_span() {
    let port = allocate_port("/repo", "/repo/wt", None, 45_000, 10, |_| PortStatus::Free).unwrap();

    assert!((45_000..45_010).contains(&port));
}

#[test]
fn allocate_port_wraps_and_stays_within_the_configured_range() {
    let first = allocate_port("/repo", "/repo/wt", None, 45_000, 3, |_| PortStatus::Free).unwrap();
    let mut visited = Vec::new();
    let port = allocate_port("/repo", "/repo/wt", None, 45_000, 3, |candidate| {
        visited.push(candidate);
        if candidate == first || candidate == 45_000 {
            PortStatus::Occupied
        } else {
            PortStatus::Free
        }
    })
    .unwrap();
    assert!((45_000..45_003).contains(&port));
    assert!(visited.iter().all(|port| (45_000..45_003).contains(port)));
}

#[test]
fn allocate_port_rejects_invalid_ranges_and_outside_stored_ports() {
    assert!(allocate_port("/repo", "/repo/wt", None, 0, 1, |_| PortStatus::Free).is_err());
    assert!(allocate_port("/repo", "/repo/wt", None, 65_535, 2, |_| PortStatus::Free).is_err());
    let port = allocate_port("/repo", "/repo/wt", Some(41_999), 45_000, 10, |_| {
        PortStatus::Free
    })
    .unwrap();
    assert!((45_000..45_010).contains(&port));
}

#[test]
fn runtime_metadata_round_trips_session_mapping() {
    let temp = unique_temp_dir("prism-opencode-runtime-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let worktree = temp.join("feature");
    let runtime = OpencodeRuntime {
        repo_root: temp.display().to_string(),
        harness_id: "opencode".to_string(),
        branch: "feature".to_string(),
        worktree_path: worktree.display().to_string(),
        server_port: 41_222,
        server_url: server_url(41_222),
        server_pid: Some(123),
        server_process_identity: Some(456),
        opencode_session_id: Some("ses_123".to_string()),
        generation: 7,
        updated_unix_ms: 42,
    };

    save_runtime(&repo, &runtime).unwrap();
    let loaded = load_runtime(&repo, "opencode", "feature", &worktree)
        .unwrap()
        .unwrap();

    assert_eq!(loaded, runtime);
    let _ = fs::remove_dir_all(temp);
}

#[tokio::test(flavor = "multi_thread")]
async fn worktrees_in_one_repository_reuse_one_healthy_server() {
    let temp = unique_temp_dir("prism-opencode-shared-server-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let first_worktree = temp.join("first");
    let second_worktree = temp.join("second");
    let (server_url, stop, server) = start_health_server(first_worktree.clone());
    let server_port = parse_localhost_url(&server_url).unwrap().1;
    let first = OpencodeRuntime {
        repo_root: temp.display().to_string(),
        harness_id: "opencode".to_string(),
        branch: "feature/first".to_string(),
        worktree_path: first_worktree.display().to_string(),
        server_port,
        server_url: server_url.clone(),
        server_pid: None,
        server_process_identity: None,
        opencode_session_id: Some("ses_first".to_string()),
        generation: 0,
        updated_unix_ms: 42,
    };
    save_runtime(&repo, &first).unwrap();

    let second = ensure_opencode_server_with_program(
        &repo,
        &Config::load(&repo),
        "opencode",
        "feature/second",
        &second_worktree,
        "/definitely/missing/opencode",
    )
    .await
    .unwrap();

    assert_eq!(second.server_url, first.server_url);
    assert_eq!(second.server_port, first.server_port);
    assert_eq!(second.server_pid, first.server_pid);
    stop.store(true, Ordering::Release);
    server.join().unwrap();
    let _ = fs::remove_dir_all(temp);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unhealthy_persisted_server_is_stopped_before_replacement() {
    use std::os::unix::process::CommandExt as _;

    let temp = unique_temp_dir("prism-opencode-unhealthy-server-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let worktree = temp.join("worktree");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg("while :; do sleep 1; done")
        .arg("opencode-fixture")
        .args([
            "serve",
            "--hostname",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ])
        .process_group(0);
    let mut child = command.spawn().unwrap();
    let recorded = crate::process::record_process(child.id()).unwrap();
    let identity = recorded
        .identity
        .expect("fixture should expose reusable process identity")
        .stored_value();
    let (exit_tx, exit_rx) = mpsc::sync_channel(1);
    let waiter = std::thread::spawn(move || {
        let _ = exit_tx.send(child.wait());
    });
    let runtime = OpencodeRuntime {
        repo_root: temp.display().to_string(),
        harness_id: "opencode".to_string(),
        branch: "feature".to_string(),
        worktree_path: worktree.display().to_string(),
        server_port: port,
        server_url: server_url(port),
        server_pid: Some(recorded.pid),
        server_process_identity: Some(identity),
        opencode_session_id: None,
        generation: 0,
        updated_unix_ms: 42,
    };
    let ready_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let arguments = crate::process::process_arguments(recorded.pid)
            .unwrap()
            .expect("fixture should still be running");
        let argument_refs = arguments.iter().map(String::as_str).collect::<Vec<_>>();
        if stored_server_args_match(&argument_refs, port)
            && stored_server_identity_is_valid(&runtime)
        {
            break;
        }
        if Instant::now() >= ready_deadline {
            let _ =
                crate::process::terminate_recorded_process(recorded, Duration::from_millis(100))
                    .await;
            let _ = exit_rx.recv_timeout(Duration::from_secs(1));
            waiter.join().unwrap();
            panic!("fixture arguments did not become reusable OpenCode identity: {arguments:?}");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    save_runtime(&repo, &runtime).unwrap();
    let mut config = Config::load(&repo);
    config.opencode_port_base = port;
    config.opencode_port_span = 1;

    let result = ensure_opencode_server_with_program(
        &repo,
        &config,
        "opencode",
        "feature",
        &worktree,
        "/definitely/missing/opencode",
    )
    .await;
    if exit_rx.recv_timeout(Duration::from_secs(2)).is_err() {
        let _ =
            crate::process::terminate_recorded_process(recorded, Duration::from_millis(100)).await;
        let _ = exit_rx.recv_timeout(Duration::from_secs(1));
        waiter.join().unwrap();
        panic!("unhealthy persisted server survived replacement");
    }
    waiter.join().unwrap();
    let error = result.expect_err("replacement fixture should fail after stale-server cleanup");
    assert!(error.contains("start opencode server"), "{error}");
    let _ = fs::remove_dir_all(temp);
}

#[cfg(windows)]
#[test]
#[ignore = "child fixture for Windows supervisor lifecycle coverage"]
fn windows_unhealthy_supervisor_fixture() {
    std::thread::sleep(Duration::from_secs(30));
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn windows_unhealthy_persisted_supervisor_is_stopped_before_replacement() {
    use std::os::windows::process::CommandExt as _;
    use windows::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

    let temp = unique_temp_dir("prism-opencode-windows-unhealthy-server-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let worktree = temp.join("worktree");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "agent_runtime::opencode::tests::windows_unhealthy_supervisor_fixture",
            "--ignored",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags((CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS).0);
    let mut child = command.spawn().unwrap();
    let recorded = crate::process::record_process(child.id()).unwrap();
    let identity = recorded
        .identity
        .expect("fixture should expose reusable process identity")
        .stored_value();
    let (exit_tx, exit_rx) = mpsc::sync_channel(1);
    let waiter = std::thread::spawn(move || {
        let _ = exit_tx.send(child.wait());
    });
    let runtime = OpencodeRuntime {
        repo_root: temp.display().to_string(),
        harness_id: "opencode".to_string(),
        branch: "feature".to_string(),
        worktree_path: worktree.display().to_string(),
        server_port: port,
        server_url: server_url(port),
        server_pid: Some(recorded.pid),
        server_process_identity: Some(identity),
        opencode_session_id: None,
        generation: 0,
        updated_unix_ms: 42,
    };
    let ready_deadline = Instant::now() + Duration::from_secs(1);
    while !stored_server_identity_is_valid(&runtime) {
        if Instant::now() >= ready_deadline {
            let _ =
                crate::process::terminate_recorded_process(recorded, Duration::from_millis(100))
                    .await;
            let _ = exit_rx.recv_timeout(Duration::from_secs(1));
            waiter.join().unwrap();
            panic!("Windows fixture did not become a reusable supervisor identity");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    save_runtime(&repo, &runtime).unwrap();
    let runtimes = load_runtimes_for_harness(&repo, "opencode").unwrap();

    let stored_port = stop_unhealthy_stored_servers(&runtimes).await.unwrap();

    assert_eq!(stored_port, Some(port));
    if exit_rx.recv_timeout(Duration::from_secs(2)).is_err() {
        let _ =
            crate::process::terminate_recorded_process(recorded, Duration::from_millis(100)).await;
        let _ = exit_rx.recv_timeout(Duration::from_secs(1));
        waiter.join().unwrap();
        panic!("unhealthy persisted Windows supervisor survived replacement cleanup");
    }
    waiter.join().unwrap();
    let _ = fs::remove_dir_all(temp);
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_worktree_servers_converge_to_one_canonical_server() {
    let temp = unique_temp_dir("prism-opencode-legacy-server-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let first_worktree = temp.join("first");
    let second_worktree = temp.join("second");
    let (first_url, first_stop, first_server) = start_health_server(first_worktree.clone());
    let (second_url, second_stop, second_server) = start_health_server(second_worktree.clone());
    let runtime =
        |branch: &str, worktree: &Path, server_url: String, session_id: &str| OpencodeRuntime {
            repo_root: temp.display().to_string(),
            harness_id: "opencode".to_string(),
            branch: branch.to_string(),
            worktree_path: worktree.display().to_string(),
            server_port: parse_localhost_url(&server_url).unwrap().1,
            server_url,
            server_pid: None,
            server_process_identity: None,
            opencode_session_id: Some(session_id.to_string()),
            generation: 0,
            updated_unix_ms: 42,
        };
    let first = runtime("feature/first", &first_worktree, first_url, "ses_first");
    let second = runtime("feature/second", &second_worktree, second_url, "ses_first");
    save_runtime(&repo, &first).unwrap();
    save_runtime(&repo, &second).unwrap();
    let canonical_url = [&first, &second]
        .into_iter()
        .min_by_key(|runtime| runtime.server_port)
        .unwrap()
        .server_url
        .clone();
    let noncanonical = [&first, &second]
        .into_iter()
        .find(|runtime| runtime.server_url != canonical_url)
        .unwrap();

    let selected = ensure_opencode_server_with_program(
        &repo,
        &Config::load(&repo),
        "opencode",
        &noncanonical.branch,
        Path::new(&noncanonical.worktree_path),
        "/definitely/missing/opencode",
    )
    .await
    .unwrap();

    assert_eq!(selected.server_url, canonical_url);
    for runtime in load_runtimes_for_harness(&repo, "opencode").unwrap() {
        assert_eq!(runtime.server_url, canonical_url);
    }
    first_stop.store(true, Ordering::Release);
    second_stop.store(true, Ordering::Release);
    first_server.join().unwrap();
    second_server.join().unwrap();
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn replacing_shared_server_updates_every_worktree_reference() {
    let temp = unique_temp_dir("prism-opencode-replacement-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let runtime = |branch: &str, worktree: &str, session_id: &str| OpencodeRuntime {
        repo_root: temp.display().to_string(),
        harness_id: "opencode".to_string(),
        branch: branch.to_string(),
        worktree_path: temp.join(worktree).display().to_string(),
        server_port: 41_000,
        server_url: server_url(41_000),
        server_pid: Some(100),
        server_process_identity: Some(200),
        opencode_session_id: Some(session_id.to_string()),
        generation: 1,
        updated_unix_ms: 42,
    };
    let first = runtime("feature/first", "first", "ses_first");
    let second = runtime("feature/second", "second", "ses_second");
    save_runtime(&repo, &first).unwrap();
    save_runtime(&repo, &second).unwrap();
    let replacement = OpencodeRuntime {
        server_port: 41_001,
        server_url: server_url(41_001),
        server_pid: Some(300),
        server_process_identity: Some(400),
        updated_unix_ms: 84,
        ..first.clone()
    };

    save_shared_server_runtime(&repo, &replacement).unwrap();

    let runtimes = load_runtimes_for_harness(&repo, "opencode").unwrap();
    assert_eq!(runtimes.len(), 2);
    for runtime in &runtimes {
        assert_eq!(runtime.server_port, replacement.server_port);
        assert_eq!(runtime.server_url, replacement.server_url);
        assert_eq!(runtime.server_pid, replacement.server_pid);
        assert_eq!(
            runtime.server_process_identity,
            replacement.server_process_identity
        );
    }
    let sessions = runtimes
        .iter()
        .map(|runtime| {
            (
                runtime.branch.as_str(),
                runtime.opencode_session_id.as_deref(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        sessions.get(first.branch.as_str()),
        Some(&Some("ses_first"))
    );
    assert_eq!(
        sessions.get(second.branch.as_str()),
        Some(&Some("ses_second"))
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn replacing_shared_server_rolls_back_every_reference_on_failure() {
    let temp = unique_temp_dir("prism-opencode-replacement-rollback-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let runtime = |branch: &str, worktree: &str| OpencodeRuntime {
        repo_root: temp.display().to_string(),
        harness_id: "opencode".to_string(),
        branch: branch.to_string(),
        worktree_path: temp.join(worktree).display().to_string(),
        server_port: 41_000,
        server_url: server_url(41_000),
        server_pid: Some(100),
        server_process_identity: Some(200),
        opencode_session_id: None,
        generation: 1,
        updated_unix_ms: 42,
    };
    let first = runtime("feature/first", "first");
    let second = runtime("feature/second", "second");
    save_runtime(&repo, &first).unwrap();
    save_runtime(&repo, &second).unwrap();
    observability::with_writable_db(&repo, |path| {
        crate::persistence::session::test_install_shared_server_runtime_upsert_failure(path)
            .map_err(|error| error.to_string())
    })
    .unwrap();
    let replacement = OpencodeRuntime {
        server_port: 41_001,
        server_url: server_url(41_001),
        server_pid: Some(300),
        server_process_identity: Some(400),
        updated_unix_ms: 84,
        ..first.clone()
    };

    let error = save_shared_server_runtime(&repo, &replacement).unwrap_err();

    assert!(error.contains("forced runtime upsert failure"));
    for runtime in load_runtimes_for_harness(&repo, "opencode").unwrap() {
        assert_eq!(runtime.server_port, first.server_port);
        assert_eq!(runtime.server_url, first.server_url);
        assert_eq!(runtime.server_pid, first.server_pid);
        assert_eq!(
            runtime.server_process_identity,
            first.server_process_identity
        );
    }
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn runtime_identity_is_isolated_by_harness_id() {
    let temp = unique_temp_dir("prism-opencode-runtime-harness-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let worktree = temp.join("feature");
    for (harness_id, port, session_id) in [
        ("opencode-a", 41_222, "ses_a"),
        ("opencode-b", 41_223, "ses_b"),
    ] {
        save_runtime(
            &repo,
            &OpencodeRuntime {
                repo_root: temp.display().to_string(),
                harness_id: harness_id.to_string(),
                branch: "feature".to_string(),
                worktree_path: worktree.display().to_string(),
                server_port: port,
                server_url: server_url(port),
                server_pid: None,
                server_process_identity: None,
                opencode_session_id: Some(session_id.to_string()),
                generation: 1,
                updated_unix_ms: 42,
            },
        )
        .unwrap();
    }

    assert_eq!(
        load_runtime(&repo, "opencode-a", "feature", &worktree)
            .unwrap()
            .unwrap()
            .opencode_session_id
            .as_deref(),
        Some("ses_a")
    );
    assert_eq!(
        load_runtime(&repo, "opencode-b", "feature", &worktree)
            .unwrap()
            .unwrap()
            .opencode_session_id
            .as_deref(),
        Some("ses_b")
    );
    let _ = fs::remove_dir_all(temp);
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn legacy_runtime_without_start_time_cannot_stop_a_matching_live_process() {
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg("while :; do sleep 1; done")
        .arg("legacy-opencode-fixture")
        .args(["serve", "--hostname", "127.0.0.1", "--port", "41222"])
        .spawn()
        .unwrap();
    let runtime = OpencodeRuntime {
        repo_root: "/repo".to_string(),
        harness_id: "opencode".to_string(),
        branch: "feature".to_string(),
        worktree_path: "/repo/feature".to_string(),
        server_port: 41_222,
        server_url: server_url(41_222),
        server_pid: Some(child.id()),
        server_process_identity: None,
        opencode_session_id: Some("ses_old".to_string()),
        generation: 2,
        updated_unix_ms: 42,
    };

    let result = shutdown_stored_server_with(&runtime, |_| {
        Ok(Some(vec![
            "legacy-opencode-fixture".to_string(),
            "serve".to_string(),
            "--hostname".to_string(),
            "127.0.0.1".to_string(),
            "--port".to_string(),
            "41222".to_string(),
        ]))
    })
    .await;
    let child_was_running = child.try_wait().unwrap().is_none();
    child.kill().unwrap();
    child.wait().unwrap();

    assert_eq!(
        result.unwrap_err(),
        format!(
            "refusing to stop opencode server {}: reusable process identity is unavailable",
            child.id()
        )
    );
    assert!(child_was_running);
}

#[test]
fn parse_sessions_accepts_top_level_array() {
    let sessions = parse_sessions(
        r#"[
                {"id":"ses_old","directory":"/repo/wt","title":"old","timeUpdated":"2026-01-01T00:00:00Z"},
                {"id":"ses_new","directory":"/repo/wt","title":"new","timeUpdated":"2026-01-02T00:00:00Z"}
            ]"#,
    );

    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].id, "ses_old");
    assert_eq!(sessions[1].directory.as_deref(), Some("/repo/wt"));
}

#[test]
fn parse_sessions_accepts_data_envelope() {
    let sessions = parse_sessions(
        r#"{"data":[{"id":"ses_1","path":"/repo/wt","updatedAt":"2026-01-01T00:00:00Z"}]}"#,
    );

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "ses_1");
    assert_eq!(sessions[0].directory.as_deref(), Some("/repo/wt"));
}

#[test]
fn parse_sessions_reads_nested_update_time_and_ignores_newer_child_session() {
    let sessions = parse_sessions(
        r#"[
                {"id":"current","directory":"/repo/wt","time":{"updated":200}},
                {"id":"child","directory":"/repo/wt","parentID":"current","time":{"updated":300}},
                {"id":"old","directory":"/repo/wt","time":{"updated":100}}
            ]"#,
    );

    let selected = newest_session_for_worktree(&sessions, "/repo/wt").unwrap();

    assert_eq!(selected.id, "current");
    assert_eq!(selected.time_updated.as_deref(), Some("200"));
}

#[test]
fn parse_session_accepts_session_envelope_and_session_id_field() {
    let session =
        parse_session(r#"{"session":{"sessionID":"ses_1","cwd":"/repo/wt","title":"feature"}}"#)
            .unwrap();

    assert_eq!(session.id, "ses_1");
    assert_eq!(session.directory.as_deref(), Some("/repo/wt"));
    assert_eq!(session.title.as_deref(), Some("feature"));
}

#[test]
fn newest_session_for_worktree_prefers_latest_matching_update_time() {
    let sessions = vec![
        OpencodeSession {
            id: "wrong".to_string(),
            directory: Some("/repo/other".to_string()),
            title: None,
            time_updated: Some("2026-01-03T00:00:00Z".to_string()),
            parent_id: None,
        },
        OpencodeSession {
            id: "old".to_string(),
            directory: Some("/repo/wt".to_string()),
            title: None,
            time_updated: Some("2026-01-01T00:00:00Z".to_string()),
            parent_id: None,
        },
        OpencodeSession {
            id: "new".to_string(),
            directory: Some("/repo/wt".to_string()),
            title: None,
            time_updated: Some("2026-01-02T00:00:00Z".to_string()),
            parent_id: None,
        },
    ];

    let selected = newest_session_for_worktree(&sessions, "/repo/wt").unwrap();

    assert_eq!(selected.id, "new");
}

#[cfg(windows)]
#[test]
fn session_matching_normalizes_windows_separators_and_case() {
    let session = OpencodeSession {
        id: "windows".to_string(),
        directory: Some(r"C:\RÉPO\worktree feature 雪".to_string()),
        title: None,
        time_updated: Some("1".to_string()),
        parent_id: None,
    };

    assert!(session_matches_worktree(
        &session,
        "c:/répo/worktree feature 雪/"
    ));
    assert_eq!(
        newest_session_for_worktree(
            std::slice::from_ref(&session),
            "c:/répo/worktree feature 雪"
        )
        .map(|session| session.id.as_str()),
        Some("windows")
    );
}

#[test]
fn newest_session_for_worktree_ignores_sessions_without_matching_directory() {
    let sessions = vec![
        OpencodeSession {
            id: "old".to_string(),
            directory: Some("/repo/wt".to_string()),
            title: None,
            time_updated: Some("2026-01-01T00:00:00Z".to_string()),
            parent_id: None,
        },
        OpencodeSession {
            id: "new_without_directory".to_string(),
            directory: None,
            title: None,
            time_updated: Some("2026-01-03T00:00:00Z".to_string()),
            parent_id: None,
        },
        OpencodeSession {
            id: "new_other_worktree".to_string(),
            directory: Some("/repo/other".to_string()),
            title: None,
            time_updated: Some("2026-01-04T00:00:00Z".to_string()),
            parent_id: None,
        },
    ];

    let selected = newest_session_for_worktree(&sessions, "/repo/wt").unwrap();

    assert_eq!(selected.id, "old");
}

#[test]
fn resolve_session_prefers_newer_worktree_session_over_stored_session() {
    let worktree = PathBuf::from("/repo/wt");
    let server_url = start_session_resolution_server();
    let runtime = OpencodeRuntime {
        repo_root: "/repo".to_string(),
        harness_id: "opencode".to_string(),
        branch: "feature".to_string(),
        worktree_path: worktree.display().to_string(),
        server_port: 41_234,
        server_url,
        server_pid: None,
        server_process_identity: None,
        opencode_session_id: Some("old".to_string()),
        generation: 0,
        updated_unix_ms: 0,
    };

    let selected = resolve_session(&runtime, &worktree).unwrap();

    assert_eq!(selected.id, "new");
}

#[test]
fn refresh_session_keeps_runtime_when_session_listing_fails() {
    let temp = unique_temp_dir("prism-opencode-refresh-offline-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let worktree = temp.join("feature");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let runtime = OpencodeRuntime {
        repo_root: temp.display().to_string(),
        harness_id: "opencode".to_string(),
        branch: "feature".to_string(),
        worktree_path: worktree.display().to_string(),
        server_port: port,
        server_url: server_url(port),
        server_pid: None,
        server_process_identity: None,
        opencode_session_id: Some("stored".to_string()),
        generation: 3,
        updated_unix_ms: 42,
    };

    let refreshed = refresh_opencode_session(&repo, runtime.clone(), &worktree).unwrap();

    assert_eq!(refreshed, runtime);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn url_path_segment_percent_encodes_non_segment_bytes() {
    assert_eq!(url_path_segment("session/id 1"), "session%2Fid%201");
    assert_eq!(url_path_segment("ses_1-2.3~4"), "ses_1-2.3~4");
}

#[test]
fn request_path_routes_requests_to_the_worktree_directory() {
    let directory = Path::new("/repo/work tree");

    assert_eq!(
        request_path("/session/status", Some(directory)),
        "/session/status?directory=%2Frepo%2Fwork%20tree"
    );
    assert_eq!(
        request_path("/session/ses_1/message?limit=10", Some(directory)),
        "/session/ses_1/message?limit=10&directory=%2Frepo%2Fwork%20tree"
    );
    assert_eq!(request_path("/global/health", None), "/global/health");
}

#[test]
fn create_session_routes_request_to_worktree_directory() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server_url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        let mut content_length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                request.push_str(&line);
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap();
            }
            request.push_str(&line);
        }
        let mut request_body = vec![0; content_length];
        reader.read_exact(&mut request_body).unwrap();
        request.push_str(&String::from_utf8_lossy(&request_body));
        drop(reader);
        let body = r#"{"id":"ses_1","directory":"/repo/work tree","title":"feature"}"#;
        let response = format!(
            "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).unwrap();
        request
    });

    let created = create_session(&server_url, Path::new("/repo/work tree"), "feature").unwrap();
    let request = server.join().unwrap();

    assert_eq!(created.id, "ses_1");
    assert!(
        request.starts_with("POST /session?directory=%2Frepo%2Fwork%20tree HTTP/1.1"),
        "{request}"
    );
    assert!(request.contains(r#"{"title":"feature"}"#));
    assert!(!request.contains(r#""directory""#));
}

#[test]
fn async_prompt_body_escapes_text_and_includes_agent_selection() {
    assert_eq!(
        prompt_async_body(
            "  hello world\n\"quotes\" and $PATH && true\n--leading-dash",
            crate::harness::AgentSelection::default(),
        )
        .unwrap(),
        r#"{"parts":[{"type":"text","text":"  hello world\n\"quotes\" and $PATH && true\n--leading-dash"}]}"#
    );
    let body = prompt_async_body(
        "build it",
        crate::harness::AgentSelection {
            model: Some("provider/model/with/slashes"),
            variant: Some("high"),
        },
    )
    .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({
            "parts": [{"type": "text", "text": "build it"}],
            "model": {
                "providerID": "provider",
                "modelID": "model/with/slashes",
            },
            "variant": "high",
        })
    );
    assert!(
        prompt_async_body(
            "build it",
            crate::harness::AgentSelection {
                model: Some("malformed"),
                variant: None,
            },
        )
        .is_err()
    );
}

#[test]
fn parses_status_messages_tools_and_todos() {
    assert_eq!(
        parse_session_state(
            r#"{"data":[{"sessionID":"ses_other","status":"idle"},{"sessionID":"ses_1","status":"busy"}]}"#,
            "ses_1"
        ),
        Some(OpencodeState::Busy)
    );

    let summary = parse_message_summary(
        r#"{"data":[
                {"role":"assistant","text":"first\nreply"},
                {"type":"tool","name":"bash","status":"running"}
            ]}"#,
    );
    assert_eq!(summary.latest_message.as_deref(), Some("first reply"));
    assert_eq!(summary.active_tool.as_deref(), Some("bash running"));

    assert!(has_pending_permission(
        r#"[{"id":"per_1","sessionID":"ses_1","permission":"read"}]"#,
        "ses_1"
    ));
    assert!(!has_pending_permission(
        r#"[{"id":"per_1","sessionID":"ses_other","permission":"read"}]"#,
        "ses_1"
    ));

    let summary = parse_message_summary(
        r#"[
                {"info":{"role":"user"},"parts":[{"type":"text","text":"question"}]},
                {"info":{"role":"assistant"},"parts":[
                    {"type":"text","text":"latest\nreply"},
                    {"type":"tool","tool":"bash","state":{"status":"completed"}}
                ]}
            ]"#,
    );
    assert_eq!(summary.latest_message.as_deref(), Some("latest reply"));
    assert_eq!(summary.latest_user_message.as_deref(), Some("question"));
    assert_eq!(summary.recent_messages, vec!["latest reply"]);
    assert_eq!(summary.active_tool, None);

    let completed = parse_message_summary(
        r#"[{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"finish":"stop"},"parts":[{"type":"text","text":"done"}]}]"#,
    );
    assert_eq!(completed.latest_turn_state, Some(OpencodeState::Done));
    assert_eq!(completed.latest_error, None);

    let aborted = parse_message_summary(
        r#"[{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"error":{"name":"MessageAbortedError"}},"parts":[]}]"#,
    );
    assert_eq!(aborted.latest_turn_state, Some(OpencodeState::Done));
    assert_eq!(aborted.latest_error.as_deref(), Some("MessageAbortedError"));

    let continuing = parse_message_summary(
        r#"[{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"finish":"tool-calls"},"parts":[]}]"#,
    );
    assert_eq!(continuing.latest_turn_state, Some(OpencodeState::Busy));

    let in_progress = parse_message_summary(
        r#"[{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1}},"parts":[]}]"#,
    );
    assert_eq!(in_progress.latest_turn_state, Some(OpencodeState::Busy));

    let todos = parse_todos(
        r#"{"todos":[
                {"content":"write code","status":"in_progress"},
                {"title":"run tests","state":"pending"}
            ]}"#,
    );
    assert_eq!(todos.len(), 2);
    assert_eq!(todos[0].text, "write code");
    assert_eq!(todos[1].status, "pending");
}

#[test]
fn missing_session_status_means_the_session_is_idle() {
    assert_eq!(
        session_state_from_status_body(r#"{}"#, "ses_1"),
        OpencodeState::Idle
    );
    assert_eq!(
        session_state_from_status_body(r#"{"ses_other":{"status":"busy"}}"#, "ses_1"),
        OpencodeState::Idle
    );
}

#[test]
fn parses_opencode_status_sse_event() {
    let event = parse_event_payload(
            r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":"busy","title":"Feature"}}"#,
        )
        .unwrap();

    assert_eq!(event.session_id.as_deref(), Some("ses_1"));
    assert_eq!(event.state, Some(OpencodeState::Busy));
    assert_eq!(event.title.as_deref(), Some("Feature"));

    let event = parse_event_payload(
            r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":{"type":"retry","attempt":2}}}"#,
        )
        .unwrap();
    assert_eq!(event.state, Some(OpencodeState::Retry));

    let event = parse_event_payload(
            r#"{"type":"permission.updated","properties":{"id":"per_1","sessionID":"ses_1","title":"Run command"}}"#,
        )
        .unwrap();
    assert_eq!(event.state, Some(OpencodeState::NeedsInput));

    let event = parse_event_payload(
            r#"{"type":"permission.asked","properties":{"id":"per_2","sessionID":"ses_1","permission":"read"}}"#,
        )
        .unwrap();
    assert_eq!(event.state, Some(OpencodeState::NeedsInput));
}

#[test]
fn parses_opencode_message_tool_and_todo_events() {
    let message = parse_event_payload(
            r#"{"type":"message.part.updated","properties":{"sessionID":"ses_1","role":"assistant","text":"hello\nthere"}}"#,
        )
        .unwrap();
    assert_eq!(message.latest_message.as_deref(), Some("hello there"));

    let completed = parse_event_payload(
            r#"{"type":"message.updated","properties":{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"finish":"stop"}}}"#,
        )
        .unwrap();
    assert_eq!(completed.session_id.as_deref(), Some("ses_1"));
    assert_eq!(completed.state, Some(OpencodeState::Done));

    let aborted = parse_event_payload(
            r#"{"type":"message.updated","properties":{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"error":{"name":"MessageAbortedError"}}}}"#,
        )
        .unwrap();
    assert_eq!(aborted.state, Some(OpencodeState::Done));
    assert_eq!(aborted.detail.as_deref(), Some("MessageAbortedError"));

    let tool_calls = parse_event_payload(
            r#"{"type":"message.updated","properties":{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"finish":"tool-calls"}}}"#,
        )
        .unwrap();
    assert_eq!(tool_calls.state, Some(OpencodeState::Busy));

    let tool = parse_event_payload(
            r#"{"type":"tool.updated","properties":{"sessionID":"ses_1","name":"bash","status":"running"}}"#,
        )
        .unwrap();
    assert_eq!(tool.active_tool.as_deref(), Some("bash running"));

    let todo = parse_event_payload(
            r#"{"type":"todo.updated","properties":{"sessionID":"ses_1","todos":[{"content":"ship it","status":"in_progress"}]}}"#,
        )
        .unwrap();
    assert_eq!(todo.todos.unwrap()[0].text, "ship it");
}

#[test]
fn classifies_only_supersedable_status_and_text_snapshots() {
    let (_, status_facet) = parse_event_payload_classified(
        r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":"busy"}}"#,
    )
    .unwrap();
    let (_, message_facet) = parse_event_payload_classified(
            r#"{"type":"message.part.updated","properties":{"sessionID":"ses_1","role":"assistant","text":"latest"}}"#,
        )
        .unwrap();
    let (_, permission_facet) = parse_event_payload_classified(
        r#"{"type":"permission.asked","properties":{"sessionID":"ses_1","permission":"bash"}}"#,
    )
    .unwrap();
    let (_, tool_facet) = parse_event_payload_classified(
            r#"{"type":"message.part.updated","properties":{"sessionID":"ses_1","type":"tool","name":"bash","status":"running"}}"#,
        )
        .unwrap();

    assert_eq!(status_facet, Some(OpencodeSnapshotFacet::Status));
    assert_eq!(message_facet, Some(OpencodeSnapshotFacet::Message));
    assert_eq!(permission_facet, None);
    assert_eq!(tool_facet, None);
}

#[test]
fn ignores_malformed_opencode_events() {
    assert_eq!(parse_event_payload("not json"), None);
    assert_eq!(parse_event_payload(r#"{"type":"session.status"}"#), None);
}

#[test]
fn opencode_event_schema_drift_does_not_read_unrelated_nested_status() {
    let event = parse_event_payload(
            r#"{"type":"session.status","properties":{"sessionID":"ses_1","metadata":{"status":"busy"}}}"#,
        )
        .unwrap();

    assert_eq!(event.session_id.as_deref(), Some("ses_1"));
    assert_eq!(event.state, None);
}

#[test]
fn opencode_status_schema_drift_does_not_read_unrelated_nested_status() {
    assert_eq!(
        parse_session_state(
            r#"{"sessionID":"ses_1","metadata":{"status":"busy"}}"#,
            "ses_1",
        ),
        None
    );
}

#[test]
fn opencode_state_maps_to_existing_agent_state() {
    assert_eq!(OpencodeState::Busy.agent_state(), AgentState::Running);
    assert_eq!(OpencodeState::Idle.agent_state(), AgentState::Idle);
    assert_eq!(OpencodeState::Done.agent_state(), AgentState::ExitedOk);
    assert_eq!(
        OpencodeState::NeedsInput.agent_state(),
        AgentState::NeedsInput
    );
    assert_eq!(
        OpencodeState::Offline.agent_state(),
        AgentState::NeedsRestart
    );
}

#[test]
fn parse_localhost_url_rejects_remote_hosts() {
    assert!(parse_localhost_url("http://example.com:41000").is_err());
    assert_eq!(
        parse_localhost_url("http://127.0.0.1:41000").unwrap(),
        ("127.0.0.1".to_string(), 41_000)
    );
}

#[test]
fn http_response_completion_uses_content_length_without_waiting_for_eof() {
    let complete =
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]";
    let partial = &complete[..complete.len() - 1];

    assert!(!http_response_is_complete(partial));
    assert!(http_response_is_complete(complete));
    assert!(http_response_is_complete(
        b"HTTP/1.1 204 No Content\r\nConnection: keep-alive\r\n\r\n"
    ));
    assert!(!http_response_is_complete(b"HTTP/1.1 100 Continue\r\n\r\n"));
    let chunked = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\n\r\n";
    assert!(http_response_is_complete(chunked.as_bytes()));
    assert_eq!(parse_response(chunked).unwrap().body, "[]");
    let chunked_with_trailer =
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\nChecksum: x\r\n\r\n";
    assert!(http_response_is_complete(chunked_with_trailer.as_bytes()));
    assert_eq!(parse_response(chunked_with_trailer).unwrap().body, "[]");
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
#[ignore = "requires PRISM_TEST_OPENCODE pointing to a real OpenCode binary"]
async fn real_opencode_server_round_trips_prism_session_api() {
    let opencode = std::env::var("PRISM_TEST_OPENCODE")
        .expect("set PRISM_TEST_OPENCODE to the real OpenCode binary");
    let temp = unique_temp_dir("prism-real-opencode-test");
    let worktree = temp.join("worktree");
    let second_worktree = temp.join("second-worktree");
    let home = temp.join("home");
    let config_dir = temp.join("opencode-config");
    let data_dir = temp.join("data");
    for path in [&worktree, &second_worktree, &home, &config_dir, &data_dir] {
        fs::create_dir_all(path).unwrap();
    }
    let worktree = fs::canonicalize(worktree).unwrap();
    let second_worktree = fs::canonicalize(second_worktree).unwrap();
    let repo = Repository::with_config_dir_for_test(worktree.clone(), temp.join("config"));
    let wrapper = temp.join("opencode-isolated");
    let real_home = std::env::var("HOME").unwrap_or_default();
    let mise_data_dir = std::env::var("MISE_DATA_DIR").unwrap_or_else(|_| {
        PathBuf::from(&real_home)
            .join(".local/share/mise")
            .display()
            .to_string()
    });
    fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexport HOME={}\nexport MISE_DATA_DIR={}\nexport npm_config_cache={}\nexport OPENCODE_CONFIG_DIR={}\nexport OPENCODE_DISABLE_AUTOUPDATE=true\nexport OPENCODE_DISABLE_DEFAULT_PLUGINS=true\nexport OPENCODE_DISABLE_LSP_DOWNLOAD=true\nexport OPENCODE_DISABLE_MODELS_FETCH=true\nexport XDG_DATA_HOME={}\nexec {} \"$@\"\n",
                shell_quote_for_test(&home.display().to_string()),
                shell_quote_for_test(&mise_data_dir),
                shell_quote_for_test(&format!("{real_home}/.npm")),
                shell_quote_for_test(&config_dir.display().to_string()),
                shell_quote_for_test(&data_dir.display().to_string()),
                shell_quote_for_test(&opencode),
            ),
        )
        .unwrap();
    let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&wrapper, permissions).unwrap();
    let mut config = Config::load(&repo);
    config.opencode_port_base = 41_000;
    config.opencode_port_span = 1_000;
    config
        .tools
        .insert("opencode".to_string(), wrapper.display().to_string());

    let runtime = ensure_opencode_server(&repo, &config, "feature/smoke", &worktree)
        .await
        .unwrap();
    let result: Result<(), String> = async {
        if !super::check_health(&runtime.server_url) {
            return Err("OpenCode server did not remain healthy".to_string());
        }
        let second_runtime =
            ensure_opencode_server(&repo, &config, "feature/second", &second_worktree).await?;
        if second_runtime.server_url != runtime.server_url
            || second_runtime.server_pid != runtime.server_pid
        {
            return Err("worktrees did not reuse one OpenCode server".to_string());
        }
        let second_session =
            create_session(&runtime.server_url, &second_worktree, "Second worktree")?;
        if get_session_for_worktree(&runtime.server_url, &second_session.id, &second_worktree)?
            .is_none()
        {
            return Err("shared server did not route the second worktree".to_string());
        }
        let created = create_session(&runtime.server_url, &worktree, "Prism smoke test")?;
        let listed = list_sessions(&runtime.server_url)?;
        if !listed.iter().any(|session| session.id == created.id) {
            return Err(format!(
                "created OpenCode session {} was not listed",
                created.id
            ));
        }
        let resolved = ensure_opencode_session(&repo, &config, "feature/smoke", &worktree).await?;
        if resolved.opencode_session_id.as_deref() != Some(created.id.as_str()) {
            return Err(format!(
                "Prism did not select created OpenCode session {} for {}",
                created.id,
                worktree.display()
            ));
        }
        let fetched = get_session(&runtime.server_url, &created.id)?
            .ok_or_else(|| format!("created OpenCode session {} was not found", created.id))?;
        if fetched.id != created.id {
            return Err(format!(
                "fetched OpenCode session {} instead of {}",
                fetched.id, created.id
            ));
        }
        let prompt = "Prism persisted prompt smoke test";
        submit_prompt(&runtime.server_url, &created.id, prompt)?;
        let mut persisted = false;
        for _ in 0..20 {
            let summary = fetch_message_summary(&runtime.server_url, &created.id, None)?;
            if summary.latest_user_message.as_deref() == Some(prompt) {
                persisted = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !persisted {
            return Err("submitted OpenCode prompt was not persisted".to_string());
        }
        Ok(())
    }
    .await;
    let shutdown = shutdown_owned_server(&runtime).await;
    let _ = fs::remove_dir_all(temp);

    result.unwrap();
    shutdown.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn worktree_cleanup_keeps_a_server_referenced_by_another_worktree() {
    let temp = unique_temp_dir("prism-shared-opencode-cleanup");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
    let control = crate::process::spawn_owned(
        crate::process::Command::new("sh").args(["-c", "while :; do sleep 1; done"]),
        crate::process::ProcessDescriptor::new("test.opencode.shared-server"),
    )
    .await
    .unwrap();
    let process_id = control.pid();
    let process_identity = control.identity();
    record_owned_server_process(control).await;
    let runtime = |branch: &str, worktree: &str| OpencodeRuntime {
        repo_root: repo.root.display().to_string(),
        harness_id: "opencode".to_string(),
        branch: branch.to_string(),
        worktree_path: worktree.to_string(),
        server_port: 41_000,
        server_url: "http://127.0.0.1:41000".to_string(),
        server_pid: Some(process_id),
        server_process_identity: process_identity,
        opencode_session_id: None,
        generation: 0,
        updated_unix_ms: 0,
    };
    let first = runtime("feature/first", "/repo/first");
    let second = runtime("feature/second", "/repo/second");
    save_runtime(&repo, &first).unwrap();
    save_runtime(&repo, &second).unwrap();

    shutdown_worktree_session_runtime_processes_with_lock_held(&repo, std::slice::from_ref(&first))
        .await
        .unwrap();
    assert!(owned_server_process(process_id).await);

    crate::persistence::session::delete_runtime(&observability::db_path(&repo), &first).unwrap();
    shutdown_worktree_session_runtime_processes_with_lock_held(
        &repo,
        std::slice::from_ref(&second),
    )
    .await
    .unwrap();
    assert!(!owned_server_process(process_id).await);
    fs::remove_dir_all(temp).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn owned_server_identity_mismatch_preserves_the_registered_control() {
    let control = crate::process::spawn_owned(
        crate::process::Command::new("sh").args(["-c", "sleep 30"]),
        crate::process::ProcessDescriptor::new("test.opencode.identity-mismatch"),
    )
    .await
    .unwrap();
    let process_id = control.pid();
    let identity = control
        .identity()
        .expect("owned process has reusable identity");
    let recorded = crate::process::RecordedProcess::from_stored(process_id, Some(identity));
    record_owned_server_process(control).await;
    let runtime = |stored_identity| OpencodeRuntime {
        repo_root: "/repo".to_string(),
        harness_id: "opencode".to_string(),
        branch: "feature/test".to_string(),
        worktree_path: "/repo/worktree".to_string(),
        server_port: 41_000,
        server_url: "http://127.0.0.1:41000".to_string(),
        server_pid: Some(process_id),
        server_process_identity: Some(stored_identity),
        opencode_session_id: None,
        generation: 0,
        updated_unix_ms: 0,
    };

    let error = shutdown_owned_server(&runtime(identity ^ 1))
        .await
        .unwrap_err();
    assert!(error.contains("registry identity disagrees"), "{error}");
    assert!(owned_server_process(process_id).await);
    assert_eq!(
        crate::process::observe_process(recorded).unwrap(),
        crate::process::ProcessObservation::RunningSameProcess
    );

    shutdown_owned_server(&runtime(identity)).await.unwrap();
    assert!(!owned_server_process(process_id).await);
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn owned_server_shutdown_kills_term_ignoring_descendant_and_reaps_leader() {
    let temp = unique_temp_dir("prism-owned-opencode-process");
    fs::create_dir_all(&temp).unwrap();
    let descendant_path = temp.join("descendant.pid");
    let script = r#"
            trap '' TERM
            (
                trap '' TERM
                while :; do sleep 1; done
            ) &
            descendant=$!
            printf '%s\n' "$descendant" > "$1"
            wait "$descendant"
        "#;
    let control = crate::process::spawn_owned(
        crate::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .arg("owned-opencode-fixture")
            .arg(&descendant_path),
        crate::process::ProcessDescriptor::new("test.opencode.owned-server"),
    )
    .await
    .unwrap();
    let process_id = control.pid();
    let recorded_process = crate::process::record_process(process_id).unwrap();
    record_owned_server_process(control).await;
    let runtime = OpencodeRuntime {
        repo_root: "/repo".to_string(),
        harness_id: "opencode".to_string(),
        branch: "feature/test".to_string(),
        worktree_path: "/repo/worktree".to_string(),
        server_port: 41_000,
        server_url: "http://127.0.0.1:41000".to_string(),
        server_pid: Some(process_id),
        server_process_identity: recorded_process
            .identity
            .map(crate::process::ProcessIdentity::stored_value),
        opencode_session_id: None,
        generation: 0,
        updated_unix_ms: 0,
    };
    let ready_deadline = std::time::Instant::now() + Duration::from_secs(1);
    while !descendant_path.exists() {
        assert!(std::time::Instant::now() < ready_deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let descendant_id = fs::read_to_string(&descendant_path)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    let recorded_descendant = crate::process::record_process(descendant_id).unwrap();

    let started = std::time::Instant::now();
    shutdown_owned_server(&runtime).await.unwrap();

    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(!owned_server_process(process_id).await);
    for process in [recorded_process, recorded_descendant] {
        let gone_deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if crate::process::observe_process(process).unwrap()
                == crate::process::ProcessObservation::Missing
            {
                break;
            }
            assert!(
                std::time::Instant::now() < gone_deadline,
                "owned server process {} survived shutdown",
                process.pid
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fs::remove_dir_all(temp).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn stored_server_shutdown_uses_verified_bounded_process_group_recovery() {
    let temp = unique_temp_dir("prism-stored-opencode-process");
    fs::create_dir_all(&temp).unwrap();
    let descendant_path = temp.join("descendant.pid");
    let script = r#"
            trap '' TERM
            (
                trap '' TERM
                while :; do sleep 1; done
            ) &
            descendant=$!
            printf '%s\n' "$descendant" > "$1"
            wait "$descendant"
        "#;
    let child = crate::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .arg("stored-opencode-fixture")
        .arg(&descendant_path)
        .args(["serve", "--hostname", "127.0.0.1", "--port", "41000"])
        .spawn_detached()
        .unwrap();
    let process_id = child.pid();
    let recorded_process = crate::process::record_process(process_id).unwrap();
    let runtime = OpencodeRuntime {
        repo_root: "/repo".to_string(),
        harness_id: "opencode".to_string(),
        branch: "feature/test".to_string(),
        worktree_path: "/repo/worktree".to_string(),
        server_port: 41_000,
        server_url: "http://127.0.0.1:41000".to_string(),
        server_pid: Some(process_id),
        server_process_identity: recorded_process
            .identity
            .map(crate::process::ProcessIdentity::stored_value),
        opencode_session_id: None,
        generation: 0,
        updated_unix_ms: 0,
    };
    let ready_deadline = std::time::Instant::now() + Duration::from_secs(1);
    while !descendant_path.exists() {
        assert!(std::time::Instant::now() < ready_deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let descendant_id = fs::read_to_string(&descendant_path)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    let recorded_descendant = crate::process::record_process(descendant_id).unwrap();

    let started = std::time::Instant::now();
    shutdown_stored_server(&runtime).await.unwrap();

    assert!(started.elapsed() < Duration::from_secs(3));
    for process in [recorded_process, recorded_descendant] {
        let gone_deadline = std::time::Instant::now() + Duration::from_secs(2);
        while crate::process::observe_process(process).unwrap()
            != crate::process::ProcessObservation::Missing
        {
            assert!(
                std::time::Instant::now() < gone_deadline,
                "stored server process {} survived shutdown",
                process.pid
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fs::remove_dir_all(temp).unwrap();
}

#[cfg(unix)]
fn shell_quote_for_test(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
}

fn start_health_server(
    worktree: PathBuf,
) -> (String, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    let server = std::thread::spawn(move || {
        while !server_stop.load(Ordering::Acquire) {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("accept health request: {error}"),
            };
            stream.set_nonblocking(false).unwrap();
            let mut request = [0_u8; 1024];
            let count = match stream.read(&mut request) {
                Ok(0) => continue,
                Ok(count) => count,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    continue;
                }
                Err(error) => panic!("read health request: {error}"),
            };
            let request = String::from_utf8_lossy(&request[..count]);
            let body = if request.starts_with("GET /global/health ") {
                r#"{"healthy":true}"#.to_string()
            } else {
                format!(
                    r#"{{"id":"ses_first","directory":"{}"}}"#,
                    worktree.display()
                )
            };
            if let Err(error) = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ) && !matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
            ) {
                panic!("write health response: {error}");
            }
        }
    });
    (url, stop, server)
}

fn start_session_resolution_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().take(2) {
            let mut stream = stream.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0_u8; 256];
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request);
            let body = if request.starts_with("GET /session/old ") {
                r#"{"id":"old","directory":"/repo/wt","timeUpdated":"2026-01-01T00:00:00Z"}"#
            } else if request.starts_with("GET /session ") || request.starts_with("GET /session?") {
                r#"[
                        {"id":"old","directory":"/repo/wt","timeUpdated":"2026-01-01T00:00:00Z"},
                        {"id":"new","directory":"/repo/wt","timeUpdated":"2026-01-02T00:00:00Z"}
                    ]"#
            } else {
                r#"{}"#
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    url
}
