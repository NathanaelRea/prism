use crate::args::{
    self, AgentCommand, Args, CommandKind, ConfigCommand, DaemonCommand, DbCommand, DebugCommand,
    InspectOptions, StatusOptions, WorkerCommand,
};
use crate::config::Config;
use crate::observability::{self, LogLevel, ObserverOptions};
use crate::process::Command as ProcessCommand;
use crate::repo::Repository;
use crate::tui::ManagedRepo;
use crate::workspace_state::{
    ControlAction, ControlRequest, InspectRequest, Subject, WorkspaceContext, WorkspaceSnapshot,
    WorkspaceState,
};
use crate::{agent_session, config, session, setup, tui, ui_state, workspace};

pub async fn run() -> Result<(), String> {
    let args = Args::parse(std::env::args_os().skip(1))?;
    if let CommandKind::Debug(DebugCommand::Record(options)) = &args.command {
        let repo = load_integrity_repo_context(args.repo.as_deref()).await?;
        eprintln!(
            "capturing the previous {}s and next {}s from the running Prism TUI...",
            options.before_seconds, options.after_seconds
        );
        let path = crate::flight_recorder::trigger(&repo, *options)?;
        println!("{}", path.display());
        return Ok(());
    }
    if matches!(args.command, CommandKind::Debug(DebugCommand::Integrity)) {
        let repo = load_integrity_repo_context(args.repo.as_deref()).await?;
        return crate::storage::print_integrity(&observability::db_path(&repo))
            .map_err(|error| error.to_string());
    }
    if matches!(
        args.command,
        CommandKind::Help | CommandKind::Version | CommandKind::DebugHelp | CommandKind::DbHelp
    ) {
        return run_static_command(args.command);
    }

    observability::init(observer_options(&args));
    observability::install_panic_hook();
    observability::emit(observability::EventInput {
        level: LogLevel::Info,
        target: "startup",
        action: "parsed_args",
        operation_id: None,
        parent_operation_id: None,
        branch: None,
        session: None,
        message: "parsed command line arguments".to_string(),
        data_json: None,
    });

    let result = match args.command {
        CommandKind::Help | CommandKind::Version | CommandKind::DebugHelp | CommandKind::DbHelp => {
            run_static_command(args.command)
        }
        CommandKind::Config(command) => {
            let (repo, config) = load_single_repo_context(args.repo.as_deref()).await?;
            run_config_command(command, &repo, &config)
        }
        CommandKind::Doctor => {
            let (repo, mut config) = load_single_repo_context(args.repo.as_deref()).await?;
            config::doctor(&repo, &mut config).await
        }
        CommandKind::Agent(command) => {
            let (repo, mut config) = load_single_repo_context(args.repo.as_deref()).await?;
            config::ensure_default_agent_noninteractive(&mut config)?;
            crate::tmux::migrate_legacy_agent_sessions(&repo, &config).await?;
            run_agent_command(command, &repo, &config).await
        }
        CommandKind::Workflow(arguments) => {
            crate::application::workflow_cli::run_workflow(args.repo.as_deref(), &arguments).await
        }
        CommandKind::Skill(arguments) => {
            crate::application::workflow_cli::run_skill(args.repo.as_deref(), &arguments).await
        }
        CommandKind::Template(arguments) => {
            crate::application::workflow_cli::run_template(args.repo.as_deref(), &arguments).await
        }
        CommandKind::Debug(command) => {
            let (repo, mut config) = load_single_repo_context(args.repo.as_deref()).await?;
            run_debug_command(command, &repo, &mut config).await
        }
        CommandKind::Db(command) => {
            let repo = load_db_repo_context(args.repo.as_deref()).await?;
            run_db_command(command, &repo).await
        }
        CommandKind::Worker(command) => run_worker_command(command).await,
        CommandKind::List(options) => run_list_command(args.repo.as_deref(), options).await,
        CommandKind::Status(options) => run_status_command(args.repo.as_deref(), options).await,
        CommandKind::Pause(selector) => {
            run_control_command(args.repo.as_deref(), ControlAction::Pause, selector).await
        }
        CommandKind::Resume(selector) => {
            run_control_command(args.repo.as_deref(), ControlAction::Resume, selector).await
        }
        CommandKind::Stop(selector) => {
            run_control_command(args.repo.as_deref(), ControlAction::Stop, selector).await
        }
        CommandKind::Recover(selector) => run_recover_command(args.repo.as_deref(), selector).await,
        CommandKind::Daemon(command) => run_daemon_command(command),
        CommandKind::Tui => run_tui(args.repo.as_deref()).await,
    };
    match &result {
        Ok(()) => observability::finish_process_runs("ok", None),
        Err(error) => {
            emit_fatal_error(error);
            observability::finish_process_runs("error", Some(error));
        }
    }
    result
}

