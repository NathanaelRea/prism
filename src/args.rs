use std::ffi::OsString;
use std::path::PathBuf;

use crate::observability::LogLevel;

#[derive(Debug)]
pub struct Args {
    pub repo: Option<PathBuf>,
    pub debug: bool,
    pub print_logs: bool,
    pub log_level: Option<LogLevel>,
    pub command: CommandKind,
}

#[derive(Debug)]
pub enum CommandKind {
    Tui,
    Doctor,
    Config,
    RunPlan(PathBuf),
    Debug(DebugCommand),
    Db(DbCommand),
}

#[derive(Debug)]
pub enum DebugCommand {
    Paths,
    Info,
    Logs,
    Startup,
}

#[derive(Debug)]
pub enum DbCommand {
    Path,
    Query(String),
}

impl Args {
    pub fn parse<I>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut repo = None;
        let mut debug = false;
        let mut print_logs = false;
        let mut log_level = None;
        let mut command = CommandKind::Tui;
        let mut iter = args.into_iter();

        while let Some(arg) = iter.next() {
            let text = arg.to_string_lossy();
            match text.as_ref() {
                "--repo" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| "--repo requires a path".to_string())?;
                    repo = Some(PathBuf::from(value));
                }
                "--debug" => debug = true,
                "--print-logs" => print_logs = true,
                "--log-level" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| "--log-level requires a level".to_string())?;
                    let value = value.to_string_lossy();
                    log_level = Some(LogLevel::parse(&value).ok_or_else(|| {
                        format!(
                            "unknown log level: {value}; expected error, warn, info, debug, or trace"
                        )
                    })?);
                }
                "doctor" => command = CommandKind::Doctor,
                "config" => command = CommandKind::Config,
                "run-plan" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| "run-plan requires a plan file".to_string())?;
                    command = CommandKind::RunPlan(PathBuf::from(value));
                }
                "debug" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| "debug requires a subcommand".to_string())?;
                    let value = value.to_string_lossy();
                    command = CommandKind::Debug(match value.as_ref() {
                        "paths" => DebugCommand::Paths,
                        "info" => DebugCommand::Info,
                        "logs" => DebugCommand::Logs,
                        "startup" => DebugCommand::Startup,
                        other => return Err(format!("unknown debug subcommand: {other}")),
                    });
                }
                "db" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| "db requires `path` or a read-only SQL query".to_string())?;
                    let mut parts = vec![value.to_string_lossy().to_string()];
                    parts.extend(iter.map(|arg| arg.to_string_lossy().to_string()));
                    if parts.len() == 1 && parts[0] == "path" {
                        command = CommandKind::Db(DbCommand::Path);
                    } else {
                        command = CommandKind::Db(DbCommand::Query(parts.join(" ")));
                    }
                    break;
                }
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                "--version" => {
                    println!("prism {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }

        Ok(Self {
            repo,
            debug,
            print_logs,
            log_level,
            command,
        })
    }
}

fn print_help() {
    println!(
        "Usage:\n  prism [--repo <path>] [--debug] [--print-logs] [--log-level <level>]\n  prism [--repo <path>] doctor\n  prism [--repo <path>] config\n  prism [--repo <path>] run-plan <plan.md>\n  prism [--repo <path>] debug paths|info|logs|startup\n  prism [--repo <path>] db path\n  prism [--repo <path>] db <read-only-sql>"
    );
}
