use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::execution::{self, DispatchState, ExecutionClaim, WorkflowIdentity, WorkflowKind};
use crate::process::DetachedProcessPolicy;
use crate::repo::Repository;
use crate::util::stable_hash;
use crate::{observability, workspace};

const PROTOCOL_VERSION: u32 = 1;
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const DAEMON_TRANSITION_TIMEOUT: Duration = Duration::from_secs(3);
const GLOBAL_CONCURRENCY: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonState {
    Running,
    Draining,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DaemonHealth {
    pub state: DaemonState,
    pub protocol_version: Option<u32>,
    pub instance_id: Option<String>,
    pub pid: Option<u32>,
    pub active: usize,
}

impl DaemonHealth {
    pub fn stopped() -> Self {
        Self {
            state: DaemonState::Stopped,
            protocol_version: None,
            instance_id: None,
            pid: None,
            active: 0,
        }
    }
}

pub fn probe_health() -> Result<DaemonHealth, String> {
    probe_health_at(&socket_path())
}

fn probe_health_at(path: &Path) -> Result<DaemonHealth, String> {
    let stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            return Ok(DaemonHealth::stopped());
        }
        Err(error) => return Err(format!("connect to Prism worker: {error}")),
    };
    parse_health_response(&request_on_stream(stream, "health")?)
}

fn parse_health_response(response: &str) -> Result<DaemonHealth, String> {
    let mut fields = response.split_whitespace();
    if fields.next() != Some("ok") {
        return Err(format!("invalid Prism daemon response: {response}"));
    }
    let version = fields
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| format!("invalid Prism daemon protocol: {response}"))?;
    if version != PROTOCOL_VERSION {
        return Err(format!("incompatible Prism daemon protocol {version}"));
    }
    let instance_id = fields
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing Prism daemon instance ID: {response}"))?;
    let mut pid: Option<u32> = None;
    let mut state = None;
    let mut active = None;
    for field in fields {
        if let Some(value) = field.strip_prefix("pid=") {
            pid = value.parse().ok();
        } else if let Some(value) = field.strip_prefix("state=") {
            state = Some(match value {
                "running" => DaemonState::Running,
                "draining" => DaemonState::Draining,
                _ => return Err(format!("unknown Prism daemon state: {value}")),
            });
        } else if let Some(value) = field.strip_prefix("active=") {
            active = value.parse().ok();
        }
    }
    Ok(DaemonHealth {
        state: state.ok_or_else(|| format!("missing Prism daemon state: {response}"))?,
        protocol_version: Some(version),
        instance_id: Some(instance_id.to_string()),
        pid: Some(pid.ok_or_else(|| format!("missing Prism daemon PID: {response}"))?),
        active: active.ok_or_else(|| format!("missing Prism daemon active count: {response}"))?,
    })
}

pub fn ensure_running() -> Result<(), String> {
    if wait_for_existing_daemon(DAEMON_TRANSITION_TIMEOUT, probe_health)? {
        return Ok(());
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve Prism worker executable: {error}"))?;
    let mut command = Command::new(executable);
    command.args(["worker", "serve"]);
    crate::process::spawn_detached_named(
        &mut command,
        DetachedProcessPolicy::WorkerDaemon,
        crate::process::ProcessDescriptor::new("prism.worker.serve"),
    )
    .map_err(|error| format!("start Prism worker daemon: {error}"))?;

    let deadline = Instant::now() + DAEMON_TRANSITION_TIMEOUT;
    let mut last_error = "worker did not become ready".to_string();
    while Instant::now() < deadline {
        match health() {
            Ok(()) => return Ok(()),
            Err(error) => last_error = error,
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err(last_error)
}

fn wait_for_existing_daemon(
    timeout: Duration,
    mut probe: impl FnMut() -> Result<DaemonHealth, String>,
) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match probe() {
            Ok(DaemonHealth {
                state: DaemonState::Running,
                ..
            }) => return Ok(true),
            Ok(DaemonHealth {
                state: DaemonState::Draining,
                active,
                ..
            }) => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "timed out waiting for Prism worker daemon to finish draining ({active} active)"
                    ));
                }
                thread::sleep(Duration::from_millis(25));
            }
            Ok(DaemonHealth {
                state: DaemonState::Stopped,
                ..
            })
            | Err(_) => return Ok(false),
        }
    }
}

pub fn wake() -> Result<(), String> {
    request("wake").map(|_| ())
}