fn run_config_command(
    command: ConfigCommand,
    repo: &Repository,
    config: &Config,
) -> Result<(), String> {
    match command {
        ConfigCommand::Show => config::print_config(repo, config),
        ConfigCommand::Example => print!("{}", config::config_example()),
        ConfigCommand::Schema => print!("{}", config::CONFIG_SCHEMA_JSON),
        ConfigCommand::Paths => {
            println!("user_config = {}", config.user_path.display());
            println!("repo_config = {}", config.repo_config_path.display());
            println!("schema_url = {}", config::CONFIG_SCHEMA_URL);
        }
    }
    Ok(())
}

async fn run_agent_command(
    command: AgentCommand,
    repo: &Repository,
    config: &Config,
) -> Result<(), String> {
    match command {
        AgentCommand::Ensure { branch } => {
            session::reconcile_worktree_state(repo, config).await?;
            let mut matches = session::discover_sessions(repo, config)
                .await?
                .into_iter()
                .filter(|session| session.branch == branch);
            let selected = matches
                .next()
                .ok_or_else(|| format!("no worktree session found for branch '{branch}'"))?;
            if matches.next().is_some() {
                return Err(format!(
                    "multiple worktree sessions found for branch '{branch}'"
                ));
            }
            let ensured = agent_session::ensure_latest_session(repo, config, &selected).await?;
            if !ensured.running {
                return Err(format!(
                    "agent session for branch '{branch}' did not become ready"
                ));
            }
            let runtime = ensured.opencode_runtime;
            let harness = config.selected_harness()?.describe();

            println!("branch = {}", selected.branch);
            println!("worktree = {}", selected.path.display());
            println!("harness_id = {}", harness.id);
            println!("adapter_id = {}", harness.adapter);
            println!("generation = {}", ensured.generation);
            println!("tmux_session = {}", ensured.tmux_session);
            println!("running = true");
            println!(
                "session_endpoint = {}",
                runtime
                    .as_ref()
                    .map(|runtime| runtime.server_url.as_str())
                    .unwrap_or("")
            );
            println!(
                "runtime_process_id = {}",
                runtime
                    .as_ref()
                    .and_then(|runtime| runtime.server_pid)
                    .map(|pid| pid.to_string())
                    .unwrap_or_default()
            );
            println!(
                "session_id = {}",
                runtime
                    .as_ref()
                    .and_then(|runtime| runtime.opencode_session_id.as_deref())
                    .unwrap_or("")
            );
            Ok(())
        }
    }
}

pub fn emit_fatal_error(error: &str) {
    observability::emit(observability::EventInput {
        level: LogLevel::Error,
        target: "process",
        action: "fatal",
        operation_id: None,
        parent_operation_id: None,
        branch: None,
        session: None,
        message: error.to_string(),
        data_json: None,
    });
}

fn run_static_command(command: CommandKind) -> Result<(), String> {
    match command {
        CommandKind::Help => {
            println!("{}", args::help_text());
            Ok(())
        }
        CommandKind::Version => {
            println!("prism {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        CommandKind::DebugHelp => {
            println!("{}", args::debug_help_text());
            Ok(())
        }
        CommandKind::DbHelp => {
            println!("{}", args::db_help_text());
            Ok(())
        }
        _ => unreachable!("static command runner received a stateful command"),
    }
}

async fn workspace_state(repo: Option<&std::path::Path>) -> Result<WorkspaceState, String> {
    WorkspaceState::open(WorkspaceContext {
        repo: repo.map(std::path::Path::to_path_buf),
        cwd: std::env::current_dir().map_err(|error| format!("current directory: {error}"))?,
    })
    .await
}

async fn run_list_command(
    repo: Option<&std::path::Path>,
    options: InspectOptions,
) -> Result<(), String> {
    let snapshot = workspace_state(repo)
        .await?
        .inspect(InspectRequest {
            include_hidden: options.all,
            include_terminal: options.all,
        })
        .await?;
    print_snapshot_warnings(&snapshot);
    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&snapshot)
                .map_err(|error| format!("serialize workspace status: {error}"))?
        );
    } else {
        print_workspace_table(&snapshot);
    }
    Ok(())
}

