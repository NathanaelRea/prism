#![cfg(unix)]

mod common;
#[allow(dead_code, unused_imports)]
#[path = "common/e2e/mod.rs"]
mod e2e;

use std::fs;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::CompactTempDir as TempDir;
use e2e::{E2eSandbox, wait_until};

fn prism_command(temp: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prism"));
    command
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
        .env("PRISM_RUNTIME_DIR", temp.runtime_path());
    command
}

fn prism(temp: &TempDir, args: &[&str]) -> Output {
    prism_command(temp).args(args).output().unwrap()
}

fn prism_with_timeout(temp: &TempDir, args: &[&str], timeout: Duration) -> Result<Output, String> {
    let mut child = prism_command(temp)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(_) => return child.wait_with_output().map_err(|error| error.to_string()),
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            None => {
                let _ = child.kill();
                let output = child
                    .wait_with_output()
                    .map_err(|error| error.to_string())?;
                return Err(format!(
                    "timed out after {timeout:?}; stderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }
    }
}

struct ProcessGuard(Option<u32>);

impl ProcessGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

fn e2e_prism(sandbox: &E2eSandbox, args: &[&str], timeout: Duration) -> Output {
    try_e2e_prism(sandbox, args, timeout)
        .unwrap_or_else(|error| panic!("prism {args:?} failed: {error}"))
}

fn try_e2e_prism(sandbox: &E2eSandbox, args: &[&str], timeout: Duration) -> Result<Output, String> {
    let mut command = sandbox.command(env!("CARGO_BIN_EXE_prism"));
    let mut child = command
        .current_dir(&sandbox.repo)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(_) => return child.wait_with_output().map_err(|error| error.to_string()),
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            None => {
                let _ = child.kill();
                let output = child
                    .wait_with_output()
                    .map_err(|error| error.to_string())?;
                return Err(format!(
                    "timed out after {timeout:?}; stderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }
    }
}

struct E2eWorker<'a>(&'a E2eSandbox);

impl Drop for E2eWorker<'_> {
    fn drop(&mut self) {
        let _ = try_e2e_prism(self.0, &["worker", "shutdown"], Duration::from_secs(2));
    }
}

#[test]
fn prompt_worker_starts_once_lists_editable_defaults_and_stops() {
    let temp = TempDir::new("prompt-worker");
    let first = prism(&temp, &["worker", "ensure"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = prism(&temp, &["worker", "ensure"]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );

    let health = prism(&temp, &["worker", "health"]);
    let health = String::from_utf8_lossy(&health.stdout);
    assert!(health.starts_with("ok 6 "), "unexpected health: {health}");
    assert!(health.contains("state=running"));

    let list = prism(&temp, &["workflow", "list", "--json"]);
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(value["kind"], "workflow.list");
    assert!(
        value["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "stabilize")
    );
    assert!(
        temp.path()
            .join("config/prism/workflows/stabilize.toml")
            .is_file()
    );

    let shutdown = prism(&temp, &["worker", "shutdown"]);
    assert!(
        shutdown.status.success(),
        "{}",
        String::from_utf8_lossy(&shutdown.stderr)
    );
    let stopped = prism(&temp, &["worker", "health"]);
    assert!(String::from_utf8_lossy(&stopped.stdout).is_empty() || !stopped.status.success());
}

#[test]
fn ensure_replaces_an_unresponsive_old_generation_worker() {
    let temp = TempDir::new("worker-old-generation");
    fs::create_dir_all(temp.runtime_path()).unwrap();
    let stale_generation = "0".repeat(64);
    let mut command = prism_command(&temp);
    command
        .args(["worker", "serve"])
        .env("PRISM_WORKER_GENERATION", &stale_generation)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut stale = command.spawn().unwrap();
    let stale_pid = stale.id();
    let mut stale_guard = ProcessGuard(Some(stale_pid));
    let stale_reaper = thread::spawn(move || stale.wait());
    let initial = wait_until(Duration::from_secs(3), "stale Worker to start", || {
        let health = prism(&temp, &["worker", "health"]);
        health.status.success().then_some(health)
    });
    assert!(
        String::from_utf8_lossy(&initial.stdout)
            .contains(&format!("generation={stale_generation}"))
    );
    let lock_path = temp.runtime_path().join("worker.lock");
    let owner: serde_json::Value = serde_json::from_slice(&fs::read(&lock_path).unwrap()).unwrap();
    assert_eq!(owner["pid"], stale_pid);
    assert_eq!(owner["binary_generation"], stale_generation);
    assert!(owner["process_identity"].as_u64().is_some());
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&lock_path)
        .unwrap();

    assert_eq!(
        unsafe { libc::kill(stale_pid as libc::pid_t, libc::SIGSTOP) },
        0
    );

    let ensured = prism_with_timeout(&temp, &["worker", "ensure"], Duration::from_secs(20))
        .unwrap_or_else(|error| panic!("failed to replace stopped old Worker: {error}"));
    assert!(
        ensured.status.success(),
        "{}",
        String::from_utf8_lossy(&ensured.stderr)
    );
    let replacement = prism(&temp, &["worker", "health"]);
    let replacement = String::from_utf8_lossy(&replacement.stdout);
    assert!(replacement.contains("state=running"), "{replacement}");
    assert!(
        !replacement.contains(&format!("pid={stale_pid} ")),
        "{replacement}"
    );
    assert!(!replacement.contains(&format!("generation={stale_generation}")));

    stale_reaper.join().unwrap().unwrap();
    stale_guard.disarm();
    let shutdown = prism(&temp, &["worker", "shutdown"]);
    assert!(
        shutdown.status.success(),
        "{}",
        String::from_utf8_lossy(&shutdown.stderr)
    );
}

#[test]
fn stabilization_observation_wakes_the_real_worker_without_deadlocking() {
    let sandbox = E2eSandbox::new("worker-observation-wake");
    let worktree = sandbox.worktrees.join("feature-status");
    let created = sandbox.git(
        &sandbox.repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature/status",
            worktree.to_str().unwrap(),
            "main",
        ],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    fs::write(
        worktree.join("status.txt"),
        "exercise remote observation wake\n",
    )
    .unwrap();
    assert!(
        sandbox
            .git(&worktree, &["add", "status.txt"])
            .status
            .success()
    );
    assert!(
        sandbox
            .git(&worktree, &["commit", "-m", "exercise observation wake"])
            .status
            .success()
    );
    assert!(
        sandbox
            .git(&worktree, &["push", "-u", "origin", "HEAD"])
            .status
            .success()
    );
    let head = sandbox.git_stdout(&worktree, &["rev-parse", "HEAD"]);

    let pull_request = sandbox
        .command(sandbox.bin.join("gh"))
        .current_dir(&worktree)
        .args(["pr", "create", "--fill", "--base", "main"])
        .output()
        .unwrap();
    assert!(
        pull_request.status.success(),
        "{}",
        String::from_utf8_lossy(&pull_request.stderr)
    );

    let ensured = e2e_prism(&sandbox, &["worker", "ensure"], Duration::from_secs(10));
    assert!(
        ensured.status.success(),
        "{}",
        String::from_utf8_lossy(&ensured.stderr)
    );
    let _worker = E2eWorker(&sandbox);
    let workflows = sandbox.config_home.join("prism/workflows");
    fs::create_dir_all(&workflows).unwrap();
    fs::write(
        workflows.join("observation-wake.toml"),
        r#"[[step]]
trigger = "merge_conflict"
prompt = "Resolve the merge conflict."
"#,
    )
    .unwrap();

    let launched = e2e_prism(
        &sandbox,
        &[
            "--repo",
            sandbox.repo.to_str().unwrap(),
            "workflow",
            "run",
            "observation-wake",
            "--worktree",
            worktree.to_str().unwrap(),
            "--change-request",
            "github:github.com:prism-e2e/project:change_request:PR_e2e_1",
            "--change-request-head",
            &head,
            "--json",
        ],
        Duration::from_secs(10),
    );
    assert!(
        launched.status.success(),
        "{}",
        String::from_utf8_lossy(&launched.stderr)
    );
    let launched: serde_json::Value = serde_json::from_slice(&launched.stdout).unwrap();
    let run_id = launched["data"]["run_id"].as_str().unwrap().to_string();

    let completed = wait_until(
        Duration::from_secs(20),
        "stabilization Workflow to complete after its remote observation",
        || {
            // A deadlocked Worker also stops serving this history request, so bound every
            // subprocess independently rather than allowing the regression test to hang.
            let history = e2e_prism(
                &sandbox,
                &[
                    "--repo",
                    sandbox.repo.to_str().unwrap(),
                    "workflow",
                    "history",
                    &run_id,
                    "--json",
                ],
                Duration::from_secs(10),
            );
            assert!(
                history.status.success(),
                "{}",
                String::from_utf8_lossy(&history.stderr)
            );
            let history: serde_json::Value = serde_json::from_slice(&history.stdout).unwrap();
            let run = history["data"][0].clone();
            match run["status"].as_str() {
                Some("succeeded") => Some(run),
                Some("failed" | "cancelled" | "recovery_required") => {
                    panic!("stabilization Workflow terminated unexpectedly: {run:#}")
                }
                _ => None,
            }
        },
    );
    assert!(
        completed["steps"]
            .as_array()
            .is_some_and(|steps| steps.len() == 1 && steps[0]["phase"] == "satisfied"),
        "unexpected completed steps: {}",
        completed["steps"]
    );

    let health = e2e_prism(&sandbox, &["worker", "health"], Duration::from_secs(3));
    assert!(health.status.success());
    assert!(String::from_utf8_lossy(&health.stdout).contains("state=running"));
    assert!(sandbox.events().iter().any(|event| event["tool"] == "gh"));
    sandbox.assert_clean_adapters();
}