pub fn health() -> Result<(), String> {
    parse_health_response(&health_response()?).map(|_| ())
}

pub fn health_response() -> Result<String, String> {
    request("health")
}

pub fn shutdown() -> Result<(), String> {
    let response = request("shutdown")?;
    if !response.starts_with(&format!("ok {PROTOCOL_VERSION} ")) {
        return Err(format!("Prism worker rejected shutdown: {response}"));
    }
    wait_for_socket_to_close(&socket_path(), DAEMON_TRANSITION_TIMEOUT)
}

fn wait_for_socket_to_close(path: &Path, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("inspect Prism worker socket: {error}")),
            Ok(_) => {}
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for Prism worker daemon to stop".to_string());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn request(command: &str) -> Result<String, String> {
    let stream = UnixStream::connect(socket_path())
        .map_err(|error| format!("connect to Prism worker: {error}"))?;
    request_on_stream(stream, command)
}

fn request_on_stream(mut stream: UnixStream, command: &str) -> Result<String, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .map_err(|error| format!("configure Prism worker socket: {error}"))?;
    stream
        .write_all(format!("{command}\n").as_bytes())
        .map_err(|error| format!("write Prism worker request: {error}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("read Prism worker response: {error}"))?;
    Ok(response.trim().to_string())
}

pub fn serve() -> Result<(), String> {
    let runtime = runtime_dir();
    if let Ok(metadata) = fs::symlink_metadata(&runtime) {
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "Prism worker runtime directory is a symlink: {}",
                runtime.display()
            ));
        }
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(format!(
                "Prism worker runtime directory is owned by another user: {}",
                runtime.display()
            ));
        }
    }
    fs::create_dir_all(&runtime).map_err(|error| format!("create worker runtime dir: {error}"))?;
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("secure worker runtime dir: {error}"))?;
    let _lock = acquire_lock(&runtime.join("worker.lock"))?;
    let socket = runtime.join("worker.sock");
    if socket.exists() {
        match UnixStream::connect(&socket) {
            Ok(_) => {
                return Err(
                    "a live Prism worker endpoint already owns the runtime socket".to_string(),
                );
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) => {}
            Err(error) => {
                return Err(format!(
                    "cannot safely classify existing Prism worker socket: {error}"
                ));
            }
        }
        fs::remove_file(&socket).map_err(|error| format!("remove stale worker socket: {error}"))?;
    }

    let instance_id = execution::new_instance_id("daemon");
    classify_abandoned(&instance_id)?;
    log_daemon_lifecycle("daemon_start", &instance_id);
    let listener = UnixListener::bind(&socket)
        .map_err(|error| format!("bind Prism worker socket {}: {error}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("secure Prism worker socket: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("configure Prism worker listener: {error}"))?;

    let active = Arc::new(Mutex::new(BTreeSet::<PathBuf>::new()));
    let mut next_poll = Instant::now();
    let mut draining = false;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if respond(&mut stream, &instance_id, &active, draining) {
                    draining = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(format!("accept Prism worker connection: {error}")),
        }
        if draining
            && active
                .lock()
                .map(|active| active.is_empty())
                .unwrap_or(false)
        {
            break;
        }
        if !draining && Instant::now() >= next_poll {
            schedule_queued(&instance_id, Arc::clone(&active));
            next_poll = Instant::now() + POLL_INTERVAL;
        }
        thread::sleep(Duration::from_millis(50));
    }
    log_daemon_lifecycle("daemon_stop", &instance_id);
    fs::remove_file(&socket).map_err(|error| format!("remove worker socket: {error}"))
}

fn respond(
    stream: &mut UnixStream,
    instance_id: &str,
    active: &Arc<Mutex<BTreeSet<PathBuf>>>,
    draining: bool,
) -> bool {
    let mut request = [0_u8; 64];
    let size = stream.read(&mut request).unwrap_or(0);
    let command = String::from_utf8_lossy(&request[..size]);
    let active = active
        .lock()
        .map(|active| active.len())
        .unwrap_or(usize::MAX);
    let response = match command.trim() {
        "health" | "wake" => format!(
            "ok {PROTOCOL_VERSION} {instance_id} pid={} state={} active={active}\n",
            std::process::id(),
            if draining { "draining" } else { "running" }
        ),
        "shutdown" => format!(
            "ok {PROTOCOL_VERSION} {instance_id} pid={} state=draining active={active}\n",
            std::process::id()
        ),
        _ => "error unknown-command\n".to_string(),
    };
    let _ = stream.write_all(response.as_bytes());
    command.trim() == "shutdown"
}