async fn run_status_command(
    repo: Option<&std::path::Path>,
    options: StatusOptions,
) -> Result<(), String> {
    let state = workspace_state(repo).await?;
    let snapshot = state
        .inspect(InspectRequest {
            include_hidden: true,
            include_terminal: true,
        })
        .await?;
    let subject = state.resolve_subject(&snapshot, options.selector.as_deref())?;
    print_snapshot_warnings(&snapshot);
    if options.json {
        let value = status_json(&snapshot, &subject);
        println!(
            "{}",
            serde_json::to_string_pretty(&value)
                .map_err(|error| format!("serialize status: {error}"))?
        );
    } else {
        print_subject(&snapshot, &subject);
    }
    Ok(())
}

async fn run_control_command(
    repo: Option<&std::path::Path>,
    action: ControlAction,
    selector: Option<String>,
) -> Result<(), String> {
    let receipt = workspace_state(repo)
        .await?
        .control(ControlRequest { action, selector })
        .await?;
    println!(
        "workflow = {}\nstate = {}",
        receipt.workflow.display_id, receipt.state
    );
    for warning in receipt.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

async fn run_recover_command(
    repo: Option<&std::path::Path>,
    selector: Option<String>,
) -> Result<(), String> {
    if selector.is_some() {
        return run_control_command(repo, ControlAction::Recover, selector).await;
    }
    let snapshot = workspace_state(repo)
        .await?
        .inspect(InspectRequest {
            include_hidden: true,
            include_terminal: true,
        })
        .await?;
    print_snapshot_warnings(&snapshot);
    let candidates = snapshot
        .repositories
        .iter()
        .flat_map(|repo| &repo.workflows)
        .filter(|workflow| workflow.available_controls.recover)
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        println!("No recovery-pending workflows.");
    } else {
        println!("WORKFLOW     REPO       WORKTREE             LAST HEARTBEAT");
        for workflow in candidates {
            println!(
                "{:<12} {:<10} {:<20} {}",
                workflow.identity.display_id,
                workspace::label_for_root(&workflow.identity.repository),
                workflow.worktree.display,
                workflow
                    .dispatch
                    .heartbeat_unix_ms
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_string())
            );
        }
    }
    Ok(())
}

fn run_daemon_command(command: DaemonCommand) -> Result<(), String> {
    match command {
        DaemonCommand::Status { json } => {
            let health = crate::worker::probe_health()?;
            if json {
                let observed_unix_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(i64::MAX as u128) as i64;
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "schema_version": 1, "observed_unix_ms": observed_unix_ms, "daemon": health, "warnings": [] })).map_err(|error| error.to_string())?);
            } else {
                println!("state = {}", daemon_state_label(&health.state));
                if let Some(instance) = health.instance_id {
                    println!("instance_id = {instance}");
                }
                if let Some(pid) = health.pid {
                    println!("pid = {pid}");
                }
                println!("active = {}", health.active);
            }
            Ok(())
        }
        DaemonCommand::Start => {
            crate::worker::ensure_running()?;
            let health = crate::worker::probe_health()?;
            println!(
                "state = {}\ninstance_id = {}\npid = {}\nactive = {}",
                daemon_state_label(&health.state),
                health.instance_id.unwrap_or_default(),
                health.pid.map(|pid| pid.to_string()).unwrap_or_default(),
                health.active
            );
            Ok(())
        }
        DaemonCommand::Stop => {
            let health = crate::worker::probe_health()?;
            if matches!(health.state, crate::worker::DaemonState::Stopped) {
                println!("state = stopped");
                return Ok(());
            }
            crate::worker::shutdown()?;
            println!("state = stopped");
            Ok(())
        }
    }
}

