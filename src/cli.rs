use crate::args::{
    self, AgentCommand, Args, AutoCommand, AutoCommandSource, CommandKind, ConfigCommand,
    DbCommand, DebugCommand,
};
use crate::auto_flow::{
    AutoExecutorConfig, AutoImplementationSource, AutoLaunch, AutoLaunchOptions, AutoRunMode,
    execute_auto_initial_step, load_recent_active_runs_for_repo, prepare_auto_run_for_resume,
    save_auto_run,
};
use crate::config::Config;
use crate::git::{current_branch_name, selected_dirty};
use crate::observability::{self, LogLevel, ObserverOptions};
use crate::plan_run::PlanRunMode;
use crate::repo::Repository;
use crate::tui::ManagedRepo;
use crate::{agent_session, config, plan, session, setup, tui, ui_state, workspace};
use std::process::{Command as ProcessCommand, Stdio};

pub fn run() -> Result<(), String> {
    let args = Args::parse(std::env::args_os().skip(1))?;
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

    match args.command {
        CommandKind::Help | CommandKind::Version | CommandKind::DebugHelp | CommandKind::DbHelp => {
            run_static_command(args.command)
        }
        CommandKind::Config(command) => {
            let (repo, config) = load_single_repo_context(args.repo.as_deref())?;
            run_config_command(command, &repo, &config);
            Ok(())
        }
        CommandKind::Doctor => {
            let (repo, mut config) = load_single_repo_context(args.repo.as_deref())?;
            config::doctor(&repo, &mut config)
        }
        CommandKind::Agent(command) => {
            let (repo, mut config) = load_single_repo_context(args.repo.as_deref())?;
            config::ensure_default_agent_noninteractive(&mut config)?;
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
        CommandKind::Tui => run_tui(args.repo.as_deref()),
    }
}

fn run_config_command(command: ConfigCommand, repo: &Repository, config: &Config) {
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

fn load_single_repo_context(
    repo_arg: Option<&std::path::Path>,
) -> Result<(Repository, Config), String> {
    let repo = observability::phase("discover_repo", || Repository::discover(repo_arg))?;
    observability::attach_repo(&repo);
    let config = observability::phase("load_config", || Ok(Config::load(&repo)))?;
    Ok((repo, config))
}

fn load_db_repo_context(repo_arg: Option<&std::path::Path>) -> Result<Repository, String> {
    if repo_arg.is_some() {
        let (repo, _) = load_single_repo_context(repo_arg)?;
        return Ok(repo);
    }
    match Repository::discover(None) {
        Ok(repo) => {
            observability::attach_repo(&repo);
            Ok(repo)
        }
        Err(discover_error) => {
            let entries = workspace::discover_valid_entries(workspace::load_entries());
            let Some(entry) = entries.into_iter().next() else {
                return Err(discover_error);
            };
            observability::attach_repo(&entry.repo);
            Ok(entry.repo)
        }
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
    observability::start_startup_run(env!("CARGO_PKG_VERSION"));
    let result: Result<(), String> = (|| {
        let (entries, selected_repo) = observability::phase("load_workspace", || {
            workspace::ensure_entries_for_tui(repo_arg)
        })?;
        let (entries, selected_repo) = observability::phase("reconcile_workspace", || {
            Ok(workspace::remove_missing_entries(entries, selected_repo))
        })?;
        let mut repos = Vec::new();
        let discovered_entries = workspace::discover_valid_entries(entries);
        let selected_repo = discovered_entries
            .iter()
            .position(|entry| entry.source_index == selected_repo)
            .unwrap_or_else(|| selected_repo.min(discovered_entries.len().saturating_sub(1)));
        for entry in discovered_entries {
            let repo = entry.repo;
            let mut config = Config::load(&repo);
            observability::phase("ensure_tools", || config::ensure_required_tools(&config))?;
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
            observability::attach_repo(&repo.repo);
        }
        if let Some(repo) = repos.get(selected_repo) {
            observability::phase("startup_setup_prompt", || {
                setup::maybe_prompt_startup_setup(&repo.repo, &repo.config)
            })?;
        }
        observability::phase("reconcile_worktrees", || {
            for managed in &repos {
                session::reconcile_worktree_state(&managed.repo, &managed.config)?;
            }
            Ok(())
        })?;
        let sessions =
            observability::phase("discover_sessions", || discover_workspace_sessions(&repos))?;
        let mut tui = observability::phase("initialize_tui", || {
            Ok(tui::Tui::new(repos, selected_repo, sessions))
        })?;
        tui.use_persisted_ui_state(ui_state::path());
        tui.select_repo(selected_repo);
        observability::phase("run_tui", || tui.run())
    })();
    match &result {
        Ok(_) => observability::finish_startup_run("ok", None),
        Err(error) => observability::finish_startup_run("error", Some(error.as_str())),
    }
    result
}

fn run_auto_command(
    repo: &Repository,
    config: &Config,
    mut command: AutoCommand,
) -> Result<(), String> {
    let existing = observability::with_writable_db(repo, |conn| {
        load_recent_active_runs_for_repo(conn, &repo.root, 1)
    })?;
    if let Some(mut run) = existing.into_iter().next() {
        let should_execute = observability::with_writable_db(repo, |conn| {
            prepare_auto_run_for_resume(
                conn,
                &mut run,
                crate::plan_run::DEFAULT_OUTPUT_LINES_PER_STEP,
            )
        })?;
        if should_execute {
            run_auto_executor(repo, config, &mut run)?;
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
    let mut persisted = launch.create_run();
    observability::with_writable_db(repo, |conn| save_auto_run(conn, &mut persisted))?;
    run_auto_executor(repo, config, &mut persisted)?;
    println!(
        "auto_run_id = {}\nstatus = {:?}\nworktree = {}",
        persisted.run.id,
        persisted.run.status,
        persisted.run.worktree_path.display()
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

fn run_auto_executor(
    repo: &Repository,
    config: &Config,
    persisted: &mut crate::auto_flow::PersistedAutoRun,
) -> Result<(), String> {
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
        )
        .ok()
        .flatten();
    let executor = AutoExecutorConfig::for_harness(
        persisted.run.harness_id.clone(),
        harness_config,
        runtime.map(|runtime| runtime.server_url),
        persisted.run.worktree_path.clone(),
        format!("Auto Flow {}", persisted.run.prompt_summary),
    );
    observability::with_writable_db(repo, |conn| {
        execute_auto_initial_step(
            conn,
            repo,
            config,
            persisted,
            &executor,
            &mut std::io::sink(),
        )
    })
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
            Ok(())
        }
        DebugCommand::Logs => {
            for line in observability::tail_runtime_log(repo, 200)? {
                println!("{line}");
            }
            Ok(())
        }
        DebugCommand::Startup => run_debug_startup(repo, config),
    }
}

fn run_debug_startup(repo: &Repository, config: &mut Config) -> Result<(), String> {
    observability::start_startup_run(env!("CARGO_PKG_VERSION"));
    let result: Result<(), String> = (|| {
        observability::phase("ensure_tools", || config::ensure_required_tools(config))?;
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
    match &result {
        Ok(_) => observability::finish_startup_run("ok", None),
        Err(error) => observability::finish_startup_run("error", Some(error.as_str())),
    }
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
        DbCommand::Query(query) => observability::run_readonly_query(repo, &query),
    }
}

fn open_interactive_db(repo: &Repository) -> Result<(), String> {
    observability::with_writable_db(repo, |_| Ok(()))?;

    let path = observability::db_path(repo);
    let status = ProcessCommand::new("sqlite3")
        .arg(&path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "sqlite3 not found; install sqlite3".to_string()
            } else {
                format!("launch sqlite3 for {}: {error}", path.display())
            }
        })?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("sqlite3 exited with status {status}"))
    }
}
