mod actions;
mod agent;
mod args;
mod config;
mod git;
mod github;
mod input;
mod json;
mod observability;
mod plan;
mod process;
mod repo;
mod review;
mod session;
mod setup;
mod terminal;
mod tmux;
mod tui;
mod util;
mod view;

use args::{Args, CommandKind, DbCommand, DebugCommand};
use config::Config;
use observability::{LogLevel, ObserverOptions};
use repo::Repository;

fn main() {
    if let Err(error) = run() {
        observability::emit(observability::EventInput {
            level: LogLevel::Error,
            target: "process",
            action: "fatal",
            operation_id: None,
            parent_operation_id: None,
            branch: None,
            session: None,
            message: error.clone(),
            data_json: None,
        });
        eprintln!("prism: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse(std::env::args_os().skip(1))?;
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

    let repo = observability::phase("discover_repo", || {
        Repository::discover(args.repo.as_deref())
    })?;
    observability::attach_repo(&repo);
    let mut config = observability::phase("load_config", || Ok(Config::load(&repo)))?;

    match args.command {
        CommandKind::Config => {
            config::print_config(&repo, &config);
            Ok(())
        }
        CommandKind::Doctor => config::doctor(&repo, &mut config),
        CommandKind::RunPlan(path) => plan::run_plan_cli(&repo, &config, &path),
        CommandKind::Debug(command) => run_debug_command(command, &repo, &mut config),
        CommandKind::Db(command) => run_db_command(command, &repo),
        CommandKind::Tui => run_tui(repo, config, args.allow_dirty),
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

fn run_tui(repo: Repository, mut config: Config, allow_dirty: bool) -> Result<(), String> {
    observability::start_startup_run(env!("CARGO_PKG_VERSION"));
    let result: Result<(), String> = (|| {
        observability::phase("ensure_tools", || config::ensure_required_tools(&config))?;
        observability::phase("ensure_default_agent", || {
            config::ensure_default_agent(&mut config)
        })?;
        observability::phase("startup_setup_prompt", || {
            setup::maybe_prompt_startup_setup(&repo, &config)
        })?;
        let sessions = observability::phase("discover_sessions", || {
            session::discover_sessions(&repo, &config)
        })?;
        let mut tui = observability::phase("initialize_tui", || {
            Ok(tui::Tui::new(repo, config, sessions, allow_dirty))
        })?;
        observability::phase("run_tui", || tui.run())
    })();
    match &result {
        Ok(_) => observability::finish_startup_run("ok", None),
        Err(error) => observability::finish_startup_run("error", Some(error.as_str())),
    }
    result
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