fn print_workspace_table(snapshot: &WorkspaceSnapshot) {
    println!(
        "REPO       WORKTREE             GIT                 AGENT          WORKFLOW     STATE              STEP                 CI"
    );
    for repo in &snapshot.repositories {
        let mut rendered_workflows = std::collections::BTreeSet::new();
        for worktree in &repo.worktrees {
            let workflows = repo
                .workflows
                .iter()
                .filter(|workflow| workflow.worktree.path == worktree.identity.path)
                .collect::<Vec<_>>();
            if workflows.is_empty() {
                println!(
                    "{:<10} {:<20} {:<19} {:<14} {:<12} {:<18} {:<20} {}",
                    repo.label,
                    worktree.identity.display,
                    worktree.git.label(),
                    agent_label(worktree),
                    "-",
                    "-",
                    "-",
                    ci_label(worktree)
                );
            } else {
                for workflow in workflows {
                    rendered_workflows.insert(workflow.identity.run_id.as_str());
                    let state = if workflow.dispatch.state.as_deref() == Some("recovery_pending") {
                        "recovery_pending"
                    } else if workflow.pause_requested
                        && workflow.lifecycle != crate::workspace_state::WorkflowLifecycle::Paused
                    {
                        "pause_requested"
                    } else {
                        workflow.lifecycle.label()
                    };
                    let step = workflow
                        .current_step
                        .as_ref()
                        .map(|step| {
                            format!(
                                "{} {}/{}",
                                step.label, workflow.progress.completed, workflow.progress.total
                            )
                        })
                        .unwrap_or_else(|| "-".to_string());
                    println!(
                        "{:<10} {:<20} {:<19} {:<14} {:<12} {:<18} {:<20} {}",
                        repo.label,
                        worktree.identity.display,
                        worktree.git.label(),
                        agent_label(worktree),
                        workflow.identity.display_id,
                        state,
                        step,
                        ci_label(worktree)
                    );
                }
            }
        }
        for workflow in &repo.workflows {
            if rendered_workflows.contains(workflow.identity.run_id.as_str()) {
                continue;
            }
            let state = if workflow.dispatch.state.as_deref() == Some("recovery_pending") {
                "recovery_pending"
            } else if workflow.pause_requested
                && workflow.lifecycle != crate::workspace_state::WorkflowLifecycle::Paused
            {
                "pause_requested"
            } else {
                workflow.lifecycle.label()
            };
            let step = workflow
                .current_step
                .as_ref()
                .map(|step| {
                    format!(
                        "{} {}/{}",
                        step.label, workflow.progress.completed, workflow.progress.total
                    )
                })
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{:<10} {:<20} {:<19} {:<14} {:<12} {:<18} {:<20} -",
                repo.label,
                workflow.worktree.display,
                "unavailable",
                "unknown",
                workflow.identity.display_id,
                state,
                step
            );
        }
    }
}

fn print_subject(snapshot: &WorkspaceSnapshot, subject: &Subject) {
    match *subject {
        Subject::Repository(repo) => {
            let repo = &snapshot.repositories[repo];
            println!(
                "repository = {}\npath = {}\nworktrees = {}\nworkflows = {}\nattention = {}",
                repo.label,
                repo.root.display(),
                repo.totals.worktrees,
                repo.totals.workflows,
                repo.totals.attention
            );
            for worktree in &repo.worktrees {
                println!(
                    "worktree = {} ({})",
                    worktree.identity.path.display(),
                    branch_label(&worktree.branch)
                );
            }
        }
        Subject::Worktree(repo, worktree) => {
            let repo = &snapshot.repositories[repo];
            let worktree = &repo.worktrees[worktree];
            println!(
                "repository = {}\nworktree = {}\nbranch = {}\ngit = {}\nagent = {}",
                repo.label,
                worktree.identity.path.display(),
                branch_label(&worktree.branch),
                worktree.git.label(),
                worktree
                    .agent
                    .state
                    .map(crate::agent::AgentState::label)
                    .unwrap_or("unknown")
            );
            for workflow in &worktree.workflows {
                println!("workflow = {}", workflow.display_id);
            }
            if let Some(pull_request) = &worktree.pull_request {
                println!(
                    "pull_request = {}\nci = {}\nmergeability = {}\nobservation_age_ms = {}\nobservation_stale = {}\nobservation_provenance = sqlite_cache",
                    pull_request.number,
                    pull_request
                        .ci
                        .map(|state| state.label())
                        .unwrap_or("unknown"),
                    pull_request
                        .mergeability
                        .map(mergeability_label)
                        .unwrap_or("unknown"),
                    pull_request.age_ms,
                    pull_request.stale,
                );
                if let Some(error) = &pull_request.error {
                    println!("observation_error = {error}");
                }
            }
        }
        Subject::Workflow(repo, workflow) => {
            let workflow = &snapshot.repositories[repo].workflows[workflow];
            println!(
                "workflow = {}\ncanonical_id = {}:{}\nrepository = {}\nworktree = {}\nlifecycle = {}\ndispatch = {}\npause_requested = {}\nprogress = {}/{}",
                workflow.identity.display_id,
                workflow.identity.repository.display(),
                workflow.identity.run_id,
                workflow.identity.repository.display(),
                workflow.worktree.path.display(),
                workflow.lifecycle.label(),
                workflow.dispatch.state.as_deref().unwrap_or("unknown"),
                workflow.pause_requested,
                workflow.progress.completed,
                workflow.progress.total
            );
            if let Some(step) = &workflow.current_step {
                println!("step = {} ({})", step.label, step.state.label());
            }
            println!(
                "controls = pause:{} resume:{} stop:{} recover:{}",
                workflow.available_controls.pause,
                workflow.available_controls.resume,
                workflow.available_controls.stop,
                workflow.available_controls.recover
            );
        }
    }
}

