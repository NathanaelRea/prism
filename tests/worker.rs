mod common;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use common::CompactTempDir as TempDir;

fn prism(runtime: &Path, home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prism"));
    command
        .env("PRISM_RUNTIME_DIR", runtime)
        .env("XDG_CONFIG_HOME", home)
        .env("HOME", home);
    command
}

fn run(runtime: &Path, home: &Path, args: &[&str]) -> Output {
    prism(runtime, home)
        .args(args)
        .output()
        .expect("run Prism worker command")
}

fn serial_worker_test() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn worker_request(runtime: &Path, request: serde_json::Value) -> serde_json::Value {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(runtime.join("worker.sock")).unwrap();
    writeln!(stream, "{request}").unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    serde_json::from_str(response.trim()).unwrap()
}

#[test]
fn worker_socket_owns_generalized_workflow_mutations_and_inspection() {
    let _serial = serial_worker_test();
    let temp = TempDir::new("worker-workflow-operations");
    let runtime = temp.runtime_path().to_path_buf();
    let home = temp.path.join("home");
    fs::create_dir_all(&home).unwrap();

    assert!(run(&runtime, &home, &["worker", "ensure"]).status.success());
    let registered = worker_request(
        &runtime,
        serde_json::json!({
            "type": "workflow_register_definition",
            "definition": {
                "id": "definition",
                "name": "socket-tracer",
                "revision": "1",
                "source": "test",
                "trusted": true,
                "body_json": "{}",
                "digest": "digest",
                "now_unix_ms": 1
            }
        }),
    );
    assert_eq!(registered, serde_json::json!({"ok": true}));

    let launched = worker_request(
        &runtime,
        serde_json::json!({
            "type": "workflow_launch",
            "run": {
                "run_id": "run",
                "definition_snapshot_id": "definition",
                "repository": null,
                "idempotency_key": "socket-run",
                "now_unix_ms": 2
            },
            "steps": [{
                "id": "step",
                "key": "approval",
                "implementation": "not-installed",
                "target_id": "local",
                "input_json": "{}",
                "dependencies": [],
                "resources": []
            }]
        }),
    );
    assert_eq!(launched, serde_json::json!({"ok": true, "run_id": "run"}));

    let inspected = worker_request(
        &runtime,
        serde_json::json!({"type": "workflow_inspect", "run_id": "run"}),
    );
    assert_eq!(inspected["ok"], true);
    assert_eq!(inspected["run"]["id"], "run");
    assert_eq!(inspected["run"]["definition_name"], "socket-tracer");
    assert!(matches!(
        inspected["run"]["status"].as_str(),
        Some("runnable" | "failed")
    ));
    assert_eq!(inspected["run"]["steps"][0]["input_json"], "{}");
    assert_eq!(inspected["run"]["artifacts"], serde_json::json!([]));
    let listed = worker_request(
        &runtime,
        serde_json::json!({"type": "workflow_list", "repository": null, "limit": 8}),
    );
    assert_eq!(listed["ok"], true);
    assert_eq!(listed["runs"].as_array().unwrap().len(), 1);
    assert_eq!(listed["runs"][0]["id"], "run");

    // The production Worker must not leave an unsupported pinned implementation runnable.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let failed = worker_request(
            &runtime,
            serde_json::json!({"type": "workflow_inspect", "run_id": "run"}),
        );
        if failed["run"]["status"] == "failed" {
            assert_eq!(failed["run"]["steps"][0]["status"], "failed");
            assert!(failed["run"]["events"].as_array().is_some_and(|events| {
                events
                    .iter()
                    .any(|event| event["kind"] == "unsupported_runnable_step")
            }));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "unsupported Step did not fail durably"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    assert!(
        run(&runtime, &home, &["worker", "shutdown"])
            .status
            .success()
    );
}

