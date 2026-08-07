use std::ffi::OsString;
use std::path::PathBuf;

use crate::flight_recorder::RecordOptions;
use crate::observability::LogLevel;

#[derive(Debug)]
pub struct Args {
    pub repo: Option<PathBuf>,
    pub debug: bool,
    pub print_logs: bool,
    pub log_level: Option<LogLevel>,
    pub command: CommandKind,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CommandKind {
    Tui,
    Help,
    Version,
    DebugHelp,
    DbHelp,
    Doctor,
    Config(ConfigCommand),
    Agent(AgentCommand),
    Workflow(Vec<String>),
    Extension(Vec<String>),
    Package(Vec<String>),
    Skill(Vec<String>),
    Template(Vec<String>),
    Debug(DebugCommand),
    Db(DbCommand),
    Worker(WorkerCommand),
    List(InspectOptions),
    Status(StatusOptions),
    Pause(Option<String>),
    Resume(Option<String>),
    Stop(Option<String>),
    Recover(Option<String>),
    Daemon(DaemonCommand),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct InspectOptions {
    pub all: bool,
    pub json: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct StatusOptions {
    pub selector: Option<String>,
    pub json: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DaemonCommand {
    Status { json: bool },
    Start,
    Stop,
}

#[derive(Debug, PartialEq, Eq)]
pub enum WorkerCommand {
    Serve,
    Ensure,
    Health,
    Shutdown,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConfigCommand {
    Show,
    Example,
    Schema,
    Paths,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AgentCommand {
    Ensure { branch: String },
}

#[derive(Debug, PartialEq, Eq)]
pub enum DebugCommand {
    Paths,
    Info,
    Logs,
    Startup,
    Integrity,
    Record(RecordOptions),
}

#[derive(Debug, PartialEq, Eq)]
pub enum DbCommand {
    Shell,
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
                "config" => {
                    let Some(value) = iter.next() else {
                        command = CommandKind::Config(ConfigCommand::Show);
                        break;
                    };
                    let value = value.to_string_lossy();
                    command = CommandKind::Config(match value.as_ref() {
                        "show" => ConfigCommand::Show,
                        "example" => ConfigCommand::Example,
                        "schema" => ConfigCommand::Schema,
                        "paths" => ConfigCommand::Paths,
                        other => return Err(format!("unknown config subcommand: {other}")),
                    });
                    break;
                }
                "agent" => {
                    let subcommand = iter
                        .next()
                        .ok_or_else(|| "agent requires a subcommand".to_string())?;
                    let subcommand = subcommand.to_string_lossy();
                    if subcommand != "ensure" {
                        return Err(format!("unknown agent subcommand: {subcommand}"));
                    }
                    let mut branch = None;
                    while let Some(flag) = iter.next() {
                        let flag = flag.to_string_lossy();
                        match flag.as_ref() {
                            "--branch" if branch.is_none() => {
                                let value = iter.next().ok_or_else(|| {
                                    "agent ensure requires --branch <branch>".to_string()
                                })?;
                                let value = value.to_string_lossy().trim().to_string();
                                if value.is_empty() {
                                    return Err(
                                        "agent ensure requires --branch <branch>".to_string()
                                    );
                                }
                                branch = Some(value);
                            }
                            "--branch" => {
                                return Err("agent ensure accepts --branch only once".to_string());
                            }
                            other => return Err(format!("unknown agent ensure argument: {other}")),
                        }
                    }
                    command = CommandKind::Agent(AgentCommand::Ensure {
                        branch: branch
                            .ok_or_else(|| "agent ensure requires --branch <branch>".to_string())?,
                    });
                    break;
                }
                family @ ("workflow" | "extension" | "package" | "skill" | "template") => {
                    let arguments = iter
                        .map(|argument| argument.to_string_lossy().into_owned())
                        .collect::<Vec<_>>();
                    if arguments.is_empty() {
                        return Err(format!("{family} requires a subcommand"));
                    }
                    command = match family {
                        "workflow" => CommandKind::Workflow(arguments),
                        "extension" => CommandKind::Extension(arguments),
                        "package" => CommandKind::Package(arguments),
                        "skill" => CommandKind::Skill(arguments),
                        "template" => CommandKind::Template(arguments),
                        _ => unreachable!(),
                    };
                    break;
                }
                "debug" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| "debug requires a subcommand".to_string())?;
                    let value = value.to_string_lossy();
                    if value == "-h" || value == "--help" {
                        command = CommandKind::DebugHelp;
                        break;
                    }
                    if value == "record" {
                        let mut options = RecordOptions::default();
                        while let Some(flag) = iter.next() {
                            let flag = flag.to_string_lossy();
                            let target = match flag.as_ref() {
                                "--before" => &mut options.before_seconds,
                                "--after" => &mut options.after_seconds,
                                other => {
                                    return Err(format!("unknown debug record argument: {other}"));
                                }
                            };
                            let value = iter
                                .next()
                                .ok_or_else(|| format!("{flag} requires a duration in seconds"))?;
                            *target = value.to_string_lossy().parse::<u64>().map_err(|_| {
                                format!("{flag} requires a whole number of seconds")
                            })?;
                        }
                        command = CommandKind::Debug(DebugCommand::Record(options.validate()?));
                        break;
                    }
                    command = CommandKind::Debug(match value.as_ref() {
                        "paths" => DebugCommand::Paths,
                        "info" => DebugCommand::Info,
                        "logs" => DebugCommand::Logs,
                        "startup" => DebugCommand::Startup,
                        "integrity" => DebugCommand::Integrity,
                        other => return Err(format!("unknown debug subcommand: {other}")),
                    });
                }
                "db" => {
                    let Some(value) = iter.next() else {
                        command = CommandKind::Db(DbCommand::Shell);
                        break;
                    };
                    let value_text = value.to_string_lossy().to_string();
                    if value_text == "-h" || value_text == "--help" {
                        command = CommandKind::DbHelp;
                        break;
                    }
                    let mut parts = vec![value_text];
                    parts.extend(iter.map(|arg| arg.to_string_lossy().to_string()));
                    if parts.len() == 1 && parts[0] == "path" {
                        command = CommandKind::Db(DbCommand::Path);
                    } else {
                        command = CommandKind::Db(DbCommand::Query(parts.join(" ")));
                    }
                    break;
                }
                "worker" => {
                    let subcommand = iter
                        .next()
                        .ok_or_else(|| "worker requires a subcommand".to_string())?;
                    let worker_command = match subcommand.to_string_lossy().as_ref() {
                        "serve" => WorkerCommand::Serve,
                        "ensure" => WorkerCommand::Ensure,
                        "health" => WorkerCommand::Health,
                        "shutdown" => WorkerCommand::Shutdown,
                        other => return Err(format!("unknown worker subcommand: {other}")),
                    };
                    if let Some(extra) = iter.next() {
                        return Err(format!(
                            "unknown worker argument: {}",
                            extra.to_string_lossy()
                        ));
                    }
                    command = CommandKind::Worker(worker_command);
                    break;
                }
                "list" => {
                    let mut options = InspectOptions::default();
                    while let Some(flag) = iter.next() {
                        match flag.to_string_lossy().as_ref() {
                            "--all" => options.all = true,
                            "--json" => options.json = true,
                            "--repo" if repo.is_none() => {
                                repo = Some(PathBuf::from(
                                    iter.next()
                                        .ok_or_else(|| "--repo requires a path".to_string())?,
                                ));
                            }
                            "--repo" => return Err("--repo accepts only one path".to_string()),
                            other => return Err(format!("unknown list argument: {other}")),
                        }
                    }
                    command = CommandKind::List(options);
                    break;
                }
                "status" => {
                    let mut options = StatusOptions::default();
                    while let Some(value) = iter.next() {
                        let value = value.to_string_lossy().to_string();
                        if value == "--json" {
                            options.json = true;
                        } else if value == "--repo" && repo.is_none() {
                            repo = Some(PathBuf::from(
                                iter.next()
                                    .ok_or_else(|| "--repo requires a path".to_string())?,
                            ));
                        } else if value == "--repo" {
                            return Err("--repo accepts only one path".to_string());
                        } else if options.selector.is_none() {
                            options.selector = Some(value);
                        } else {
                            return Err(format!("unknown status argument: {value}"));
                        }
                    }
                    command = CommandKind::Status(options);
                    break;
                }
                "pause" | "resume" | "stop" | "recover" => {
                    let name = text.into_owned();
                    let mut selector = None;
                    while let Some(value) = iter.next() {
                        let value = value.to_string_lossy().to_string();
                        if value == "--repo" && repo.is_none() {
                            repo = Some(PathBuf::from(
                                iter.next()
                                    .ok_or_else(|| "--repo requires a path".to_string())?,
                            ));
                        } else if value == "--repo" {
                            return Err("--repo accepts only one path".to_string());
                        } else if selector.is_none() {
                            selector = Some(value);
                        } else {
                            return Err(format!("unknown {name} argument: {value}"));
                        }
                    }
                    command = match name.as_str() {
                        "pause" => CommandKind::Pause(selector),
                        "resume" => CommandKind::Resume(selector),
                        "stop" => CommandKind::Stop(selector),
                        _ => CommandKind::Recover(selector),
                    };
                    break;
                }
                "daemon" => {
                    let subcommand = iter
                        .next()
                        .ok_or_else(|| "daemon requires a subcommand".to_string())?;
                    command = match subcommand.to_string_lossy().as_ref() {
                        "status" => {
                            let json = match iter.next() {
                                None => false,
                                Some(flag) if flag == "--json" => true,
                                Some(flag) => {
                                    return Err(format!(
                                        "unknown daemon status argument: {}",
                                        flag.to_string_lossy()
                                    ));
                                }
                            };
                            CommandKind::Daemon(DaemonCommand::Status { json })
                        }
                        "start" => CommandKind::Daemon(DaemonCommand::Start),
                        "stop" => CommandKind::Daemon(DaemonCommand::Stop),
                        other => return Err(format!("unknown daemon subcommand: {other}")),
                    };
                    if let Some(extra) = iter.next() {
                        return Err(format!(
                            "unknown daemon argument: {}",
                            extra.to_string_lossy()
                        ));
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
    "Usage:\n  prism [--repo <path>] [--debug] [--print-logs] [--log-level <level>]\n  prism [--repo <path>] workflow list|show|new|copy|edit|validate|preview|run|history|migrate|updates\n  prism [--repo <path>] workflow pause|resume|cancel|retry|approve|reject\n  prism [--repo <path>] extension list|show|new|edit|check|build|reload|doctor\n  prism [--repo <path>] package new|validate|install|list|show|update|remove\n  prism [--repo <path>] skill list|show|install|remove\n  prism [--repo <path>] template list|show|copy\n  prism [--repo <path>] list [--all] [--json]\n  prism [--repo <path>] status [<selector>] [--json]\n  prism [--repo <path>] pause|resume|stop [<workflow-selector>]\n  prism [--repo <path>] recover [<workflow-selector>]\n  prism daemon status [--json]\n  prism daemon start|stop\n  prism [--repo <path>] doctor\n  prism [--repo <path>] config [show|example|schema|paths]\n  prism [--repo <path>] agent ensure --branch <branch>\n  prism [--repo <path>] debug paths|info|logs|startup|integrity\n  prism [--repo <path>] debug record [--before <seconds>] [--after <seconds>]\n  prism [--repo <path>] debug --help\n  prism [--repo <path>] db\n  prism [--repo <path>] db path\n  prism [--repo <path>] db <read-only-sql>\n  prism [--repo <path>] db --help\n\nWorkflow JSON:\n  Stable --json responses use {schema_version, kind, data}.\n  workflow run accepts repeated --input name=json and --idempotency-key <key>.\n\nSelectors:\n  Use a workflow run id, repo:<name>, wt:<branch>, or an absolute repository/worktree path.\n\nDebugging:\n  Use `debug record` while Prism is running to capture its in-memory flight recorder,\n  `debug paths` to find Prism state, `debug logs` to tail the runtime log, and\n  `debug integrity` for read-only database checks. Use `db path` or\n  `db <read-only-sql>` to inspect persisted repo state.\n  Use `--print-logs --log-level trace` to print detailed subprocess logs."
}

pub fn debug_help_text() -> &'static str {
    "Usage:\n  prism [--repo <path>] debug paths\n  prism [--repo <path>] debug info\n  prism [--repo <path>] debug logs\n  prism [--repo <path>] debug startup\n  prism [--repo <path>] debug integrity\n  prism [--repo <path>] debug record [--before <seconds>] [--after <seconds>]\n\nDebug commands:\n  paths      print repo root, Prism state directory, database path, runtime log path, and config paths\n  info       print resolved runtime/config facts and startup setup facts\n  logs       tail the repo runtime log from Prism state\n  startup    run startup checks and print startup timing/debug output\n  integrity  inspect SQLite integrity and foreign keys without migrating or writing\n  record     capture the running TUI's flight recorder (default: previous 60s plus next 30s)\n\nRecord options:\n  --before <seconds>     history to include, from 0 to 60 (default: 60)\n  --after <seconds>      time to continue recording, from 0 to 30 (default: 30)\n\nLogging flags:\n  --print-logs           print runtime logs to stderr while Prism runs\n  --log-level trace      include detailed subprocess argv/status logs"
}

pub fn db_help_text() -> &'static str {
    "Usage:\n  prism [--repo <path>] db\n  prism [--repo <path>] db path\n  prism [--repo <path>] db <read-only-sql>\n\nDB commands:\n  db                  open sqlite3 on the repo Prism database\n  db path             print the repo Prism database path\n  db <read-only-sql>  run a read-only SQL query against persisted repo state\n\nWhen --repo is omitted outside a Git repo, db uses the first configured Prism repository."
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> CommandKind {
        Args::parse(args.iter().map(OsString::from))
            .unwrap()
            .command
    }

    #[test]
    fn help_documents_current_workflow_selectors() {
        let help = help_text();
        assert!(help.contains("workflow run id"));
    }

    #[test]
    fn agent_ensure_requires_and_parses_branch() {
        assert_eq!(
            parse(&["agent", "ensure", "--branch", "feature/e2e"]),
            CommandKind::Agent(AgentCommand::Ensure {
                branch: "feature/e2e".to_string(),
            })
        );
        assert_eq!(
            Args::parse([OsString::from("agent"), OsString::from("ensure")]).unwrap_err(),
            "agent ensure requires --branch <branch>"
        );
    }

    #[test]
    fn help_documents_agent_ensure() {
        assert!(help_text().contains("agent ensure --branch <branch>"));
    }

    #[test]
    fn db_without_arguments_parses_as_shell() {
        assert_eq!(parse(&["db"]), CommandKind::Db(DbCommand::Shell));
    }

    #[test]
    fn config_subcommands_parse() {
        assert_eq!(parse(&["config"]), CommandKind::Config(ConfigCommand::Show));
        assert_eq!(
            parse(&["config", "show"]),
            CommandKind::Config(ConfigCommand::Show)
        );
        assert_eq!(
            parse(&["config", "example"]),
            CommandKind::Config(ConfigCommand::Example)
        );
        assert_eq!(
            parse(&["config", "schema"]),
            CommandKind::Config(ConfigCommand::Schema)
        );
        assert_eq!(
            parse(&["config", "paths"]),
            CommandKind::Config(ConfigCommand::Paths)
        );
    }

    #[test]
    fn db_path_parses_as_path_command() {
        assert_eq!(parse(&["db", "path"]), CommandKind::Db(DbCommand::Path));
    }

    #[test]
    fn db_query_joins_remaining_arguments() {
        assert_eq!(
            parse(&["db", "select", "*", "from", "task_metadata"]),
            CommandKind::Db(DbCommand::Query("select * from task_metadata".to_string()))
        );
    }

    #[test]
    fn db_whitespace_query_parses_as_query() {
        assert_eq!(
            parse(&["db", "   "]),
            CommandKind::Db(DbCommand::Query("   ".to_string()))
        );
    }

    #[test]
    fn help_documents_db_forms() {
        let help = help_text();
        assert!(help.contains("prism [--repo <path>] db\n"));
        assert!(help.contains("prism [--repo <path>] db path"));
        assert!(help.contains("prism [--repo <path>] db <read-only-sql>"));
    }

    #[test]
    fn internal_worker_command_parses() {
        for (name, expected) in [
            ("serve", WorkerCommand::Serve),
            ("ensure", WorkerCommand::Ensure),
            ("health", WorkerCommand::Health),
            ("shutdown", WorkerCommand::Shutdown),
        ] {
            assert_eq!(parse(&["worker", name]), CommandKind::Worker(expected));
        }
    }

    #[test]
    fn inspection_and_control_commands_parse() {
        assert_eq!(
            parse(&["list", "--all", "--json"]),
            CommandKind::List(InspectOptions {
                all: true,
                json: true
            })
        );
        assert_eq!(
            parse(&["status", "w:12345678", "--json"]),
            CommandKind::Status(StatusOptions {
                selector: Some("w:12345678".to_string()),
                json: true
            })
        );
        assert_eq!(parse(&["pause"]), CommandKind::Pause(None));
        assert_eq!(
            parse(&["resume", "w:12345678"]),
            CommandKind::Resume(Some("w:12345678".to_string()))
        );
        assert_eq!(
            parse(&["stop", "w:run-1"]),
            CommandKind::Stop(Some("w:run-1".to_string()))
        );
        assert_eq!(parse(&["recover"]), CommandKind::Recover(None));
    }

    #[test]
    fn inspection_and_control_accept_command_local_repo() {
        for arguments in [
            vec!["list", "--json", "--repo", "/tmp/repo"],
            vec!["status", "--repo", "/tmp/repo", "wt:feature"],
            vec!["pause", "a:12345678", "--repo", "/tmp/repo"],
        ] {
            let parsed = Args::parse(arguments.into_iter().map(OsString::from)).unwrap();
            assert_eq!(parsed.repo, Some(PathBuf::from("/tmp/repo")));
        }
    }

    #[test]
    fn daemon_commands_parse() {
        assert_eq!(
            parse(&["daemon", "status", "--json"]),
            CommandKind::Daemon(DaemonCommand::Status { json: true })
        );
        assert_eq!(
            parse(&["daemon", "start"]),
            CommandKind::Daemon(DaemonCommand::Start)
        );
        assert_eq!(
            parse(&["daemon", "stop"]),
            CommandKind::Daemon(DaemonCommand::Stop)
        );
    }

    #[test]
    fn debug_help_parses_as_static_command() {
        assert_eq!(parse(&["debug", "--help"]), CommandKind::DebugHelp);
        assert!(debug_help_text().contains("debug logs"));
    }

    #[test]
    fn debug_record_uses_bounded_defaults_and_parses_overrides() {
        assert_eq!(
            parse(&["debug", "record"]),
            CommandKind::Debug(DebugCommand::Record(RecordOptions::default()))
        );
        assert_eq!(
            parse(&["debug", "record", "--before", "15", "--after", "5"]),
            CommandKind::Debug(DebugCommand::Record(RecordOptions {
                before_seconds: 15,
                after_seconds: 5,
            }))
        );
        assert!(
            Args::parse(
                ["debug", "record", "--before", "61"]
                    .into_iter()
                    .map(OsString::from)
            )
            .is_err()
        );
    }

    #[test]
    fn db_help_parses_as_static_command() {
        assert_eq!(parse(&["db", "--help"]), CommandKind::DbHelp);
        assert!(db_help_text().contains("db path"));
    }
}