fn status_json(snapshot: &WorkspaceSnapshot, subject: &Subject) -> serde_json::Value {
    let subject = match *subject {
        Subject::Repository(repo) => {
            serde_json::to_value(&snapshot.repositories[repo]).unwrap_or(serde_json::Value::Null)
        }
        Subject::Worktree(repo, worktree) => {
            serde_json::to_value(&snapshot.repositories[repo].worktrees[worktree])
                .unwrap_or(serde_json::Value::Null)
        }
        Subject::Workflow(repo, workflow) => {
            serde_json::to_value(&snapshot.repositories[repo].workflows[workflow])
                .unwrap_or(serde_json::Value::Null)
        }
    };
    serde_json::json!({ "schema_version": 1, "observed_unix_ms": snapshot.observed_unix_ms, "daemon": snapshot.daemon, "subject": subject, "warnings": snapshot.warnings })
}

fn print_snapshot_warnings(snapshot: &WorkspaceSnapshot) {
    for warning in &snapshot.warnings {
        eprintln!("warning: {}: {}", warning.scope, warning.message);
    }
}

fn branch_label(branch: &crate::workspace_state::BranchState) -> &str {
    match branch {
        crate::workspace_state::BranchState::Named(name) => name,
        crate::workspace_state::BranchState::Detached => "(detached)",
    }
}

fn ci_label(worktree: &crate::workspace_state::WorktreeSnapshot) -> String {
    worktree
        .pull_request
        .as_ref()
        .and_then(|pr| {
            pr.ci
                .as_ref()
                .map(|ci| format!("{} {}s cache", ci.label(), pr.age_ms / 1_000))
        })
        .unwrap_or_else(|| "-".to_string())
}

fn agent_label(worktree: &crate::workspace_state::WorktreeSnapshot) -> &'static str {
    worktree
        .agent
        .state
        .map(crate::agent::AgentState::label)
        .unwrap_or("unknown")
}

fn mergeability_label(state: crate::workspace_state::MergeabilityState) -> &'static str {
    match state {
        crate::workspace_state::MergeabilityState::Clean => "clean",
        crate::workspace_state::MergeabilityState::Dirty => "dirty",
        crate::workspace_state::MergeabilityState::Blocked => "blocked",
        crate::workspace_state::MergeabilityState::Behind => "behind",
        crate::workspace_state::MergeabilityState::Unstable => "unstable",
        crate::workspace_state::MergeabilityState::HasHooks => "has_hooks",
        crate::workspace_state::MergeabilityState::Unknown => "unknown",
    }
}

fn daemon_state_label(state: &crate::worker::DaemonState) -> &'static str {
    match state {
        crate::worker::DaemonState::Running => "running",
        crate::worker::DaemonState::Draining => "draining",
        crate::worker::DaemonState::Stopped => "stopped",
    }
}

async fn load_single_repo_context(
    repo_arg: Option<&std::path::Path>,
) -> Result<(Repository, Config), String> {
    let repo =
        observability::phase_async("discover_repo", || Repository::discover(repo_arg)).await?;
    observability::attach_repo(&repo)?;
    let config = observability::phase("load_config", || Ok(Config::load(&repo)))?;
    warn_pending_recovery(&repo).await;
    Ok((repo, config))
}

async fn warn_pending_recovery(repo: &Repository) {
    let count = match workspace_state(Some(&repo.root)).await {
        Ok(state) => {
            state
                .inspect(InspectRequest {
                    include_hidden: true,
                    include_terminal: true,
                })
                .await
        }
        Err(error) => Err(error),
    }
    .map(|snapshot| {
        snapshot
            .repositories
            .iter()
            .flat_map(|repo| &repo.workflows)
            .filter(|workflow| workflow.dispatch.state.as_deref() == Some("recovery_pending"))
            .count()
    });
    if let Ok(count) = count
        && count > 0
    {
        eprintln!(
            "Prism has {count} interrupted managed run(s); open interactive Prism to choose which to resume"
        );
    }
}

