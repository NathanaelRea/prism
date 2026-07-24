use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "prism-worker-test-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create worker test directory");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

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

#[test]
fn real_worker_starts_once_reports_health_and_shuts_down() {
    let _serial = serial_worker_test();
    let temp = TempDir::new("lifecycle");
    let runtime = temp.path.join("runtime");
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
fn real_worker_recovers_stale_socket_and_lock_files() {
    let _serial = serial_worker_test();
    use std::os::unix::net::UnixListener;

    let temp = TempDir::new("stale-runtime");
    let runtime = temp.path.join("runtime");
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
fn real_worker_executes_a_queued_plan_and_persists_lifecycle() {
    let _serial = serial_worker_test();
    let temp = TempDir::new("execute-plan");
    let runtime = temp.path.join("runtime");
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
    conn.execute(
        "insert into plan_run (
           id, harness_id, adapter_id, repo_root, scope_path, plan_path, plan_display,
           step_name, start_step, total_steps, mode, status, pause_requested,
           selected_step, created_unix_ms, updated_unix_ms
         ) values ('worker-plan', 'test', 'generic', ?1, ?1, ?2, 'plan.md',
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
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let state: String = conn
            .query_row(
                "select dispatch_state from workflow_execution
                 where workflow_kind = 'plan' and run_id = 'worker-plan'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        if state == "terminal" {
            let status: String = conn
                .query_row(
                    "select status from plan_run where id = 'worker-plan'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(status, "done");
            let output: String = conn
                .query_row(
                    "select text from plan_output_line
                     where run_id = 'worker-plan' order by line_number desc limit 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(output, "worker output");
            let events: i64 = conn
                .query_row(
                    "select count(*) from event where target = 'worker'
                     and action in ('claim', 'executor_start', 'release', 'executor_stop')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(events >= 4);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker did not finish queued plan"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        run(&runtime, &home, &["worker", "shutdown"])
            .status
            .success()
    );
}

#[test]
fn daemon_crash_leaves_claimed_work_recovery_pending() {
    let _serial = serial_worker_test();
    let temp = TempDir::new("crash-recovery");
    let runtime = temp.path.join("runtime");
    let home = temp.path.join("home");
    let repo = temp.path.join("repo");
    fs::create_dir_all(home.join("prism")).unwrap();
    fs::create_dir_all(&repo).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    let harness = temp.path.join("sleep.sh");
    fs::write(&harness, "#!/bin/sh\nsleep 30\n").unwrap();
    fs::set_permissions(&harness, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        home.join("prism/config.toml"),
        format!(
            "default_harness = \"test\"\n[harnesses.test]\nadapter = \"generic\"\ninteractive_command = [\"{}\"]\nheadless_command = [\"{}\", \"{{prompt}}\"]\nheadless_prompt_transport = \"argument\"\noutput_format = \"text\"\n",
            harness.display(), harness.display()
        ),
    )
    .unwrap();
    fs::write(
        home.join("prism/repos.toml"),
        format!("[[repos]]\npath = \"{}\"\n", repo.display()),
    )
    .unwrap();
    let db = run(
        &runtime,
        &home,
        &["--repo", repo.to_str().unwrap(), "db", "path"],
    );
    assert!(db.status.success());
    let db = PathBuf::from(String::from_utf8_lossy(&db.stdout).trim());
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "insert into plan_run (id, harness_id, adapter_id, repo_root, scope_path, plan_path,
           plan_display, step_name, start_step, total_steps, mode, status, pause_requested,
           selected_step, created_unix_ms, updated_unix_ms)
         values ('crash-plan', 'test', 'generic', ?1, ?1, ?2, 'plan.md', 'Phase', 1, 1,
                 'sequential', 'queued', 0, 1, 1, 1)",
        rusqlite::params![
            repo.display().to_string(),
            repo.join("plan.md").display().to_string()
        ],
    )
    .unwrap();
    conn.execute(
        "insert into plan_step_run (run_id, step, prompt, status)
         values ('crash-plan', 1, 'sleep', 'queued')",
        [],
    )
    .unwrap();
    conn.execute(
        "insert into workflow_execution (workflow_kind, run_id, dispatch_state, fencing_token,
           interruption_generation, created_unix_ms, updated_unix_ms)
         values ('plan', 'crash-plan', 'queued', 0, 0, 1, 1)",
        [],
    )
    .unwrap();
    drop(conn);
    assert!(run(&runtime, &home, &["worker", "ensure"]).status.success());

    let deadline = Instant::now() + Duration::from_secs(30);
    let harness_pid = loop {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let pid = conn
            .query_row(
                "select execution_process_id from plan_step_run where run_id = 'crash-plan'",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
            .unwrap();
        if let Some(pid) = pid {
            break pid;
        }
        assert!(Instant::now() < deadline, "harness did not start");
        std::thread::sleep(Duration::from_millis(25));
    };
    let health = run(&runtime, &home, &["worker", "health"]);
    let health = String::from_utf8_lossy(&health.stdout);
    let daemon_pid: i32 = health
        .split_whitespace()
        .find_map(|field| field.strip_prefix("pid="))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(daemon_pid, libc::SIGKILL) }, 0);

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let ensure = run(&runtime, &home, &["worker", "ensure"]);
        if ensure.status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "replacement daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let conn = rusqlite::Connection::open(&db).unwrap();
    let state: String = conn
        .query_row(
            "select dispatch_state from workflow_execution
             where workflow_kind = 'plan' and run_id = 'crash-plan'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "recovery_pending");
    std::thread::sleep(Duration::from_millis(1200));
    let state: String = conn
        .query_row(
            "select dispatch_state from workflow_execution
             where workflow_kind = 'plan' and run_id = 'crash-plan'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "recovery_pending");
    drop(conn);
    let _ = unsafe { libc::kill(-(harness_pid as i32), libc::SIGTERM) };
    assert!(
        run(&runtime, &home, &["worker", "shutdown"])
            .status
            .success()
    );
}