fn classify_abandoned(instance_id: &str) -> Result<(), String> {
    for entry in workspace::discover_valid_entries(workspace::load_entries()?) {
        observability::attach_run_repo(&entry.repo)?;
        observability::with_writable_db(&entry.repo, |conn| {
            execution::mark_abandoned(conn, instance_id).map(|_| ())
        })?;
    }
    Ok(())
}

fn schedule_queued(instance_id: &str, active: Arc<Mutex<BTreeSet<PathBuf>>>) {
    let active_count = active
        .lock()
        .map(|active| active.len())
        .unwrap_or(usize::MAX);
    if active_count >= GLOBAL_CONCURRENCY {
        return;
    }
    let entries = match workspace::load_entries() {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("Prism worker cannot load repositories: {error}");
            return;
        }
    };
    for entry in workspace::discover_valid_entries(entries) {
        let repo = entry.repo;
        if let Err(error) = observability::attach_run_repo(&repo) {
            eprintln!(
                "Prism worker cannot attach repository {}: {error}",
                repo.root.display()
            );
            continue;
        }
        let _ = observability::with_writable_db(&repo, |conn| {
            execution::mark_abandoned(conn, instance_id).map(|_| ())
        });
        let queued = observability::with_writable_db(&repo, |conn| execution::queued(conn, 16));
        let Ok(queued) = queued else {
            continue;
        };
        for workflow in queued {
            if active
                .lock()
                .map(|active| active.len())
                .unwrap_or(usize::MAX)
                >= GLOBAL_CONCURRENCY
            {
                return;
            }
            let Ok(worktree) = workflow_worktree(&repo, &workflow) else {
                continue;
            };
            let config = Config::load(&repo);
            if !matches!(legacy_worker_running(&repo, &config, &workflow), Ok(false)) {
                continue;
            }
            let inserted = active
                .lock()
                .map(|mut active| active.insert(worktree.clone()))
                .unwrap_or(false);
            if !inserted {
                continue;
            }
            let worker_id = execution::new_instance_id("executor");
            let claim = observability::with_writable_db_mut(&repo, |conn| {
                execution::claim(conn, &workflow, instance_id, &worker_id)
            });
            let Ok(Some(claim)) = claim else {
                if let Ok(mut active) = active.lock() {
                    active.remove(&worktree);
                }
                continue;
            };
            log_claim_lifecycle(&repo, "claim", &claim, "workflow claimed");
            let active = Arc::clone(&active);
            let executor_repo = repo.clone();
            thread::spawn(move || {
                execute_claim(&executor_repo, &claim);
                if let Ok(mut active) = active.lock() {
                    active.remove(&worktree);
                }
            });
        }
    }
}

pub fn legacy_worker_running(
    repo: &Repository,
    config: &Config,
    workflow: &WorkflowIdentity,
) -> Result<bool, String> {
    let expected = format!(
        "prism-{:016x}-worker-{}-{:016x}",
        stable_hash(&repo.root),
        workflow.kind.label(),
        stable_hash(Path::new(&workflow.run_id))
    );
    crate::tmux::named_session_exists(config, &expected)
        .map_err(|error| format!("inspect legacy tmux workers: {error}"))
}

fn workflow_worktree(repo: &Repository, workflow: &WorkflowIdentity) -> Result<PathBuf, String> {
    observability::with_writable_db(repo, |conn| {
        let (table, column) = match workflow.kind {
            WorkflowKind::Auto => ("auto_run", "worktree_path"),
            WorkflowKind::Plan => ("plan_run", "scope_path"),
        };
        conn.query_row(
            &format!("select {column} from {table} where id = ?1"),
            [&workflow.run_id],
            |row| row.get::<_, String>(0),
        )
        .map(PathBuf::from)
        .map_err(|error| format!("load workflow worktree: {error}"))
    })
}