async fn run_worker_command(command: WorkerCommand) -> Result<(), String> {
    match command {
        WorkerCommand::Serve => crate::worker::serve().await,
        WorkerCommand::Ensure => crate::worker::ensure_running(),
        WorkerCommand::Health => {
            println!("{}", crate::worker::health_response()?);
            Ok(())
        }
        WorkerCommand::Shutdown => crate::worker::shutdown(),
    }
}

async fn load_db_repo_context(repo_arg: Option<&std::path::Path>) -> Result<Repository, String> {
    if repo_arg.is_some() {
        let (repo, _) = load_single_repo_context(repo_arg).await?;
        return Ok(repo);
    }
    match Repository::discover(None).await {
        Ok(repo) => {
            observability::attach_repo(&repo)?;
            Ok(repo)
        }
        Err(discover_error) => {
            let entries = workspace::discover_valid_entries(workspace::load_entries()?).await;
            let Some(entry) = entries.into_iter().next() else {
                return Err(discover_error);
            };
            observability::attach_repo(&entry.repo)?;
            Ok(entry.repo)
        }
    }
}

async fn load_integrity_repo_context(
    repo_arg: Option<&std::path::Path>,
) -> Result<Repository, String> {
    if repo_arg.is_some() {
        return Repository::discover(repo_arg).await;
    }
    match Repository::discover(None).await {
        Ok(repo) => Ok(repo),
        Err(discover_error) => workspace::discover_valid_entries(workspace::load_entries()?)
            .await
            .into_iter()
            .next()
            .map(|entry| entry.repo)
            .ok_or(discover_error),
    }
}

fn observer_options(args: &Args) -> ObserverOptions {
    let log_level = args.log_level.unwrap_or(if args.debug {
        LogLevel::Debug
    } else {
        LogLevel::Info
    });
    ObserverOptions {
        log_level,
        print_logs: args.print_logs || args.debug,
    }
}

async fn run_tui(repo_arg: Option<&std::path::Path>) -> Result<(), String> {
    let (entries, selected_repo) = observability::phase_async("load_workspace", || {
        workspace::ensure_entries_for_tui(repo_arg)
    })
    .await?;
    let (entries, selected_repo) = observability::phase("reconcile_workspace", || {
        workspace::remove_missing_entries(entries, selected_repo)
    })?;
    let mut repos = Vec::new();
    let discovered_entries = workspace::discover_valid_entries(entries).await;
    let selected_repo = discovered_entries
        .iter()
        .position(|entry| entry.source_index == selected_repo)
        .unwrap_or_else(|| selected_repo.min(discovered_entries.len().saturating_sub(1)));
    for (index, entry) in discovered_entries.iter().enumerate() {
        if index == selected_repo {
            observability::attach_repo(&entry.repo)?;
        } else {
            observability::attach_run_repo(&entry.repo)?;
        }
    }
    for entry in discovered_entries {
        let repo = entry.repo;
        let mut config = Config::load(&repo);
        observability::phase("standard_pack_bootstrap", || {
            setup::ensure_user_owned_resources(&config)
        })?;
        let worktrunk_version = observability::phase_async("ensure_tools", || {
            config::ensure_required_tools(&repo, &config)
        })
        .await?;
        crate::flight_recorder::record(
            "startup",
            "worktrunk_version",
            None,
            vec![crate::flight_recorder::text(
                "version",
                &worktrunk_version.raw,
            )],
        );
        if observability::phase("initial_harness_setup", || {
            setup::maybe_prompt_harness(&config)
        })?
        .is_some()
        {
            config = Config::load(&repo);
        }
        observability::phase("ensure_default_agent", || {
            config::ensure_default_agent(&mut config)
        })?;
        repos.push(ManagedRepo::new(repo, config, entry.key));
    }
    observability::phase("ensure_generic_worker", crate::worker::ensure_running)?;
    if let Some(repo) = repos.get(selected_repo)
        && setup::maybe_prompt_icon_style(&repo.config)?.is_some()
    {
        for repo in &mut repos {
            repo.config = Config::load(&repo.repo);
        }
    }
    let selected_repo = selected_repo.min(repos.len().saturating_sub(1));
    if let Some(repo) = repos.get(selected_repo) {
        observability::phase_async("startup_setup_prompt", || {
            setup::maybe_prompt_startup_setup(&repo.repo, &repo.config)
        })
        .await?;
    }
    observability::phase_async("migrate_tmux_session_names", || async {
        for managed in &repos {
            crate::tmux::migrate_legacy_agent_sessions(&managed.repo, &managed.config).await?;
        }
        Ok(())
    })
    .await?;
    observability::phase_async("reconcile_worktrees", || async {
        for managed in &repos {
            session::reconcile_worktree_state(&managed.repo, &managed.config).await?;
            crate::tui::maintain_workflow_storage(&managed.repo)?;
        }
        Ok(())
    })
    .await?;
    let sessions =
        observability::phase_async("discover_sessions", || discover_workspace_sessions(&repos))
            .await?;
    let mut tui = observability::phase("initialize_tui", || {
        Ok(tui::Tui::new(repos, selected_repo, sessions))
    })?;
    tui.use_persisted_ui_state(ui_state::path())?;
    tui.select_repo(selected_repo);
    observability::phase_async("run_tui", || tui.run()).await
}

