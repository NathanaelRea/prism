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
    Workflow(WorkflowCommand),
    Config(ConfigCommand),
    Agent(AgentCommand),
    Auto(AutoCommand),
    RunPlan(Option<PathBuf>),
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

#[derive(Debug, PartialEq, Eq)]
pub enum WorkflowCommand {
    List {
        json: bool,
    },
    Validate {
        selector: Option<String>,
        all: bool,
        json: bool,
    },
    Preview {
        selector: String,
        json: bool,
    },
    Trust {
        selector: String,
    },
    Launch {
        selector: String,
        inputs: Vec<String>,
        idempotency_key: Option<String>,
        actor: Option<String>,
        json: bool,
    },
    Schema,
    Example,
    Runs {
        json: bool,
    },
    Attention {
        json: bool,
    },
    Status {
        run_id: String,
        json: bool,
    },
    History {
        run_id: String,
        after: i64,
        limit: usize,
        json: bool,
    },
    Pause {
        run_id: String,
    },
    Resume {
        run_id: String,
    },
    Cancel {
        run_id: String,
    },
    Retry {
        attempt_id: String,
    },
    RecoverAttempt {
        attempt_id: String,
        retry: bool,
    },
    Decide {
        request_id: String,
        approved: bool,
        actor: Option<String>,
        reason: Option<String>,
        json: bool,
    },
    Doctor {
        json: bool,
    },
    TriggerList {
        json: bool,
    },
    TriggerEnable {
        id: String,
        enabled: bool,
    },
    TriggerRunNow {
        id: String,
        json: bool,
    },
    TriggerStatus {
        id: Option<String>,
        json: bool,
    },
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
    MigrateWorkflows { json: bool },
}

#[derive(Debug, PartialEq, Eq)]
pub enum AgentCommand {
    Ensure { branch: String },
}

