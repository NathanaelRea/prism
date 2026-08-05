use crate::args::{
    self, AgentCommand, Args, AutoCommand, AutoCommandSource, CommandKind, ConfigCommand,
    DaemonCommand, DbCommand, DebugCommand, InspectOptions, StatusOptions, WorkerCommand,
};
use crate::auto_flow::{
    AutoFlowStore, AutoImplementationSource, AutoLaunch, AutoLaunchOptions, AutoRunMode,
    load_recent_active_runs_for_repo, prepare_auto_run_for_resume,
};
use crate::config::Config;
use crate::git::{current_branch_name, selected_dirty};
use crate::observability::{self, LogLevel, ObserverOptions};
use crate::plan_run::PlanRunMode;
use crate::repo::Repository;
use crate::tui::ManagedRepo;
use crate::workspace_state::{
    ControlAction, ControlRequest, InspectRequest, Subject, WorkspaceContext, WorkspaceSnapshot,
    WorkspaceState,
};
use crate::{agent_session, config, plan, session, setup, tui, ui_state, workspace};
use std::process::Command as ProcessCommand;

pub fn run() -> Result<(), String> {
    let args = Args::parse(std::env::args_os().skip(1))?;
    if let CommandKind::Debug(DebugCommand::Record(options)) = &args.command {
        let repo = load_integrity_repo_context(args.repo.as_deref())?;
        eprintln!(
            "capturing the previous {}s and next {}s from the running Prism TUI...",
            options.before_seconds, options.after_seconds
        );
        let path = crate::flight_recorder::trigger(&repo, *options)?;
        println!("{}", path.display());
        return Ok(());
    }
    if matches!(args.command, CommandKind::Debug(DebugCommand::Integrity)) {
        let repo = load_integrity_repo_context(args.repo.as_deref())?;
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
            let (repo, config) = load_single_repo_context(args.repo.as_deref())?;
            run_config_command(command, &repo, &config)
        }
        CommandKind::Doctor => {
            let (repo, mut config) = load_single_repo_context(args.repo.as_deref())?;
            config::doctor(&repo, &mut config)
        }
        CommandKind::Agent(command) => {
            let (repo, mut config) = load_single_repo_context(args.repo.as_deref())?;
            config::ensure_default_agent_noninteractive(&mut config)?;
            crate::tmux::migrate_legacy_agent_sessions(&repo, &config)?;
            run_agent_command(command, &repo, &config)
        }
        CommandKind::RunPlan(path) => {
            let (repo, config) = load_single_repo_context(args.repo.as_deref())?;
            plan::run_plan_mode(&repo.root, &config, path.as_deref())
        }
        CommandKind::Auto(command) => {
            let (repo, config) = load_single_repo_context(args.repo.as_deref())?;
            run_auto_command(&repo, &config, command)
        }
        CommandKind::Debug(command) => {
            let (repo, mut config) = load_single_repo_context(args.repo.as_deref())?;
            run_debug_command(command, &repo, &mut config)
        }
        CommandKind::Db(command) => {
            let repo = load_db_repo_context(args.repo.as_deref())?;
            run_db_command(command, &repo)
        }
        CommandKind::Worker(command) => run_worker_command(command),
        CommandKind::List(options) => run_list_command(args.repo.as_deref(), options),
        CommandKind::Status(options) => run_status_command(args.repo.as_deref(), options),
        CommandKind::Pause(selector) => {
            run_control_command(args.repo.as_deref(), ControlAction::Pause, selector)
        }
        CommandKind::Resume(selector) => {
            run_control_command(args.repo.as_deref(), ControlAction::Resume, selector)
        }
        CommandKind::Stop(selector) => {
            run_control_command(args.repo.as_deref(), ControlAction::Stop, selector)
        }
        CommandKind::Recover(selector) => run_recover_command(args.repo.as_deref(), selector),
        CommandKind::Daemon(command) => run_daemon_command(command),
        CommandKind::Tui => run_tui(args.repo.as_deref()),
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
        ConfigCommand::Schema => print!("{}", config::GLOBAL_CONFIG_SCHEMA_JSON),
        ConfigCommand::Paths => {
            println!("user_config = {}", config.user_path.display());
            println!("repo_config = {}", config.repo_config_path.display());
            println!("global_schema_url = {}", config::GLOBAL_CONFIG_SCHEMA_URL);
            println!(
                "repository_schema_url = {}",
                config::REPOSITORY_CONFIG_SCHEMA_URL
            );
        }
        ConfigCommand::Migrate(mode) => {
            for line in config::migrate_config_files(config, mode)? {
                println!("{line}");
            }
        }
    }
    Ok(())
}