async fn discover_workspace_sessions(
    repos: &[ManagedRepo],
) -> Result<Vec<session::Session>, String> {
    let mut all = Vec::new();
    for (index, managed) in repos.iter().enumerate() {
        let mut sessions = session::discover_sessions(&managed.repo, &managed.config).await?;
        for session in &mut sessions {
            session.repo_index = index;
            session.repo_label = managed.label.clone();
            session.repo_key = managed.key;
        }
        all.extend(sessions);
    }
    Ok(all)
}

async fn run_debug_command(
    command: DebugCommand,
    repo: &Repository,
    config: &mut Config,
) -> Result<(), String> {
    match command {
        DebugCommand::Paths => {
            println!("repo_root = {}", repo.root.display());
            println!("prism_dir = {}", repo.prism_dir().display());
            println!("db_path = {}", observability::db_path(repo).display());
            println!(
                "runtime_log_path = {}",
                observability::runtime_log_path(repo).display()
            );
            println!("user_config = {}", config.user_path.display());
            println!("repo_config = {}", config.repo_config_path.display());
            println!("logs_dir = {}", repo.prism_dir().join("logs").display());
            println!(
                "recordings_dir = {}",
                repo.prism_dir().join("recordings").display()
            );
            println!(
                "flight_recorder_socket = {}",
                crate::flight_recorder::control_socket_path(repo).display()
            );
            println!(
                "worker_runtime_dir = {}",
                crate::worker::runtime_dir().display()
            );
            println!(
                "worker_socket_path = {}",
                crate::worker::socket_path().display()
            );
            println!(
                "worker_lock_path = {}",
                crate::worker::runtime_dir().join("worker.lock").display()
            );
            Ok(())
        }
        DebugCommand::Info => {
            println!("version = {}", env!("CARGO_PKG_VERSION"));
            println!("repo_root = {}", repo.root.display());
            println!("prism_dir = {}", repo.prism_dir().display());
            println!(
                "default_base = {}",
                config.default_base.as_deref().unwrap_or("")
            );
            let harness = config.selected_harness()?;
            let description = harness.describe();
            println!("default_harness = {}", description.id);
            println!("default_adapter = {}", description.adapter);
            println!(
                "default_harness_command = {}",
                observability::sanitize_command_text(
                    &harness
                        .interactive_argv(None, None, None, &repo.root)?
                        .argv
                        .join(" ")
                )
            );
            println!("worktree_command = {}", config.worktree_command);
            println!("review_packet_dir = {}", config.review_packet_dir);
            println!("escape_key = {}", config.escape_key.label());
            println!("tools:");
            for (key, value) in &config.tools {
                println!("  {key} = {value}");
            }
            match setup::inspect_startup_setup(repo, config).await {
                Ok(setup) => {
                    println!("startup_setup_needs_prompt = {}", setup.needs_prompt);
                    println!(
                        "startup_current_branch = {}",
                        setup.current_branch.as_deref().unwrap_or("")
                    );
                    println!(
                        "startup_default_base = {}",
                        setup.default_base.as_deref().unwrap_or("")
                    );
                    println!("startup_no_extra_worktrees = {}", setup.no_extra_worktrees);
                    println!("startup_can_move_branch = {}", setup.can_move_branch);
                }
                Err(error) => println!("startup_setup_error = {error}"),
            }
            match crate::storage::passive_checkpoint_status(&observability::db_path(repo)) {
                Ok(status) => {
                    println!("database_main_bytes = {}", status.main_bytes);
                    println!("database_wal_bytes = {}", status.wal_bytes);
                    println!("database_shm_bytes = {}", status.shm_bytes);
                    println!("wal_checkpoint_passive_busy = {}", status.checkpoint_busy);
                    println!(
                        "wal_checkpoint_passive_log_frames = {}",
                        status.checkpoint_log_frames
                    );
                    println!(
                        "wal_checkpoint_passive_checkpointed_frames = {}",
                        status.checkpointed_frames
                    );
                }
                Err(error) => println!("wal_checkpoint_passive_error = {error}"),
            }
            println!(
                "workflow_database = {}",
                crate::PromptWorkflowService::database_path().display()
            );
            match crate::worker::probe_health() {
                Ok(health) => println!(
                    "workflow_worker = {:?} active={}",
                    health.state, health.active
                ),
                Err(error) => println!("workflow_worker_error = {error}"),
            }
            Ok(())
        }
        DebugCommand::Logs => {
            for line in observability::tail_runtime_log(repo, 200)? {
                println!("{line}");
            }
            Ok(())
        }
        DebugCommand::Startup => run_debug_startup(repo, config).await,
        DebugCommand::Integrity => {
            unreachable!("integrity runs before observability initialization")
        }
        DebugCommand::Record(_) => {
            unreachable!("record runs before observability initialization")
        }
    }
}