#[derive(Debug, PartialEq, Eq)]
pub struct AutoCommand {
    pub source: AutoCommandSource,
    pub prompt: Option<String>,
    pub plan_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoCommandSource {
    Prompt,
    ExistingPlan,
    DraftPlan,
    ExistingPullRequest,
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
                "workflow" => {
                    let subcommand = iter
                        .next()
                        .ok_or_else(|| "workflow requires a subcommand".to_string())?;
                    let subcommand = subcommand.to_string_lossy().to_string();
                    match subcommand.as_str() {
                        "list" => {
                            let mut json = false;
                            while let Some(flag) = iter.next() {
                                match flag.to_string_lossy().as_ref() {
                                    "--json" => json = true,
                                    "--repo" if repo.is_none() => {
                                        repo =
                                            Some(PathBuf::from(iter.next().ok_or_else(|| {
                                                "--repo requires a path".to_string()
                                            })?));
                                    }
                                    "--repo" => {
                                        return Err("--repo accepts only one path".to_string());
                                    }
                                    other => {
                                        return Err(format!(
                                            "unknown workflow list argument: {other}"
                                        ));
                                    }
                                }
                            }
                            command = CommandKind::Workflow(WorkflowCommand::List { json });
                        }
                        "validate" => {
                            let mut selector = None;
                            let mut all = false;
                            let mut json = false;
                            while let Some(value) = iter.next() {
                                let value = value.to_string_lossy().to_string();
                                match value.as_str() {
                                    "--all" => all = true,
                                    "--json" => json = true,
                                    "--repo" if repo.is_none() => {
                                        repo =
                                            Some(PathBuf::from(iter.next().ok_or_else(|| {
                                                "--repo requires a path".to_string()
                                            })?))
                                    }
                                    "--repo" => {
                                        return Err("--repo accepts only one path".to_string());
                                    }
                                    _ if selector.is_none() => selector = Some(value),
                                    _ => {
                                        return Err(format!(
                                            "unknown workflow validate argument: {value}"
                                        ));
                                    }
                                }
                            }
                            if all == selector.is_some() {
                                return Err(
                                    "workflow validate requires one selector or --all".to_string()
                                );
                            }
                            command = CommandKind::Workflow(WorkflowCommand::Validate {
                                selector,
                                all,
                                json,
                            });
                        }
                        "preview" => {
                            let selector = iter
                                .next()
                                .ok_or_else(|| "workflow preview requires a selector".to_string())?
                                .to_string_lossy()
                                .to_string();
                            let mut json = false;
                            while let Some(flag) = iter.next() {
                                match flag.to_string_lossy().as_ref() {
                                    "--json" => json = true,
                                    "--repo" if repo.is_none() => {
                                        repo =
                                            Some(PathBuf::from(iter.next().ok_or_else(|| {
                                                "--repo requires a path".to_string()
                                            })?))
                                    }
                                    "--repo" => {
                                        return Err("--repo accepts only one path".to_string());
                                    }
                                    other => {
                                        return Err(format!(
                                            "unknown workflow preview argument: {other}"
                                        ));
                                    }
                                }
                            }
                            command =
                                CommandKind::Workflow(WorkflowCommand::Preview { selector, json });
                        }
                        "trust" => {
                            let selector = iter
                                .next()
                                .ok_or_else(|| "workflow trust requires a selector".to_string())?
                                .to_string_lossy()
                                .to_string();
                            if iter.next().is_some() {
                                return Err("workflow trust accepts one selector".to_string());
                            }
                            command = CommandKind::Workflow(WorkflowCommand::Trust { selector });
                        }
                        "launch" => {
                            let selector = iter
                                .next()
                                .ok_or_else(|| "workflow launch requires a selector".to_string())?
                                .to_string_lossy()
                                .to_string();
                            let mut inputs = Vec::new();
                            let mut idempotency_key = None;
                            let mut actor = None;
                            let mut json = false;
                            while let Some(flag) = iter.next() {
                                match flag.to_string_lossy().as_ref() {
                                    "--input" => inputs.push(
                                        iter.next()
                                            .ok_or_else(|| {
                                                "--input requires name=<json>".to_string()
                                            })?
                                            .to_string_lossy()
                                            .to_string(),
                                    ),
                                    "--idempotency-key" if idempotency_key.is_none() => {
                                        idempotency_key = Some(
                                            iter.next()
                                                .ok_or_else(|| {
                                                    "--idempotency-key requires a value".to_string()
                                                })?
                                                .to_string_lossy()
                                                .to_string(),
                                        )
                                    }
                                    "--actor" if actor.is_none() => {
                                        actor = Some(
                                            iter.next()
                                                .ok_or_else(|| {
                                                    "--actor requires a value".to_string()
                                                })?
                                                .to_string_lossy()
                                                .to_string(),
                                        )
                                    }
                                    "--json" => json = true,
                                    other => {
                                        return Err(format!(
                                            "unknown workflow launch argument: {other}"
                                        ));
                                    }
                                }
                            }
                            command = CommandKind::Workflow(WorkflowCommand::Launch {
                                selector,
                                inputs,
                                idempotency_key,
                                actor,
                                json,
                            });
                        }
                        "schema" => command = CommandKind::Workflow(WorkflowCommand::Schema),
                        "example" => command = CommandKind::Workflow(WorkflowCommand::Example),
                        "runs" => {
                            command = CommandKind::Workflow(WorkflowCommand::Runs {
                                json: iter.any(|value| value == "--json"),
                            })
                        }
                        "attention" => {
                            command = CommandKind::Workflow(WorkflowCommand::Attention {
                                json: iter.any(|value| value == "--json"),
                            })
                        }
                        "status" => {
                            let run_id = iter
                                .next()
                                .ok_or_else(|| "workflow status requires a Run ID".to_string())?
                                .to_string_lossy()
                                .to_string();
                            command = CommandKind::Workflow(WorkflowCommand::Status {
                                run_id,
                                json: iter.any(|value| value == "--json"),
                            });
                        }
                        "history" => {
                            let run_id = iter
                                .next()
                                .ok_or_else(|| "workflow history requires a Run ID".to_string())?
                                .to_string_lossy()
                                .to_string();
                            let mut after = 0i64;
                            let mut limit = 100usize;
                            let mut json = false;
                            while let Some(flag) = iter.next() {
                                match flag.to_string_lossy().as_ref() {
                                    "--json" => json = true,
                                    "--after" => {
                                        after = iter
                                            .next()
                                            .ok_or_else(|| {
                                                "--after requires an event ID".to_string()
                                            })?
                                            .to_string_lossy()
                                            .parse()
                                            .map_err(|_| {
                                                "--after requires an integer".to_string()
                                            })?
                                    }
                                    "--limit" => {
                                        limit = iter
                                            .next()
                                            .ok_or_else(|| "--limit requires a count".to_string())?
                                            .to_string_lossy()
                                            .parse()
                                            .map_err(|_| {
                                                "--limit requires an integer".to_string()
                                            })?
                                    }
                                    other => {
                                        return Err(format!(
                                            "unknown workflow history argument: {other}"
                                        ));
                                    }
                                }
                            }
                            command = CommandKind::Workflow(WorkflowCommand::History {
                                run_id,
                                after,
                                limit,
                                json,
                            });
                        }
                        "pause" | "resume" | "cancel" => {
                            let run_id = iter
                                .next()
                                .ok_or_else(|| format!("workflow {subcommand} requires a Run ID"))?
                                .to_string_lossy()
                                .to_string();
                            if iter.next().is_some() {
                                return Err(format!("workflow {subcommand} accepts one Run ID"));
                            }
                            command = CommandKind::Workflow(match subcommand.as_str() {
                                "pause" => WorkflowCommand::Pause { run_id },
                                "resume" => WorkflowCommand::Resume { run_id },
                                _ => WorkflowCommand::Cancel { run_id },
                            });
                        }
                        "retry" => {
                            let attempt_id = iter
                                .next()
                                .ok_or_else(|| "workflow retry requires an Attempt ID".to_string())?
                                .to_string_lossy()
                                .to_string();
                            if iter.next().is_some() {
                                return Err("workflow retry accepts one Attempt ID".to_string());
                            }
                            command = CommandKind::Workflow(WorkflowCommand::Retry { attempt_id });
                        }
                        "recover-attempt" => {
                            let attempt_id = iter
                                .next()
                                .ok_or_else(|| {
                                    "workflow recover-attempt requires an Attempt ID".to_string()
                                })?
                                .to_string_lossy()
                                .to_string();
                            let mut retry = false;
                            for flag in iter {
                                if flag == "--retry" {
                                    retry = true;
                                } else {
                                    return Err(format!(
                                        "unknown workflow recover-attempt argument: {}",
                                        flag.to_string_lossy()
                                    ));
                                }
                            }
                            command = CommandKind::Workflow(WorkflowCommand::RecoverAttempt {
                                attempt_id,
                                retry,
                            });
                        }
                        "approve" | "reject" => {
                            let request_id = iter
                                .next()
                                .ok_or_else(|| {
                                    format!("workflow {subcommand} requires an Approval Request ID")
                                })?
                                .to_string_lossy()
                                .to_string();
                            let mut actor = None;
                            let mut reason = None;
                            let mut json = false;
                            while let Some(flag) = iter.next() {
                                match flag.to_string_lossy().as_ref() {
                                    "--actor" if actor.is_none() => {
                                        actor = Some(
                                            iter.next()
                                                .ok_or_else(|| {
                                                    "--actor requires a value".to_string()
                                                })?
                                                .to_string_lossy()
                                                .to_string(),
                                        )
                                    }
                                    "--reason" if reason.is_none() => {
                                        reason = Some(
                                            iter.next()
                                                .ok_or_else(|| {
                                                    "--reason requires a value".to_string()
                                                })?
                                                .to_string_lossy()
                                                .to_string(),
                                        )
                                    }
                                    "--json" => json = true,
                                    other => {
                                        return Err(format!(
                                            "unknown workflow {subcommand} argument: {other}"
                                        ));
                                    }
                                }
                            }
                            command = CommandKind::Workflow(WorkflowCommand::Decide {
                                request_id,
                                approved: subcommand == "approve",
                                actor,
                                reason,
                                json,
                            });
                        }
                        "doctor" => {
                            command = CommandKind::Workflow(WorkflowCommand::Doctor {
                                json: iter.any(|value| value == "--json"),
                            })
                        }
                        "triggers" => {
                            command = CommandKind::Workflow(WorkflowCommand::TriggerList {
                                json: iter.any(|value| value == "--json"),
                            })
                        }
                        "trigger-enable" | "trigger-disable" => {
                            let id = iter
                                .next()
                                .ok_or_else(|| {
                                    format!("workflow {subcommand} requires a Trigger ID")
                                })?
                                .to_string_lossy()
                                .to_string();
                            command = CommandKind::Workflow(WorkflowCommand::TriggerEnable {
                                id,
                                enabled: subcommand == "trigger-enable",
                            });
                        }
                        "trigger-run-now" => {
                            let id = iter
                                .next()
                                .ok_or_else(|| {
                                    "workflow trigger-run-now requires a Trigger ID".to_string()
                                })?
                                .to_string_lossy()
                                .to_string();
                            command = CommandKind::Workflow(WorkflowCommand::TriggerRunNow {
                                id,
                                json: iter.any(|value| value == "--json"),
                            });
                        }
                        "trigger-status" => {
                            let mut id = None;
                            let mut json = false;
                            for value in iter.by_ref() {
                                if value == "--json" {
                                    json = true;
                                } else if id.is_none() {
                                    id = Some(value.to_string_lossy().to_string());
                                } else {
                                    return Err(
                                        "workflow trigger-status accepts at most one Trigger ID"
                                            .to_string(),
                                    );
                                }
                            }
                            command =
                                CommandKind::Workflow(WorkflowCommand::TriggerStatus { id, json });
                        }
                        other => return Err(format!("unknown workflow subcommand: {other}")),
                    }
                    break;
                }
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
                        "migrate-workflows" => {
                            let mut json = false;
                            for flag in iter.by_ref() {
                                if flag == "--json" {
                                    json = true;
                                } else {
                                    return Err(format!(
                                        "unknown config migrate-workflows argument: {}",
                                        flag.to_string_lossy()
                                    ));
                                }
                            }
                            ConfigCommand::MigrateWorkflows { json }
                        }
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
                "auto" => {
                    let first = iter.next().map(|arg| arg.to_string_lossy().to_string());
                    let (source, prompt, plan_path) = match first.as_deref() {
                        Some("run-plan") => {
                            let path = iter
                                .next()
                                .ok_or_else(|| "auto run-plan requires a plan path".to_string())?;
                            (
                                AutoCommandSource::ExistingPlan,
                                None,
                                Some(PathBuf::from(path)),
                            )
                        }
                        Some("plan") | Some("plan-first") | Some("intensive") => (
                            AutoCommandSource::DraftPlan,
                            iter.next().map(|arg| arg.to_string_lossy().to_string()),
                            None,
                        ),
                        Some("pr") => (AutoCommandSource::ExistingPullRequest, None, None),
                        _ => (AutoCommandSource::Prompt, first, None),
                    };
                    command = CommandKind::Auto(AutoCommand {
                        source,
                        prompt,
                        plan_path,
                    });
                }
                "run-plan" | "plan" => {
                    command = CommandKind::RunPlan(iter.next().map(PathBuf::from));
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
    "Usage:\n  prism [--repo <path>] [--debug] [--print-logs] [--log-level <level>]\n  prism [--repo <path>] workflow list [--json]\n  prism [--repo <path>] workflow validate <selector>|--all [--json]\n  prism [--repo <path>] workflow preview|trust <selector> [--json]\n  prism [--repo <path>] workflow launch <selector> --input <name=json>... [--idempotency-key <key>] [--json]\n  prism workflow schema|example\n  prism workflow runs|attention|status <run-id>|history <run-id> [--json]\n  prism workflow pause|resume|cancel <run-id>\n  prism workflow retry <attempt-id>|recover-attempt <attempt-id> [--retry]\n  prism workflow approve|reject <request-id> [--actor <actor>] [--reason <reason>] [--json]\n  prism workflow doctor|triggers [--json]\n  prism workflow trigger-enable|trigger-disable|trigger-run-now <trigger-id> [--json]\n  prism workflow trigger-status [<trigger-id>] [--json]\n  prism [--repo <path>] list [--all] [--json]\n  prism [--repo <path>] status [<selector>] [--json]\n  prism [--repo <path>] pause|resume|stop [<workflow-selector>]\n  prism [--repo <path>] recover [<workflow-selector>]\n  prism daemon status [--json]\n  prism daemon start|stop\n  prism [--repo <path>] doctor\n  prism [--repo <path>] config [show|example|schema|paths|migrate-workflows [--json]]\n  prism [--repo <path>] agent ensure --branch <branch>\n  prism [--repo <path>] auto [prompt]\n  prism [--repo <path>] auto pr\n  prism [--repo <path>] auto run-plan <plan.md>\n  prism [--repo <path>] auto plan [prompt]\n  prism [--repo <path>] auto plan-first [prompt]\n  prism [--repo <path>] auto intensive [prompt]\n  prism [--repo <path>] run-plan [plan.md]\n  prism [--repo <path>] plan [plan.md]\n  prism [--repo <path>] debug paths|info|logs|startup|integrity\n  prism [--repo <path>] debug record [--before <seconds>] [--after <seconds>]\n  prism [--repo <path>] debug --help\n  prism [--repo <path>] db\n  prism [--repo <path>] db path\n  prism [--repo <path>] db <read-only-sql>\n  prism [--repo <path>] db --help\n\nWorkflow definition selectors:\n  builtin:<name>, global:<name>, repository:<name>, or an unqualified unique name.\n\nSelectors:\n  a:<short-id>, p:<short-id>, auto:<full-id>, plan:<full-id>, repo:<name>,\n  wt:<branch>, or an absolute repository/worktree path.\n\nDebugging:\n  Use `debug record` while Prism is running to capture its in-memory flight recorder,\n  `debug paths` to find Prism state, `debug logs` to tail the runtime log, and\n  `debug integrity` for read-only database checks. Use `db path` or\n  `db <read-only-sql>` to inspect persisted repo state.\n  Use `--print-logs --log-level trace` to print detailed subprocess logs.\n\nAliases:\n  auto plan-first and auto intensive are aliases for auto plan."
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
    fn auto_prompt_parses_existing_prompt_first_form() {
        assert_eq!(
            parse(&["auto", "implement the task"]),
            CommandKind::Auto(AutoCommand {
                source: AutoCommandSource::Prompt,
                prompt: Some("implement the task".to_string()),
                plan_path: None,
            })
        );
    }

    #[test]
    fn auto_run_plan_requires_and_parses_plan_path() {
        assert_eq!(
            parse(&["auto", "run-plan", "plan.md"]),
            CommandKind::Auto(AutoCommand {
                source: AutoCommandSource::ExistingPlan,
                prompt: None,
                plan_path: Some(PathBuf::from("plan.md")),
            })
        );
        assert_eq!(
            Args::parse([OsString::from("auto"), OsString::from("run-plan")]).unwrap_err(),
            "auto run-plan requires a plan path"
        );
    }

    #[test]
    fn auto_pr_parses_existing_pull_request_source() {
        assert_eq!(
            parse(&["auto", "pr"]),
            CommandKind::Auto(AutoCommand {
                source: AutoCommandSource::ExistingPullRequest,
                prompt: None,
                plan_path: None,
            })
        );
    }

    #[test]
    fn auto_plan_aliases_parse_as_draft_plan() {
        for alias in ["plan", "plan-first", "intensive"] {
            assert_eq!(
                parse(&["auto", alias, "draft before coding"]),
                CommandKind::Auto(AutoCommand {
                    source: AutoCommandSource::DraftPlan,
                    prompt: Some("draft before coding".to_string()),
                    plan_path: None,
                })
            );
        }
    }

    #[test]
    fn help_documents_auto_plan_forms() {
        let help = help_text();
        assert!(help.contains("auto run-plan <plan.md>"));
        assert!(help.contains("auto pr"));
        assert!(help.contains("auto plan-first [prompt]"));
        assert!(help.contains("auto intensive [prompt]"));
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
        assert_eq!(
            parse(&["config", "migrate-workflows", "--json"]),
            CommandKind::Config(ConfigCommand::MigrateWorkflows { json: true })
        );
    }

    #[test]
    fn db_path_parses_as_path_command() {
        assert_eq!(parse(&["db", "path"]), CommandKind::Db(DbCommand::Path));
    }

    #[test]
    fn db_query_joins_remaining_arguments() {
        assert_eq!(
            parse(&["db", "select", "*", "from", "plan_run"]),
            CommandKind::Db(DbCommand::Query("select * from plan_run".to_string()))
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
            parse(&["status", "a:12345678", "--json"]),
            CommandKind::Status(StatusOptions {
                selector: Some("a:12345678".to_string()),
                json: true
            })
        );
        assert_eq!(parse(&["pause"]), CommandKind::Pause(None));
        assert_eq!(
            parse(&["resume", "p:12345678"]),
            CommandKind::Resume(Some("p:12345678".to_string()))
        );
        assert_eq!(
            parse(&["stop", "auto:run-1"]),
            CommandKind::Stop(Some("auto:run-1".to_string()))
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
