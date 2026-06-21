use crate::args::{self, Args, CommandKind, DbCommand, DebugCommand};
use crate::config::Config;
use crate::observability::{self, LogLevel, ObserverOptions};
use crate::repo::Repository;
use crate::tui::ManagedRepo;
use crate::{config, plan, session, setup, tui, workspace};

pub fn run() -> Result<(), String> {
    let args = Args::parse(std::env::args_os().skip(1))?;
    if matches!(args.command, CommandKind::Help | CommandKind::Version) {
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
        CommandKind::Help | CommandKind::Version => run_static_command(args.command),
        CommandKind::Config => {
            let (repo, config) = load_single_repo_context(args.repo.as_deref())?;
            config::print_config(&repo, &config);
            Ok(())
        }
        CommandKind::Doctor => {
            let (repo, mut config) = load_single_repo_context(args.repo.as_deref())?;
            config::doctor(&repo, &mut config)
        }
        CommandKind::RunPlan(path) => {
            let (repo, config) = load_single_repo_context(args.repo.as_deref())?;
            plan::run_plan_mode(&repo.root, &config, path.as_deref())
        }
        CommandKind::Debug(command) => {
            let (repo, mut config) = load_single_repo_context(args.repo.as_deref())?;
            run_debug_command(command, &repo, &mut config)
        }
        CommandKind::Db(command) => {
            let (repo, _) = load_single_repo_context(args.repo.as_deref())?;
            run_db_command(command, &repo)
        }
        CommandKind::Tui => run_tui(args.repo.as_deref()),
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
        let selected_repo = selected_repo.min(repos.len().saturating_sub(1));
        if let Some(repo) = repos.get(selected_repo) {
            observability::attach_repo(&repo.repo);
        }
        if let Some(repo) = repos.get(selected_repo) {
            observability::phase("startup_setup_prompt", || {
                setup::maybe_prompt_startup_setup(&repo.repo, &repo.config)
            })?;
        }
        let sessions =
            observability::phase("discover_sessions", || discover_workspace_sessions(&repos))?;
        let mut tui = observability::phase("initialize_tui", || {
            Ok(tui::Tui::new(repos, selected_repo, sessions))
        })?;
        tui.select_repo(selected_repo);
        observability::phase("run_tui", || tui.run())
    })();
    match &result {
        Ok(_) => observability::finish_startup_run("ok", None),
        Err(error) => observability::finish_startup_run("error", Some(error.as_str())),
    }
    result
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
            println!("default_agent = {}", config.default_agent);
            println!(
                "default_agent_prompt_mode = {}",
                config.agent_prompt_mode(&config.default_agent).label()
            );
            println!(
                "default_agent_command = {}",
                observability::sanitize_command_text(&config.agent_command(&config.default_agent))
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
        DbCommand::Path => {
            println!("{}", observability::db_path(repo).display());
            Ok(())
        }
        DbCommand::Query(query) => observability::run_readonly_query(repo, &query),
    }
}
