mod common;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use common::CompactTempDir as TempDir;

fn prism(runtime: &Path, home: &Path) -> Command {
    prism_at(Path::new(env!("CARGO_BIN_EXE_prism")), runtime, home)
}

fn prism_at(executable: &Path, runtime: &Path, home: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .env("PRISM_RUNTIME_DIR", runtime)
        .env("XDG_CONFIG_HOME", home)
        .env("HOME", home);
    command
}

fn health_pid(output: &Output) -> u32 {
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find_map(|field| field.strip_prefix("pid="))
        .and_then(|pid| pid.parse().ok())
        .expect("worker health PID")
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
    assert!(health.starts_with("ok 1 "), "unexpected health: {health}");
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
fn worker_ensure_restarts_a_same_version_replaced_executable() {
    let _serial = serial_worker_test();
    let temp = TempDir::new("worker-replaced-executable");
    let runtime = temp.runtime_path().to_path_buf();
    let home = temp.path.join("home");
    let installed = temp.path.join("prism");
    let replacement = temp.path.join("prism.new");
    fs::create_dir_all(&home).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_prism"), &installed).unwrap();

    let first = prism_at(&installed, &runtime, &home)
        .args(["worker", "ensure"])
        .output()
        .unwrap();
    assert!(first.status.success());
    let first_health = prism_at(&installed, &runtime, &home)
        .args(["worker", "health"])
        .output()
        .unwrap();
    let first_pid = health_pid(&first_health);

    fs::copy(env!("CARGO_BIN_EXE_prism"), &replacement).unwrap();
    fs::rename(&replacement, &installed).unwrap();
    let ensure = prism_at(&installed, &runtime, &home)
        .args(["worker", "ensure"])
        .output()
        .unwrap();
    assert!(
        ensure.status.success(),
        "replacement ensure failed: {}",
        String::from_utf8_lossy(&ensure.stderr)
    );
    let second_health = prism_at(&installed, &runtime, &home)
        .args(["worker", "health"])
        .output()
        .unwrap();
    assert_ne!(health_pid(&second_health), first_pid);
    assert!(String::from_utf8_lossy(&second_health.stdout).contains(" exe="));

    assert!(
        prism_at(&installed, &runtime, &home)
            .args(["worker", "shutdown"])
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn worker_ensure_schedules_replacement_for_active_legacy_draining_daemon() {
    let _serial = serial_worker_test();
    let temp = TempDir::new("worker-legacy-draining");
    let runtime = temp.runtime_path().to_path_buf();
    let home = temp.path.join("home");
    fs::create_dir_all(&runtime).unwrap();
    fs::create_dir_all(&home).unwrap();

    let socket = runtime.join("worker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = Arc::clone(&stop);
    let server = std::thread::spawn(move || {
        loop {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("accept legacy worker request: {error}"),
            };
            let mut request = [0_u8; 64];
            let size = stream.read(&mut request).unwrap();
            let request = std::str::from_utf8(&request[..size]).unwrap().trim();
            if server_stop.load(Ordering::Acquire) && request == "health" {
                return;
            }
            match request {
                "health" => stream
                    .write_all(b"ok 1 legacy pid=42 state=draining active=1 exe=legacy\n")
                    .unwrap(),
                "replace" => stream.write_all(b"error unknown-command\n").unwrap(),
                request => panic!("unexpected legacy worker request: {request}"),
            }
        }
    });

    let ensure = run(&runtime, &home, &["worker", "ensure"]);
    stop.store(true, Ordering::Release);
    server.join().unwrap();
    fs::remove_file(&socket).unwrap();

    assert!(
        ensure.status.success(),
        "ensure against legacy draining worker failed: {}",
        String::from_utf8_lossy(&ensure.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let health = run(&runtime, &home, &["worker", "health"]);
        if health.status.success()
            && String::from_utf8_lossy(&health.stdout).contains("state=running active=0")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "replacement daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        run(&runtime, &home, &["worker", "shutdown"])
            .status
            .success()
    );
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

#[test]
fn worker_never_executes_legacy_queued_runs() {
    let _serial = serial_worker_test();
    let temp = TempDir::new("worker-plan");
    let runtime = temp.runtime_path().to_path_buf();
    let home = temp.path.join("home");
    let repo = temp.path.join("repo");
    fs::create_dir_all(home.join("prism")).unwrap();
    fs::create_dir_all(&repo).unwrap();
    let git = Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(git.status.success());

    let harness = temp.path.join("harness.sh");
    fs::write(&harness, "#!/bin/sh\nprintf 'worker output\\n'\n").unwrap();
    fs::set_permissions(&harness, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        home.join("prism/config.toml"),
        format!(
            "default_harness = \"test\"\n\n[harnesses.test]\nadapter = \"generic\"\ninteractive_command = [\"{}\"]\nheadless_command = [\"{}\", \"{{prompt}}\"]\nheadless_prompt_transport = \"argument\"\noutput_format = \"text\"\n",
            harness.display(),
            harness.display(),
        ),
    )
    .unwrap();
    fs::write(
        home.join("prism/repos.toml"),
        format!("[[repos]]\npath = \"{}\"\n", repo.display()),
    )
    .unwrap();

    let db_path = run(
        &runtime,
        &home,
        &["--repo", repo.to_str().unwrap(), "db", "path"],
    );
    assert!(db_path.status.success());
    let db_path = PathBuf::from(String::from_utf8_lossy(&db_path.stdout).trim());
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    insert_active_worktree_session(&conn, "00000000000000000000000000000001", &repo);
    conn.execute(
        "insert into plan_run (
           id, harness_id, adapter_id, repo_root, scope_path, worktree_session_id, plan_path, plan_display,
           step_name, start_step, total_steps, mode, status, pause_requested,
           selected_step, created_unix_ms, updated_unix_ms
         ) values ('worker-plan', 'test', 'generic', ?1, ?1, '00000000000000000000000000000001', ?2, 'plan.md',
                   'Phase', 1, 1, 'sequential', 'queued', 0, 1, 1, 1)",
        rusqlite::params![
            repo.display().to_string(),
            repo.join("plan.md").display().to_string()
        ],
    )
    .unwrap();
    conn.execute(
        "insert into plan_step_run (run_id, step, prompt, status)
         values ('worker-plan', 1, 'execute the deterministic test', 'queued')",
        [],
    )
    .unwrap();
    conn.execute(
        "insert into workflow_execution (
           workflow_kind, run_id, dispatch_state, fencing_token,
           interruption_generation, created_unix_ms, updated_unix_ms
         ) values ('plan', 'worker-plan', 'queued', 0, 0, 1, 1)",
        [],
    )
    .unwrap();
    drop(conn);

    assert!(run(&runtime, &home, &["worker", "ensure"]).status.success());
    std::thread::sleep(Duration::from_millis(500));
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let (dispatch, status): (String, String) = conn
        .query_row(
            "select execution.dispatch_state,run.status from workflow_execution execution join plan_run run on run.id=execution.run_id where execution.workflow_kind='plan' and execution.run_id='worker-plan'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((dispatch.as_str(), status.as_str()), ("queued", "queued"));
    drop(conn);
    assert!(
        run(&runtime, &home, &["worker", "shutdown"])
            .status
            .success()
    );
}

#[test]
fn worker_executes_a_generalized_command_attempt() {
    let _serial = serial_worker_test();
    let temp = TempDir::new("worker-generalized-command");
    let runtime = temp.runtime_path().to_path_buf();
    let home = temp.path.join("home");
    let repo = temp.path.join("repo");
    fs::create_dir_all(home.join("prism/workflows")).unwrap();
    fs::create_dir_all(&repo).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    fs::write(repo.join("seed"), "seed").unwrap();
    assert!(
        Command::new("git")
            .args(["add", "seed"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "seed"
            ])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );

    let marker = temp.path.join("generalized-ran");
    let command = temp.path.join("command.sh");
    fs::write(
        &command,
        format!(
            "#!/bin/sh\nprintf ran > '{}'\nprintf result\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&command, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        home.join("prism/workflows/command.toml"),
        format!("schema_version = 1\nname = \"command\"\ncapabilities = [\"process_execute\"]\n[inputs.task]\nartifact_type = \"builtin:task@1\"\n[budgets]\nmax_attempts = 1\nmax_fan_out = 1\nmax_child_depth = 0\nmax_mutations = 0\n[[steps]]\nid = \"run\"\nclass = \"action\"\nimplementation = \"builtin:command@1\"\ncapabilities = [\"process_execute\"]\n[steps.inputs.task]\nfrom = \"run.task\"\nartifact_type = \"builtin:task@1\"\n[steps.outputs.result]\nartifact_type = \"builtin:task@1\"\n[steps.settings]\ncommand = [\"{}\"]\n", command.display()),
    ).unwrap();

    let migrate = run(
        &runtime,
        &home,
        &[
            "--repo",
            repo.to_str().unwrap(),
            "config",
            "migrate-workflows",
        ],
    );
    assert!(
        migrate.status.success(),
        "{}",
        String::from_utf8_lossy(&migrate.stderr)
    );
    let launch = run(
        &runtime,
        &home,
        &[
            "--repo",
            repo.to_str().unwrap(),
            "workflow",
            "launch",
            "global:command",
            "--input",
            "task={}",
        ],
    );
    assert!(
        launch.status.success(),
        "{}",
        String::from_utf8_lossy(&launch.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "generalized command Attempt did not execute"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(fs::read_to_string(marker).unwrap(), "ran");
    assert!(
        run(&runtime, &home, &["worker", "shutdown"])
            .status
            .success()
    );
}

fn insert_active_worktree_session(conn: &rusqlite::Connection, id: &str, repo: &Path) {
    let path = repo.display().to_string();
    let mut repo_hash = 0xcbf29ce484222325_u64;
    for byte in path.as_bytes() {
        repo_hash ^= u64::from(*byte);
        repo_hash = repo_hash.wrapping_mul(0x100000001b3);
    }
    fs::write(
        repo.join(".git/prism-worktree-session-id"),
        format!("{repo_hash:016x}:{id}\n"),
    )
    .unwrap();
    conn.execute(
        "insert into worktree_session
         (id, repo_root, initial_branch, initial_worktree_path, created_unix_ms)
         values (?1, ?2, 'main', ?2, 1)",
        rusqlite::params![id, path],
    )
    .unwrap();
    conn.execute(
        "insert into active_worktree_session
         (worktree_session_id, repo_root, branch, worktree_path, worktree_incarnation, observed_unix_ms)
         values (?1, ?2, 'main', ?2, 'test', 1)",
        rusqlite::params![id, path],
    )
    .unwrap();
}