fn execute_claim(repo: &Repository, claim: &ExecutionClaim) {
    log_claim_lifecycle(repo, "executor_start", claim, "workflow executor started");
    let heartbeat_stop = Arc::new(AtomicBool::new(false));
    let ownership_lost = Arc::new(AtomicBool::new(false));
    let heartbeat = spawn_heartbeat(
        repo.clone(),
        claim.clone(),
        Arc::clone(&heartbeat_stop),
        Arc::clone(&ownership_lost),
    );
    let config = Config::load(repo);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        observability::with_writable_db(repo, |conn| execution::validate_claim(conn, claim))
            .and_then(|()| match claim.workflow.kind {
                WorkflowKind::Auto => execute_auto(repo, &config, claim),
                WorkflowKind::Plan => execute_plan(repo, &config, claim),
            })
    }))
    .unwrap_or_else(|_| Err("workflow executor panicked".to_string()));
    heartbeat_stop.store(true, Ordering::Release);
    let _ = heartbeat.join();

    let state = match result {
        Ok(()) => workflow_release_state(repo, &claim.workflow).unwrap_or(DispatchState::Terminal),
        Err(error) => {
            if !ownership_lost.load(Ordering::Acquire) {
                mark_domain_failed(repo, claim, &error);
            }
            DispatchState::Terminal
        }
    };
    match observability::with_writable_db(repo, |conn| execution::release(conn, claim, state)) {
        Ok(()) => log_claim_lifecycle(repo, "release", claim, state.label()),
        Err(error) => log_claim_lifecycle(repo, "release_failed", claim, &error),
    }
    log_claim_lifecycle(repo, "executor_stop", claim, "workflow executor stopped");
}

fn spawn_heartbeat(
    repo: Repository,
    claim: ExecutionClaim,
    stop: Arc<AtomicBool>,
    ownership_lost: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
        while !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(100));
            if stop.load(Ordering::Acquire) {
                break;
            }
            if Instant::now() < next_heartbeat {
                continue;
            }
            if observability::with_writable_db(&repo, |conn| execution::heartbeat(conn, &claim))
                .is_err()
            {
                let validation = observability::with_writable_db(&repo, |conn| {
                    execution::validate_claim(conn, &claim)
                });
                if matches!(
                    validation,
                    Err(ref error) if execution::is_stale_claim_error(error)
                ) {
                    ownership_lost.store(true, Ordering::Release);
                    log_claim_lifecycle(
                        &repo,
                        "heartbeat_lost",
                        &claim,
                        "execution ownership lost",
                    );
                    break;
                }
            }
            next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
        }
    })
}

fn execute_auto(repo: &Repository, config: &Config, claim: &ExecutionClaim) -> Result<(), String> {
    let run_id = &claim.workflow.run_id;
    let mut persisted = observability::with_writable_db(repo, |conn| {
        crate::auto_flow::load_auto_run(conn, run_id)
    })?
    .ok_or_else(|| format!("auto flow run not found: {run_id}"))?;
    let harness_config = config
        .harness_config(&persisted.run.harness_id)
        .map_err(|_| {
            format!(
                "auto run harness '{}' is no longer configured",
                persisted.run.harness_id
            )
        })?;
    if harness_config.adapter != persisted.run.adapter_id {
        return Err(format!(
            "auto run harness '{}' was recorded with adapter '{}', but it is now configured as '{}'",
            persisted.run.harness_id, persisted.run.adapter_id, harness_config.adapter
        ));
    }
    let runtime = crate::harness::Harness::new(&persisted.run.harness_id, &harness_config)
        .prepare_server(
            repo,
            config,
            &persisted.run.branch,
            &persisted.run.worktree_path,
        )?
        .map(|runtime| runtime.server_url);
    let executor = crate::auto_flow::AutoExecutorConfig::for_harness(
        persisted.run.harness_id.clone(),
        harness_config,
        runtime,
        persisted.run.worktree_path.clone(),
        format!("Auto Flow {}", persisted.run.prompt_summary),
    );
    observability::with_writable_db(repo, |conn| {
        execution::install_claim_guards(conn, claim)?;
        crate::auto_flow::execute_auto_initial_step(
            conn,
            repo,
            config,
            &mut persisted,
            &executor,
            &mut std::io::sink(),
        )
    })
}