async fn run_debug_startup(repo: &Repository, config: &mut Config) -> Result<(), String> {
    let result: Result<(), String> = async {
        let worktrunk_version = observability::phase_async("ensure_tools", || {
            config::ensure_required_tools(repo, config)
        })
        .await?;
        println!("worktrunk_version = {}", worktrunk_version.raw);
        observability::phase("ensure_default_agent", || {
            config::ensure_default_agent_noninteractive(config)
        })?;
        let setup = observability::phase_async("startup_setup_check", || {
            setup::inspect_startup_setup(repo, config)
        })
        .await?;
        println!("startup_setup_needs_prompt = {}", setup.needs_prompt);
        println!(
            "startup_current_branch = {}",
            setup.current_branch.as_deref().unwrap_or("")
        );
        println!(
            "startup_default_base = {}",
            setup.default_base.as_deref().unwrap_or("")
        );
        println!("startup_no_extra_worktrees = {}", setup.no_extra_worktrees);
        println!("startup_can_move_branch = {}", setup.can_move_branch);
        let sessions = observability::phase_async("discover_sessions", || {
            session::discover_sessions(repo, config)
        })
        .await?;
        println!("sessions = {}", sessions.len());
        Ok(())
    }
    .await;
    print_startup_phases();
    result
}

fn print_startup_phases() {
    println!("phases:");
    for phase in observability::startup_phases() {
        let elapsed = phase
            .elapsed_ms
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        let error = phase.error.unwrap_or_default();
        if error.is_empty() {
            println!("  {}\t{}\t{}ms", phase.phase, phase.status, elapsed);
        } else {
            println!(
                "  {}\t{}\t{}ms\t{}",
                phase.phase, phase.status, elapsed, error
            );
        }
    }
}

async fn run_db_command(command: DbCommand, repo: &Repository) -> Result<(), String> {
    match command {
        DbCommand::Shell => open_interactive_db(repo).await,
        DbCommand::Path => {
            println!("{}", observability::db_path(repo).display());
            Ok(())
        }
        DbCommand::Query(query) if query.trim().is_empty() => {
            Err("database query must not be empty".to_string())
        }
        DbCommand::Query(query) => observability::run_readonly_query(repo, &query),
    }
}

async fn open_interactive_db(repo: &Repository) -> Result<(), String> {
    observability::with_writable_db(repo, |_| Ok(()))?;
    if !crate::process::command_exists("sqlite3") {
        return Err("sqlite3 not found; install sqlite3".to_string());
    }

    let path = observability::db_path(repo);
    let command = ProcessCommand::new("sqlite3")
        .args(["-cmd", ".timeout 5000"])
        .args(["-cmd", "PRAGMA foreign_keys=ON;"])
        .args(["-cmd", "PRAGMA synchronous=FULL;"])
        .arg(&path);
    crate::process::run_status_inherited(command).await
}