#[test]
fn production_worker_executes_a_bundled_command_step() {
    let _serial = serial_worker_test();
    let temp = TempDir::new("worker-command-step");
    let runtime = temp.runtime_path().to_path_buf();
    let home = temp.path.join("home");
    fs::create_dir_all(&home).unwrap();

    assert!(run(&runtime, &home, &["worker", "ensure"]).status.success());
    assert_eq!(
        worker_request(
            &runtime,
            serde_json::json!({
                "type": "workflow_register_definition",
                "definition": {
                    "id": "command-definition",
                    "name": "command",
                    "revision": "1",
                    "source": "test",
                    "trusted": true,
                    "body_json": "{}",
                    "digest": "command-digest",
                    "now_unix_ms": 1
                }
            }),
        ),
        serde_json::json!({"ok": true})
    );
    assert_eq!(
        worker_request(
            &runtime,
            serde_json::json!({
                "type": "workflow_launch",
                "run": {
                    "run_id": "command-run",
                    "definition_snapshot_id": "command-definition",
                    "repository": null,
                    "idempotency_key": "command-run",
                    "now_unix_ms": 2
                },
                "steps": [{
                    "id": "command-step",
                    "key": "command",
                    "implementation": "command",
                    "target_id": "local",
                    "input_json": serde_json::json!({
                        "program": "/bin/sh",
                        "args": ["-c", "printf 'command output\\n'"]
                    }).to_string(),
                    "dependencies": [],
                    "resources": []
                }]
            }),
        ),
        serde_json::json!({"ok": true, "run_id": "command-run"})
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = worker_request(
            &runtime,
            serde_json::json!({"type": "workflow_inspect", "run_id": "command-run"}),
        );
        if response["run"]["status"] == "succeeded" {
            assert!(
                response["run"]["attempts"][0]["process_id"]
                    .as_i64()
                    .unwrap()
                    > 0
            );
            assert!(
                response["run"]["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|event| event["kind"] == "process_recorded")
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "command workflow did not finish: {response}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(
        run(&runtime, &home, &["worker", "shutdown"])
            .status
            .success()
    );
}

#[test]
fn production_worker_executes_a_standard_process_in_the_exact_worktree() {
    use std::os::unix::fs::MetadataExt;

    let _serial = serial_worker_test();
    let temp = TempDir::new("worker-standard-process");
    let runtime = temp.runtime_path().to_path_buf();
    let home = temp.path.join("home");
    let repository = temp.path.join("repository");
    fs::create_dir_all(repository.join(".git")).unwrap();
    fs::create_dir_all(&home).unwrap();
    let metadata = fs::metadata(repository.join(".git")).unwrap();
    let incarnation = format!("directory:{}:{}", metadata.dev(), metadata.ino());
    let scope_id = format!("{}:{}", repository.display(), repository.display());

    assert!(run(&runtime, &home, &["worker", "ensure"]).status.success());
    assert_eq!(
        worker_request(
            &runtime,
            serde_json::json!({
                "type":"workflow_register_definition",
                "definition":{
                    "id":"standard-process-definition","name":"standard-process",
                    "revision":"1","source":"test","trusted":true,
                    "body_json":"{}","digest":"standard-process-digest","now_unix_ms":1
                }
            })
        ),
        serde_json::json!({"ok":true})
    );
    assert_eq!(
        worker_request(
            &runtime,
            serde_json::json!({
                "type":"workflow_launch",
                "run":{
                    "run_id":"standard-process-run",
                    "definition_snapshot_id":"standard-process-definition",
                    "repository":repository,
                    "idempotency_key":"standard-process-run","now_unix_ms":2
                },
                "steps":[{
                    "id":"standard-process-step","key":"process",
                    "implementation":"prism.standard/command","target_id":"local",
                    "input_json":serde_json::json!({
                        "executable":"/bin/sh",
                        "arguments":["-c","printf workflow-host-process"],
                        "working_scope":{"id":scope_id,"revision":incarnation},
                        "environment":{},"timeout_ms":5000,"max_output_bytes":4096
                    }).to_string(),
                    "dependencies":[],"resources":[]
                }]
            })
        ),
        serde_json::json!({"ok":true,"run_id":"standard-process-run"})
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = worker_request(
            &runtime,
            serde_json::json!({"type":"workflow_inspect","run_id":"standard-process-run"}),
        );
        if response["run"]["status"] == "succeeded" {
            assert!(
                response["run"]["attempts"][0]["process_id"]
                    .as_i64()
                    .is_some_and(|pid| pid > 0)
            );
            assert!(response["run"]["events"].as_array().is_some_and(|events| {
                events
                    .iter()
                    .any(|event| event["kind"] == "process_recorded")
            }));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Standard process did not finish: {response}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        run(&runtime, &home, &["worker", "shutdown"])
            .status
            .success()
    );
}

#[test]
fn production_worker_executes_the_standard_extension() {
    let _serial = serial_worker_test();
    let temp = TempDir::new("worker-standard-extension");
    let runtime = temp.runtime_path().to_path_buf();
    let home = temp.path.join("home");
    fs::create_dir_all(&home).unwrap();

    assert!(run(&runtime, &home, &["worker", "ensure"]).status.success());
    assert_eq!(
        worker_request(
            &runtime,
            serde_json::json!({
                "type": "workflow_register_definition",
                "definition": {
                    "id": "standard-definition",
                    "name": "standard",
                    "revision": "1",
                    "source": "test",
                    "trusted": true,
                    "body_json": "{}",
                    "digest": "standard-digest",
                    "now_unix_ms": 1
                }
            }),
        ),
        serde_json::json!({"ok": true})
    );
    assert_eq!(
        worker_request(
            &runtime,
            serde_json::json!({
                "type": "workflow_launch",
                "run": {
                    "run_id": "standard-run",
                    "definition_snapshot_id": "standard-definition",
                    "repository": null,
                    "idempotency_key": "standard-run",
                    "now_unix_ms": 2
                },
                "steps": [{
                    "id": "standard-step",
                    "key": "echo",
                    "implementation": "prism.standard/echo",
                    "target_id": "local",
                    "input_json": "{}",
                    "dependencies": [],
                    "resources": []
                }]
            }),
        ),
        serde_json::json!({"ok": true, "run_id": "standard-run"})
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = worker_request(
            &runtime,
            serde_json::json!({"type": "workflow_inspect", "run_id": "standard-run"}),
        );
        if response["run"]["status"] == "succeeded" {
            assert_eq!(response["run"]["attempts"][0]["status"], "succeeded");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Standard Extension workflow did not finish: {response}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(
        run(&runtime, &home, &["worker", "shutdown"])
            .status
            .success()
    );
}

#[test]
fn platform_smoke_native_worker_starts_once_reports_health_and_shuts_down() {
    let _serial = serial_worker_test();
    let temp = TempDir::new("worker-start");
    let runtime = temp.runtime_path().to_path_buf();
    let home = temp.path.join("home");
    fs::create_dir_all(&home).unwrap();

    let mut starts = Vec::new();
    for _ in 0..4 {
        starts.push(
            prism(&runtime, &home)
                .args(["worker", "ensure"])
                .spawn()
                .expect("spawn concurrent worker ensure"),
        );
    }
    for mut start in starts {
        assert!(start.wait().expect("wait for worker ensure").success());
    }

    let health = run(&runtime, &home, &["worker", "health"]);
    assert!(health.status.success());
    let health = String::from_utf8_lossy(&health.stdout);
    assert!(health.starts_with("ok 3 "), "unexpected health: {health}");
    assert!(health.contains("generation="));
    assert!(health.contains("state=running active=0"));

    let second = run(&runtime, &home, &["worker", "serve"]);
    assert!(!second.status.success());

    let shutdown = run(&runtime, &home, &["worker", "shutdown"]);
    assert!(
        shutdown.status.success(),
        "shutdown failed: {}",
        String::from_utf8_lossy(&shutdown.stderr)
    );
    assert!(!runtime.join("worker.sock").exists());
}

#[test]
fn platform_smoke_native_worker_recovers_stale_socket_and_lock_files() {
    let _serial = serial_worker_test();
    use std::os::unix::net::UnixListener;

    let temp = TempDir::new("worker-stale");
    let runtime = temp.runtime_path().to_path_buf();
    let home = temp.path.join("home");
    fs::create_dir_all(&runtime).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::write(runtime.join("worker.lock"), "stale").unwrap();
    let listener = UnixListener::bind(runtime.join("worker.sock")).unwrap();
    drop(listener);

    let ensure = run(&runtime, &home, &["worker", "ensure"]);
    assert!(
        ensure.status.success(),
        "ensure failed: {}",
        String::from_utf8_lossy(&ensure.stderr)
    );
    assert!(run(&runtime, &home, &["worker", "health"]).status.success());
    assert!(
        run(&runtime, &home, &["worker", "shutdown"])
            .status
            .success()
    );
}

#[test]
fn worker_ensure_rejects_an_invalid_socket_path_before_startup_side_effects() {
    let _serial = serial_worker_test();
    let temp = TempDir::new("worker-invalid-runtime");
    let runtime = temp.path.join("x".repeat(120));
    let home = temp.path.join("home");
    fs::create_dir_all(&home).unwrap();

    let ensure = run(&runtime, &home, &["worker", "ensure"]);

    assert!(!ensure.status.success());
    let error = String::from_utf8_lossy(&ensure.stderr);
    assert!(error.contains("103 bytes"), "{error}");
    assert!(error.contains("PRISM_RUNTIME_DIR"), "{error}");
    assert!(!runtime.exists());
}