fn execute_plan(repo: &Repository, config: &Config, claim: &ExecutionClaim) -> Result<(), String> {
    let run_id = &claim.workflow.run_id;
    let mut persisted =
        observability::with_writable_db(repo, |conn| crate::plan_run::load_plan_run(conn, run_id))?
            .ok_or_else(|| format!("plan run not found: {run_id}"))?;
    let harness_config = config
        .harness_config(&persisted.run.harness_id)
        .map_err(|_| {
            format!(
                "plan run harness '{}' is no longer configured",
                persisted.run.harness_id
            )
        })?;
    if harness_config.adapter != persisted.run.adapter_id {
        return Err(format!(
            "plan run harness '{}' was recorded with adapter '{}', but it is now configured as '{}'",
            persisted.run.harness_id, persisted.run.adapter_id, harness_config.adapter
        ));
    }
    let server_url = crate::harness::Harness::new(&persisted.run.harness_id, &harness_config)
        .prepare_server(repo, config, "plan", &persisted.run.scope_path)?
        .map(|runtime| runtime.server_url);
    let mut executor = crate::plan_run::PlanExecutorConfig::for_harness(
        persisted.run.harness_id.clone(),
        harness_config.clone(),
        server_url,
        persisted.run.scope_path.clone(),
        persisted.run.plan_display.clone(),
    );
    if harness_config.adapter == "opencode"
        && config.opencode_plan_plugin
        && let Ok(plugin) = crate::plan_run::prepare_plan_plugin_config(&repo.prism_dir())
    {
        executor = executor.with_plugin_config(plugin);
    }
    observability::with_writable_db(repo, |conn| {
        execution::install_claim_guards(conn, claim)?;
        match persisted.run.mode {
            crate::plan_run::PlanRunMode::Sequential => crate::plan_run::execute_plan_sequential(
                conn,
                &mut persisted,
                &executor,
                &mut std::io::sink(),
            ),
            crate::plan_run::PlanRunMode::Parallel => crate::plan_run::execute_plan_parallel(
                conn,
                &mut persisted,
                &executor,
                &mut std::io::sink(),
            ),
        }
    })
}

fn workflow_release_state(
    repo: &Repository,
    workflow: &WorkflowIdentity,
) -> Result<DispatchState, String> {
    observability::with_writable_db(repo, |conn| {
        let table = match workflow.kind {
            WorkflowKind::Auto => "auto_run",
            WorkflowKind::Plan => "plan_run",
        };
        let status = conn
            .query_row(
                &format!("select status from {table} where id = ?1"),
                [&workflow.run_id],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| format!("load completed workflow status: {error}"))?;
        Ok(if status == "paused" {
            DispatchState::Paused
        } else {
            DispatchState::Terminal
        })
    })
}

fn mark_domain_failed(repo: &Repository, claim: &ExecutionClaim, error: &str) {
    let _ = observability::with_writable_db(repo, |conn| {
        execution::install_claim_guards(conn, claim)?;
        match claim.workflow.kind {
            WorkflowKind::Auto => {
                if let Some(mut persisted) =
                    crate::auto_flow::load_auto_run(conn, &claim.workflow.run_id)?
                {
                    crate::auto_flow::fail_auto_run(conn, &mut persisted, error.to_string())?;
                }
            }
            WorkflowKind::Plan => {
                conn.execute(
                    "update plan_run set status = 'failed', updated_unix_ms = ?1
                     where id = ?2 and status not in ('aborted', 'done')",
                    rusqlite::params![execution::now_ms(), claim.workflow.run_id],
                )
                .map_err(|db_error| format!("mark plan run failed: {db_error}"))?;
            }
        }
        Ok(())
    });
}

fn log_daemon_lifecycle(action: &str, instance_id: &str) {
    let entries = match workspace::load_entries() {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("Prism worker cannot load repositories: {error}");
            return;
        }
    };
    for entry in workspace::discover_valid_entries(entries) {
        let data = format!("{{\"daemon_instance_id\":\"{instance_id}\"}}");
        log_worker_event(
            &entry.repo,
            action,
            "Prism worker daemon lifecycle",
            Some(&data),
        );
    }
}

fn log_claim_lifecycle(repo: &Repository, action: &str, claim: &ExecutionClaim, message: &str) {
    let data = format!(
        "{{\"workflow_kind\":\"{}\",\"run_id\":{},\"worker_id\":{},\"daemon_instance_id\":{},\"fencing_token\":{}}}",
        claim.workflow.kind.label(),
        serde_json::to_string(&claim.workflow.run_id).unwrap_or_else(|_| "null".to_string()),
        serde_json::to_string(&claim.worker_id).unwrap_or_else(|_| "null".to_string()),
        serde_json::to_string(&claim.daemon_instance_id).unwrap_or_else(|_| "null".to_string()),
        claim.fencing_token,
    );
    log_worker_event(repo, action, message, Some(&data));
}