fn run_agent_command(
    command: AgentCommand,
    repo: &Repository,
    config: &Config,
) -> Result<(), String> {
    match command {
        AgentCommand::Ensure { branch } => {
            session::reconcile_worktree_state(repo, config)?;
            let mut matches = session::discover_sessions(repo, config)?
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
            let ensured = agent_session::ensure_latest_session(repo, config, &selected)?;
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

fn workspace_state(repo: Option<&std::path::Path>) -> Result<WorkspaceState, String> {
    WorkspaceState::open(WorkspaceContext {
        repo: repo.map(std::path::Path::to_path_buf),
        cwd: std::env::current_dir().map_err(|error| format!("current directory: {error}"))?,
    })
}

fn run_list_command(repo: Option<&std::path::Path>, options: InspectOptions) -> Result<(), String> {
    let snapshot = workspace_state(repo)?.inspect(InspectRequest {
        include_hidden: options.all,
        include_terminal: options.all,
    })?;
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

fn run_status_command(
    repo: Option<&std::path::Path>,
    options: StatusOptions,
) -> Result<(), String> {
    let state = workspace_state(repo)?;
    let snapshot = state.inspect(InspectRequest {
        include_hidden: true,
        include_terminal: true,
    })?;
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

fn run_control_command(
    repo: Option<&std::path::Path>,
    action: ControlAction,
    selector: Option<String>,
) -> Result<(), String> {
    let receipt = workspace_state(repo)?.control(ControlRequest { action, selector })?;
    println!(
        "workflow = {}\nstate = {}",
        receipt.workflow.display_id, receipt.state
    );
    for warning in receipt.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

fn run_recover_command(
    repo: Option<&std::path::Path>,
    selector: Option<String>,
) -> Result<(), String> {
    if selector.is_some() {
        return run_control_command(repo, ControlAction::Recover, selector);
    }
    let snapshot = workspace_state(repo)?.inspect(InspectRequest {
        include_hidden: true,
        include_terminal: true,
    })?;
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
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "schema_version": 1, "observed_unix_ms": crate::execution::now_ms(), "daemon": health, "warnings": [] })).map_err(|error| error.to_string())?);
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
                    rendered_workflows.insert((
                        workflow.identity.kind.as_str(),
                        workflow.identity.run_id.as_str(),
                    ));
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
            if rendered_workflows.contains(&(
                workflow.identity.kind.as_str(),
                workflow.identity.run_id.as_str(),
            )) {
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
                "workflow = {}\ncanonical_id = {}:{}:{}\nrepository = {}\nworktree = {}\nlifecycle = {}\ndispatch = {}\npause_requested = {}\nprogress = {}/{}",
                workflow.identity.display_id,
                workflow.identity.repository.display(),
                workflow.identity.kind,
                workflow.identity.run_id,
                workflow.identity.repository.display(),
                workflow.worktree.path.display(),
                workflow.lifecycle.label(),
                workflow.dispatch.state.as_deref().unwrap_or("unknown"),
                workflow.pause_requested,
                workflow.progress.completed,
                workflow.progress.total
            );
            if let Some(owner) = &workflow.owner {
                println!("owner = {}", owner.display_id);
            }
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

fn load_single_repo_context(
    repo_arg: Option<&std::path::Path>,
) -> Result<(Repository, Config), String> {
    let repo = observability::phase("discover_repo", || Repository::discover(repo_arg))?;
    observability::attach_repo(&repo)?;
    let config = observability::phase("load_config", || Ok(Config::load(&repo)))?;
    warn_pending_recovery(&repo);
    Ok((repo, config))
}

fn warn_pending_recovery(repo: &Repository) {
    let count = workspace_state(Some(&repo.root))
        .and_then(|state| {
            state.inspect(InspectRequest {
                include_hidden: true,
                include_terminal: true,
            })
        })
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

fn run_worker_command(command: WorkerCommand) -> Result<(), String> {
    match command {
        WorkerCommand::Serve => crate::worker::serve(),
        WorkerCommand::Ensure => crate::worker::ensure_running(),
        WorkerCommand::Health => {
            println!("{}", crate::worker::health_response()?);
            Ok(())
        }
        WorkerCommand::Shutdown => crate::worker::shutdown(),
    }
}

fn load_db_repo_context(repo_arg: Option<&std::path::Path>) -> Result<Repository, String> {
    if repo_arg.is_some() {
        let (repo, _) = load_single_repo_context(repo_arg)?;
        return Ok(repo);
    }
    match Repository::discover(None) {
        Ok(repo) => {
            observability::attach_repo(&repo)?;
            Ok(repo)
        }
        Err(discover_error) => {
            let entries = workspace::discover_valid_entries(workspace::load_entries()?);
            let Some(entry) = entries.into_iter().next() else {
                return Err(discover_error);
            };
            observability::attach_repo(&entry.repo)?;
            Ok(entry.repo)
        }
    }
}

fn load_integrity_repo_context(repo_arg: Option<&std::path::Path>) -> Result<Repository, String> {
    if repo_arg.is_some() {
        return Repository::discover(repo_arg);
    }
    match Repository::discover(None) {
        Ok(repo) => Ok(repo),
        Err(discover_error) => workspace::discover_valid_entries(workspace::load_entries()?)
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

fn run_tui(repo_arg: Option<&std::path::Path>) -> Result<(), String> {
    (|| {
        let (entries, selected_repo) = observability::phase("load_workspace", || {
            workspace::ensure_entries_for_tui(repo_arg)
        })?;
        let (entries, selected_repo) = observability::phase("reconcile_workspace", || {
            workspace::remove_missing_entries(entries, selected_repo)
        })?;
        let mut repos = Vec::new();
        let discovered_entries = workspace::discover_valid_entries(entries);
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
            let worktrunk_version = observability::phase("ensure_tools", || {
                config::ensure_required_tools(&repo, &config)
            })?;
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
        if let Some(repo) = repos.get(selected_repo)
            && setup::maybe_prompt_icon_style(&repo.config)?.is_some()
        {
            for repo in &mut repos {
                repo.config = Config::load(&repo.repo);
            }
        }
        let selected_repo = selected_repo.min(repos.len().saturating_sub(1));
        if let Some(repo) = repos.get(selected_repo) {
            observability::phase("startup_setup_prompt", || {
                setup::maybe_prompt_startup_setup(&repo.repo, &repo.config)
            })?;
        }
        observability::phase("migrate_tmux_session_names", || {
            for managed in &repos {
                crate::tmux::migrate_legacy_agent_sessions(&managed.repo, &managed.config)?;
            }
            Ok(())
        })?;
        observability::phase("reconcile_worktrees", || {
            for managed in &repos {
                session::reconcile_worktree_state(&managed.repo, &managed.config)?;
                crate::tui::maintain_workflow_storage(&managed.repo)?;
            }
            Ok(())
        })?;
        let sessions =
            observability::phase("discover_sessions", || discover_workspace_sessions(&repos))?;
        let mut tui = observability::phase("initialize_tui", || {
            Ok(tui::Tui::new(repos, selected_repo, sessions))
        })?;
        tui.use_persisted_ui_state(ui_state::path())?;
        tui.select_repo(selected_repo);
        observability::phase("run_tui", || tui.run())
    })()
}

fn run_auto_command(
    repo: &Repository,
    config: &Config,
    mut command: AutoCommand,
) -> Result<(), String> {
    workspace::ensure_repo_entry(&repo.root)?;
    let existing = observability::with_writable_db(repo, |path| {
        load_recent_active_runs_for_repo(&AutoFlowStore::open(path), &repo.root, 1)
    })?;
    if let Some(mut run) = existing.into_iter().next() {
        let workflow = crate::execution::WorkflowIdentity::new(
            crate::execution::WorkflowKind::Auto,
            &run.run.id,
        );
        let dispatch = observability::with_writable_db(repo, |path| {
            crate::execution::dispatch_state(path, &workflow)
        })?;
        if matches!(
            dispatch,
            Some(crate::execution::DispatchState::RecoveryPending)
        ) {
            return Err(format!(
                "Auto Flow run {} was interrupted; open Prism to choose whether to resume it",
                run.run.id
            ));
        }
        if matches!(
            dispatch,
            Some(
                crate::execution::DispatchState::Queued | crate::execution::DispatchState::Claimed
            )
        ) {
            println!(
                "auto_run_id = {}\nstatus = {:?}\nworktree = {}",
                run.run.id,
                run.run.status,
                run.run.worktree_path.display()
            );
            return Ok(());
        }
        let should_execute = observability::with_writable_db(repo, |path| {
            prepare_auto_run_for_resume(
                &AutoFlowStore::open(path),
                &mut run,
                crate::plan_run::DEFAULT_OUTPUT_LINES_PER_STEP,
            )
        })?;
        if should_execute {
            observability::with_writable_db(repo, |path| {
                crate::execution::enqueue(path, &workflow)
            })?;
            crate::worker::ensure_running()?;
            crate::worker::wake()?;
        }
        println!(
            "auto_run_id = {}\nstatus = {:?}\nworktree = {}",
            run.run.id,
            run.run.status,
            run.run.worktree_path.display()
        );
        return Ok(());
    }
    if !config.selected_harness()?.describe().headless {
        return Err(format!(
            "harness '{}' does not support managed Auto Flow execution; configure headless_command and headless_prompt_transport",
            config.default_harness
        ));
    }
    validate_auto_command_before_launch(repo, &mut command)?;
    let branch = current_branch_name(&repo.root, config)?
        .ok_or_else(|| "Auto Flow cannot start on detached HEAD".to_string())?;
    if config.is_default_branch(&branch) {
        return Err("Auto Flow cannot start on the default branch".to_string());
    }
    if selected_dirty(&repo.root, config)? {
        return Err("Auto Flow requires a clean worktree at launch".to_string());
    }
    let launch_options = auto_launch_options_for_command(repo, branch, command)?;
    let launch = AutoLaunch::with_options(&repo.root, &repo.root, launch_options)?.with_harness(
        config.default_harness.clone(),
        config.harness_adapter(&config.default_harness)?,
    );
    let run_id =
        crate::worker::launch_bundled_coding(crate::workflow::bundled::BundledCodingLaunch {
            repository: launch.repo_root.clone(),
            worktree_path: launch.worktree_path.clone(),
            task: launch.initial_prompt.clone(),
            plan_path: launch.plan_path.clone(),
            draft_plan: launch.implementation_source
                == crate::auto_flow::AutoImplementationSource::DraftPlan,
            harness_id: config.default_harness.clone(),
            variant: Some(launch.variant.clone()),
        })?;
    println!(
        "workflow_run_id = {run_id}\nstatus = queued\nworktree = {}",
        launch.worktree_path.display()
    );
    Ok(())
}

fn validate_auto_command_before_launch(
    repo: &Repository,
    command: &mut AutoCommand,
) -> Result<(), String> {
    if command.source != AutoCommandSource::ExistingPlan {
        return Ok(());
    }
    let plan_path = command
        .plan_path
        .as_deref()
        .ok_or_else(|| "auto run-plan requires a plan path".to_string())?;
    let plan_path = resolve_cli_plan_path(&repo.root, plan_path);
    let total = plan::infer_total_phases(&plan_path)?;
    if total == 0 {
        return Err("could not infer phases; add headings like 'Phase 1'".to_string());
    }
    command.plan_path = Some(plan_path);
    Ok(())
}

fn auto_launch_options_for_command(
    repo: &Repository,
    branch: String,
    command: AutoCommand,
) -> Result<AutoLaunchOptions, String> {
    match command.source {
        AutoCommandSource::Prompt => {
            let initial_prompt = command
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "prism auto requires an initial prompt for a new run".to_string())?;
            Ok(AutoLaunchOptions {
                branch,
                mode: AutoRunMode::Standard,
                implementation_source: AutoImplementationSource::Prompt,
                plan_path: None,
                plan_run_mode: PlanRunMode::Sequential,
                variant: "default".to_string(),
                agent_profile: None,
                initial_prompt: initial_prompt.to_string(),
            })
        }
        AutoCommandSource::ExistingPlan => {
            let plan_path = command
                .plan_path
                .ok_or_else(|| "auto run-plan requires a plan path".to_string())?;
            let plan_path = resolve_cli_plan_path(&repo.root, &plan_path);
            let total = plan::infer_total_phases(&plan_path)?;
            if total == 0 {
                return Err("could not infer phases; add headings like 'Phase 1'".to_string());
            }
            Ok(AutoLaunchOptions {
                branch,
                mode: AutoRunMode::Standard,
                implementation_source: AutoImplementationSource::ExistingPlan,
                plan_path: Some(plan_path.clone()),
                plan_run_mode: PlanRunMode::Sequential,
                variant: "plan".to_string(),
                agent_profile: None,
                initial_prompt: format!("Run plan phases from {}", plan_path.display()),
            })
        }
        AutoCommandSource::DraftPlan => {
            let initial_prompt = command
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    "prism auto plan requires a task prompt for a new run".to_string()
                })?;
            Ok(AutoLaunchOptions {
                branch,
                mode: AutoRunMode::PlanFirst,
                implementation_source: AutoImplementationSource::DraftPlan,
                plan_path: Some(repo.root.join("plan.md")),
                plan_run_mode: PlanRunMode::Sequential,
                variant: "draft-plan".to_string(),
                agent_profile: None,
                initial_prompt: initial_prompt.to_string(),
            })
        }
    }
}

fn resolve_cli_plan_path(cwd: &std::path::Path, path: &std::path::Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn discover_workspace_sessions(repos: &[ManagedRepo]) -> Result<Vec<session::Session>, String> {
    let mut all = Vec::new();
    for (index, managed) in repos.iter().enumerate() {
        let mut sessions = session::discover_sessions(&managed.repo, &managed.config)?;
        for session in &mut sessions {
            session.repo_index = index;
            session.repo_label = managed.label.clone();
            session.repo_key = managed.key;
        }
        all.extend(sessions);
    }
    Ok(all)
}

fn control_plane_debug_metrics() -> Result<Vec<crate::ControlPlaneMetric>, String> {
    crate::async_runtime::block_on(async {
        crate::WorkflowOperations::open_default()
            .await
            .map_err(|error| error.to_string())?
            .control_plane_metrics()
            .await
            .map_err(|error| error.to_string())
    })
    .map_err(|error| format!("run workflow debug operation: {error}"))?
}

fn run_debug_command(
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
            println!("plan_dir = {}", config.plan_dir);
            println!("review_packet_dir = {}", config.review_packet_dir);
            println!("escape_key = {}", config.escape_key.label());
            println!("tools:");
            for (key, value) in &config.tools {
                println!("  {key} = {value}");
            }
            match setup::inspect_startup_setup(repo, config) {
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
            match control_plane_debug_metrics() {
                Ok(metrics) => {
                    println!(
                        "workflow_database = {}",
                        crate::util::prism_config_dir()
                            .join("workflow.db")
                            .display()
                    );
                    for metric in metrics {
                        println!(
                            "control_plane.{} = {} time_unix_ms={} labels={}",
                            metric.name, metric.value, metric.time_unix_ms, metric.labels_json
                        );
                    }
                }
                Err(error) => println!("control_plane_error = {error}"),
            }
            Ok(())
        }
        DebugCommand::Logs => {
            for line in observability::tail_runtime_log(repo, 200)? {
                println!("{line}");
            }
            Ok(())
        }
        DebugCommand::Startup => run_debug_startup(repo, config),
        DebugCommand::Integrity => {
            unreachable!("integrity runs before observability initialization")
        }
        DebugCommand::Record(_) => {
            unreachable!("record runs before observability initialization")
        }
    }
}

fn run_debug_startup(repo: &Repository, config: &mut Config) -> Result<(), String> {
    let result: Result<(), String> = (|| {
        let worktrunk_version = observability::phase("ensure_tools", || {
            config::ensure_required_tools(repo, config)
        })?;
        println!("worktrunk_version = {}", worktrunk_version.raw);
        observability::phase("ensure_default_agent", || {
            config::ensure_default_agent_noninteractive(config)
        })?;
        let setup = observability::phase("startup_setup_check", || {
            setup::inspect_startup_setup(repo, config)
        })?;
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
        let sessions = observability::phase("discover_sessions", || {
            session::discover_sessions(repo, config)
        })?;
        println!("sessions = {}", sessions.len());
        Ok(())
    })();
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

fn run_db_command(command: DbCommand, repo: &Repository) -> Result<(), String> {
    match command {
        DbCommand::Shell => open_interactive_db(repo),
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

fn open_interactive_db(repo: &Repository) -> Result<(), String> {
    observability::with_writable_db(repo, |_| Ok(()))?;
    if !crate::process::command_exists("sqlite3") {
        return Err("sqlite3 not found; install sqlite3".to_string());
    }

    let path = observability::db_path(repo);
    crate::process::run_status_inherited(
        ProcessCommand::new("sqlite3")
            .args(["-cmd", ".timeout 5000"])
            .args(["-cmd", "PRAGMA foreign_keys=ON;"])
            .args(["-cmd", "PRAGMA synchronous=FULL;"])
            .arg(&path),
    )
}
