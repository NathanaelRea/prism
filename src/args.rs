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
    Help,
    Version,
    Doctor,
    Config,
    Auto(AutoCommand),
    RunPlan(Option<PathBuf>),
    Debug(DebugCommand),
    Db(DbCommand),
}

#[derive(Debug)]
pub struct AutoCommand {
    pub plan_first: bool,
    pub prompt: Option<String>,
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
                "auto" => {
                    let first = iter.next().map(|arg| arg.to_string_lossy().to_string());
                    let (plan_first, prompt) = match first.as_deref() {
                        Some("plan") | Some("plan-first") | Some("intensive") => (
                            true,
                            iter.next().map(|arg| arg.to_string_lossy().to_string()),
                        ),
                        _ => (false, first),
                    };
                    command = CommandKind::Auto(AutoCommand { plan_first, prompt });
                }
                "run-plan" | "plan" => {
                    command = CommandKind::RunPlan(iter.next().map(PathBuf::from));
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
                "-h" | "--help" => command = CommandKind::Help,
                "--version" => command = CommandKind::Version,
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

pub fn help_text() -> &'static str {
    "Usage:\n  prism [--repo <path>] [--debug] [--print-logs] [--log-level <level>]\n  prism [--repo <path>] doctor\n  prism [--repo <path>] config\n  prism [--repo <path>] auto [prompt]\n  prism [--repo <path>] auto plan [prompt]\n  prism [--repo <path>] run-plan [plan.md]\n  prism [--repo <path>] plan [plan.md]\n  prism [--repo <path>] debug paths|info|logs|startup\n  prism [--repo <path>] db path\n  prism [--repo <path>] db <read-only-sql>"
}