fn log_worker_event(repo: &Repository, action: &str, message: &str, data_json: Option<&str>) {
    let _ = observability::with_writable_db(repo, |conn| {
        conn.execute(
            "insert into event (
               time_unix_ms, level, target, action, repo, message, data_json
             ) values (?1, 'info', 'worker', ?2, ?3, ?4, ?5)",
            rusqlite::params![
                execution::now_ms(),
                action,
                repo.root.display().to_string(),
                message,
                data_json,
            ],
        )
        .map(|_| ())
        .map_err(|error| format!("record worker lifecycle event: {error}"))
    });
}

fn acquire_lock(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("open Prism worker lock: {error}"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("secure Prism worker lock: {error}"))?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == -1 {
        return Err(format!(
            "Prism worker is already running: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(file)
}

pub fn runtime_dir() -> PathBuf {
    let override_path = std::env::var_os("PRISM_RUNTIME_DIR").filter(|path| !path.is_empty());
    let xdg_runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|path| !path.is_empty());
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    #[cfg(target_os = "linux")]
    let target = "linux";
    #[cfg(target_os = "macos")]
    let target = "macos";
    runtime_dir_for(
        target,
        override_path.as_deref(),
        xdg_runtime.as_deref(),
        home.as_deref(),
        &crate::util::prism_config_dir(),
    )
}

fn runtime_dir_for(
    target: &str,
    override_path: Option<&std::ffi::OsStr>,
    xdg_runtime: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
    fallback_config: &Path,
) -> PathBuf {
    if let Some(path) = override_path {
        return PathBuf::from(path);
    }
    if target == "linux"
        && let Some(path) = xdg_runtime
    {
        return PathBuf::from(path).join("prism");
    }
    if target == "macos"
        && let Some(home) = home
    {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Prism")
            .join("runtime");
    }
    fallback_config.join("runtime")
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("worker.sock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn socket_and_lock_share_a_private_runtime_directory() {
        assert_eq!(socket_path().parent(), Some(runtime_dir().as_path()));
        assert_eq!(
            socket_path().file_name().and_then(|name| name.to_str()),
            Some("worker.sock")
        );
    }

    #[test]
    fn runtime_paths_cover_linux_and_macos() {
        assert_eq!(
            runtime_dir_for(
                "linux",
                None,
                Some(OsStr::new("/run/user/1000")),
                Some(OsStr::new("/home/user")),
                Path::new("/fallback"),
            ),
            PathBuf::from("/run/user/1000/prism")
        );
        assert_eq!(
            runtime_dir_for(
                "macos",
                None,
                None,
                Some(OsStr::new("/Users/user")),
                Path::new("/fallback"),
            ),
            PathBuf::from("/Users/user/Library/Application Support/Prism/runtime")
        );
        assert_eq!(
            runtime_dir_for(
                "linux",
                Some(OsStr::new("/override")),
                Some(OsStr::new("/ignored")),
                None,
                Path::new("/fallback"),
            ),
            PathBuf::from("/override")
        );
    }

    #[test]
    fn probe_health_treats_a_stale_socket_as_stopped() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let runtime = std::env::temp_dir().join(format!(
            "prism-worker-stale-socket-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&runtime).unwrap();
        let socket = runtime.join("worker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        drop(listener);

        assert_eq!(probe_health_at(&socket).unwrap(), DaemonHealth::stopped());

        fs::remove_file(socket).unwrap();
        fs::remove_dir(runtime).unwrap();
    }

    #[test]
    fn waiting_for_a_live_socket_to_close_times_out() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let runtime = std::env::temp_dir().join(format!(
            "prism-worker-live-socket-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&runtime).unwrap();
        let socket = runtime.join("worker.sock");
        let _listener = UnixListener::bind(&socket).unwrap();

        assert_eq!(
            wait_for_socket_to_close(&socket, Duration::ZERO),
            Err("timed out waiting for Prism worker daemon to stop".to_string())
        );

        fs::remove_file(socket).unwrap();
        fs::remove_dir(runtime).unwrap();
    }

    #[test]
    fn waiting_for_a_draining_daemon_times_out() {
        assert_eq!(
            wait_for_existing_daemon(Duration::ZERO, || Ok(DaemonHealth {
                state: DaemonState::Draining,
                protocol_version: Some(PROTOCOL_VERSION),
                instance_id: Some("test".to_string()),
                pid: Some(std::process::id()),
                active: 2,
            })),
            Err(
                "timed out waiting for Prism worker daemon to finish draining (2 active)"
                    .to_string()
            )
        );
    }
}
