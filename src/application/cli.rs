use crate::args::{
    self, AgentCommand, Args, AutoCommand, AutoCommandSource, CommandKind, ConfigCommand,
    DaemonCommand, DbCommand, DebugCommand, InspectOptions, StatusOptions, WorkerCommand,
    WorkflowCommand,
};
use crate::config::Config;
use crate::observability::{self, LogLevel, ObserverOptions};
use crate::repo::Repository;
use crate::tui::ManagedRepo;
use crate::workspace_state::{
    ControlAction, ControlRequest, InspectRequest, Subject, WorkspaceContext, WorkspaceSnapshot,
    WorkspaceState,
};
use crate::{agent_session, config, session, setup, tui, ui_state, workspace};
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
        CommandKind::Config(ConfigCommand::MigrateWorkflows { json }) => {
            run_workflow_migration(args.repo.as_deref(), json)
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
        CommandKind::Workflow(command) => run_workflow_command(command, args.repo.as_deref()),
        CommandKind::Agent(command) => {
            let (repo, mut config) = load_single_repo_context(args.repo.as_deref())?;
            config::ensure_default_agent_noninteractive(&mut config)?;
            crate::tmux::migrate_legacy_agent_sessions(&repo, &config)?;
            run_agent_command(command, &repo, &config)
        }
        CommandKind::RunPlan(path) => {
            let (repo, _) = load_single_repo_context(args.repo.as_deref())?;
            let ledger = crate::run::RunLedger::user()?;
            if !ledger.cutover_complete()? {
                return Err("legacy Plan execution is disabled; run `prism config migrate-workflows` before launching bundled Workflows".to_string());
            }
            launch_bundled_plan(&repo, path.as_deref(), ledger)
        }
        CommandKind::Auto(command) => {
            let (repo, _) = load_single_repo_context(args.repo.as_deref())?;
            let ledger = crate::run::RunLedger::user()?;
            if !ledger.cutover_complete()? {
                return Err("legacy Auto Flow execution is disabled; run `prism config migrate-workflows` before launching bundled Workflows".to_string());
            }
            launch_bundled_coding(&repo, command, ledger)
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

#[derive(serde::Serialize)]
struct WorkflowListOutput {
    schema_version: u32,
    definitions: Vec<crate::definition::DefinitionSummary>,
}

fn run_workflow_command(
    command: WorkflowCommand,
    repository: Option<&std::path::Path>,
) -> Result<(), String> {
    if matches!(command, WorkflowCommand::Schema) {
        print!("{}", crate::definition::WORKFLOW_SCHEMA_JSON);
        return Ok(());
    }
    if matches!(command, WorkflowCommand::Example) {
        print!("{}", crate::definition::WORKFLOW_EXAMPLE);
        return Ok(());
    }
    let repository = workflow_repository_root(repository)?;
    let catalog = crate::definition::DefinitionCatalog::discover(repository.as_deref());
    match command {
        WorkflowCommand::List { json } => {
            let definitions = catalog.list()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&WorkflowListOutput {
                        schema_version: 1,
                        definitions
                    })
                    .map_err(|error| error.to_string())?
                );
            } else {
                for definition in definitions {
                    println!(
                        "{}@{}  {}{}",
                        definition.source.qualified_name(),
                        definition.source.revision,
                        if definition.valid { "valid" } else { "invalid" },
                        if definition.trust_required {
                            "  trust required"
                        } else {
                            ""
                        }
                    );
                    for diagnostic in definition.diagnostics {
                        println!("  {}: {}", diagnostic.code, diagnostic.message);
                    }
                }
            }
            Ok(())
        }
        WorkflowCommand::Validate {
            selector,
            all,
            json,
        } => {
            let definitions = if all {
                catalog.list()?
            } else {
                let selector = selector.expect("parser requires selector");
                match catalog.preview(&selector) {
                    Ok(preview) => vec![crate::definition::DefinitionSummary {
                        source: preview.source,
                        valid: true,
                        trust_required: preview.trust_required,
                        requested_capabilities: preview.snapshot.content.capabilities,
                        diagnostics: Vec::new(),
                    }],
                    Err(diagnostics) => {
                        if json {
                            println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "schema_version": 1, "valid": false, "diagnostics": diagnostics })).map_err(|error| error.to_string())?);
                        }
                        return Err(diagnostics
                            .into_iter()
                            .map(|diagnostic| diagnostic.message)
                            .collect::<Vec<_>>()
                            .join("; "));
                    }
                }
            };
            let valid = definitions.iter().all(|definition| definition.valid);
            if json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "schema_version": 1, "valid": valid, "definitions": definitions })).map_err(|error| error.to_string())?);
            } else {
                for definition in &definitions {
                    println!(
                        "{}: {}",
                        definition.source.qualified_name(),
                        if definition.valid { "valid" } else { "invalid" }
                    );
                    for diagnostic in &definition.diagnostics {
                        println!("  {}: {}", diagnostic.code, diagnostic.message);
                    }
                }
            }
            if valid {
                Ok(())
            } else {
                Err("one or more workflow definitions are invalid".to_string())
            }
        }
        WorkflowCommand::Preview { selector, json } => {
            let preview = catalog.preview(&selector).map_err(|diagnostics| {
                diagnostics
                    .into_iter()
                    .map(|diagnostic| diagnostic.message)
                    .collect::<Vec<_>>()
                    .join("; ")
            })?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&preview).map_err(|error| error.to_string())?
                );
            } else {
                println!("definition = {}", preview.source.qualified_name());
                println!("snapshot_digest = {}", preview.snapshot.digest);
                println!("trust_required = {}", preview.trust_required);
                println!(
                    "source_revision = {}",
                    preview.snapshot.content.source_revision
                );
                println!("description = {}", preview.snapshot.content.description);
                println!("inputs = {:#?}", preview.snapshot.content.inputs);
                println!("outputs = {:#?}", preview.snapshot.content.outputs);
                println!("budgets = {:#?}", preview.snapshot.content.budgets);
                println!(
                    "capabilities = {:#?}",
                    preview.snapshot.content.capabilities
                );
                println!(
                    "transitive_capabilities = {:#?}",
                    preview.snapshot.content.transitive_capabilities
                );
                println!(
                    "admission_policy = {:#?}",
                    preview.snapshot.content.admission_policy
                );
                println!(
                    "pinned_workflows = {:#?}",
                    preview.snapshot.content.pinned_workflows
                );
                println!(
                    "implementations = {:#?}",
                    preview.snapshot.content.implementations
                );
                for step in &preview.snapshot.content.steps {
                    println!("step.{} = {step:#?}", step.id);
                }
            }
            Ok(())
        }
        WorkflowCommand::Trust { selector } => {
            let preview = catalog
                .preview(&selector)
                .map_err(format_workflow_diagnostics)?;
            let ledger = crate::run::RunLedger::user()?;
            ledger.trust_definition(&preview.snapshot)?;
            println!(
                "trusted {}@{}",
                preview.source.qualified_name(),
                preview.source.digest
            );
            Ok(())
        }
        WorkflowCommand::Launch {
            selector,
            inputs,
            idempotency_key,
            actor,
            json,
        } => {
            let preview = catalog
                .preview(&selector)
                .map_err(format_workflow_diagnostics)?;
            let inputs = parse_workflow_inputs(&preview.snapshot, &inputs)?;
            let ledger = crate::run::RunLedger::user()?;
            if !ledger.cutover_complete()? {
                return Err(
                    "workflow execution is not cut over; run `prism config migrate-workflows` to stop legacy scheduling and import history first"
                        .to_string(),
                );
            }
            let repository_id = repository
                .as_deref()
                .map(|path| ledger.repository_id(path))
                .transpose()?;
            let actor = actor.unwrap_or_else(current_workflow_actor);
            let receipt = crate::operations::WorkflowOperations::new(ledger).execute(
                crate::operations::WorkflowCommand::Launch(Box::new(crate::run::StartRun {
                    actor_capabilities: preview.snapshot.content.transitive_capabilities.clone(),
                    snapshot: preview.snapshot,
                    repository_id,
                    inputs,
                    idempotency_key,
                    actor,
                })),
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&receipt).map_err(|error| error.to_string())?
                );
            } else if let crate::operations::WorkflowCommandReceipt::Launched(result) = receipt {
                println!(
                    "{}  {}",
                    result.run_id.as_str(),
                    if result.created {
                        "created"
                    } else {
                        "existing"
                    }
                );
            }
            crate::worker::ensure_running()?;
            crate::worker::wake()
        }
        WorkflowCommand::Runs { json } => {
            let runs = crate::run::RunLedger::user()?.list(200)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"schema_version":1,"runs":runs})
                    )
                    .map_err(|error| error.to_string())?
                );
            } else {
                for run in runs {
                    println!(
                        "{}  {}  {}",
                        run.id.as_str(),
                        run.state.label(),
                        run.definition
                    );
                }
            }
            Ok(())
        }
        WorkflowCommand::Attention { json } => {
            let attention = crate::run::RunLedger::user()?.attention(200)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"schema_version":1,"attention":attention})
                    )
                    .map_err(|error| error.to_string())?
                );
            } else {
                println!("pending approvals: {}", attention.pending_approvals);
                println!(
                    "recovery-required attempts: {}",
                    attention.recovery_required_attempts
                );
                println!(
                    "quarantined workspaces: {}",
                    attention.quarantined_workspaces
                );
                for run in attention.runs {
                    println!(
                        "{}  {}  {}",
                        run.id.as_str(),
                        run.state.label(),
                        run.definition
                    );
                }
            }
            Ok(())
        }
        WorkflowCommand::Status { run_id, json } => {
            let projection = crate::run::RunLedger::user()?.inspect(&crate::run::RunId(run_id))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&projection).map_err(|error| error.to_string())?
                );
            } else {
                println!(
                    "{}  {}  {}",
                    projection.run.id.as_str(),
                    projection.run.state.label(),
                    projection.run.definition
                );
                for step in projection.steps {
                    println!(
                        "  {}  {}  {}",
                        step.definition_step_id,
                        step.state.label(),
                        step.blocker.unwrap_or_default()
                    );
                }
            }
            Ok(())
        }
        WorkflowCommand::History {
            run_id,
            after,
            limit,
            json,
        } => {
            let events =
                crate::run::RunLedger::user()?.history(&crate::run::RunId(run_id), after, limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"schema_version":1,"events":events})
                    )
                    .map_err(|error| error.to_string())?
                );
            } else {
                for event in events {
                    println!("{}  {}  {}", event.id, event.kind, event.data_json);
                }
            }
            Ok(())
        }
        WorkflowCommand::Pause { run_id } => control_workflow_run(run_id, "pause_requested"),
        WorkflowCommand::Resume { run_id } => control_workflow_run(run_id, "running"),
        WorkflowCommand::Cancel { run_id } => control_workflow_run(run_id, "cancel_requested"),
        WorkflowCommand::Retry { attempt_id } => {
            let receipt =
                crate::operations::WorkflowOperations::new(crate::run::RunLedger::user()?)
                    .retry_attempt(crate::run::AttemptId(attempt_id))?;
            println!(
                "{}",
                serde_json::to_string(&receipt).map_err(|error| error.to_string())?
            );
            crate::worker::ensure_running()?;
            crate::worker::wake()
        }
        WorkflowCommand::RecoverAttempt { attempt_id, retry } => {
            let receipt =
                crate::operations::WorkflowOperations::new(crate::run::RunLedger::user()?)
                    .execute(crate::operations::WorkflowCommand::Recover {
                        attempt_id: crate::run::AttemptId(attempt_id),
                        retry,
                    })?;
            println!(
                "{}",
                serde_json::to_string(&receipt).map_err(|error| error.to_string())?
            );
            crate::worker::ensure_running()?;
            crate::worker::wake()
        }
        WorkflowCommand::Decide {
            request_id,
            approved,
            actor,
            reason,
            json,
        } => {
            let receipt =
                crate::operations::WorkflowOperations::new(crate::run::RunLedger::user()?)
                    .decide_pending(
                        crate::run::ApprovalRequestId(request_id),
                        approved,
                        actor.unwrap_or_else(current_workflow_actor),
                        reason,
                    )?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&receipt).map_err(|error| error.to_string())?
                );
            } else {
                println!(
                    "{}",
                    serde_json::to_string(&receipt).map_err(|error| error.to_string())?
                );
            }
            crate::worker::ensure_running()?;
            crate::worker::wake()
        }
        WorkflowCommand::Doctor { json } => {
            let health = crate::run::RunLedger::user()?.health()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&health).map_err(|error| error.to_string())?
                );
            } else {
                println!(
                    "workflow database: {}",
                    if health.integrity_ok {
                        "ok"
                    } else {
                        "problems found"
                    }
                );
                println!("active leases: {}", health.active_leases);
                println!("dangling claims: {}", health.dangling_claims);
                println!("quarantined workspaces: {}", health.quarantined_workspaces);
                println!("overdue waits: {}", health.overdue_waits);
                println!(
                    "recovery-required attempts: {}",
                    health.recovery_required_attempts
                );
                println!("unresolved effects: {}", health.unresolved_effects);
                println!("enabled triggers: {}", health.enabled_triggers);
                println!("orphaned blobs: {}", health.orphaned_blobs);
                for target in &health.target_descriptors {
                    println!(
                        "target {}: local={} confined={} continuation={}",
                        target.id, target.local, target.confined, target.supports_continuation
                    );
                }
                for problem in health.problems {
                    println!("  problem: {problem}");
                }
            }
            Ok(())
        }
        WorkflowCommand::TriggerList { json } => {
            let triggers =
                crate::trigger::TriggerEngine::new(crate::run::RunLedger::user()?)?.list()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"schema_version":1,"triggers":triggers})
                    )
                    .map_err(|error| error.to_string())?
                );
            } else {
                for (trigger, enabled) in triggers {
                    println!(
                        "{}  {}  {}",
                        trigger.id,
                        if enabled { "enabled" } else { "disabled" },
                        trigger.definition_selector
                    );
                }
            }
            Ok(())
        }
        WorkflowCommand::TriggerEnable { id, enabled } => {
            crate::trigger::TriggerEngine::new(crate::run::RunLedger::user()?)?
                .set_enabled(&id, enabled)?;
            println!("{}  {}", id, if enabled { "enabled" } else { "disabled" });
            Ok(())
        }
        WorkflowCommand::TriggerStatus { id, json } => {
            let engine = crate::trigger::TriggerEngine::new(crate::run::RunLedger::user()?)?;
            let mut statuses = engine.statuses(crate::run::now_ms(), 20)?;
            if let Some(id) = id {
                statuses.retain(|status| status.definition.id == id);
                if statuses.is_empty() {
                    return Err(format!("Trigger '{id}' was not found"));
                }
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"schema_version":1,"triggers":statuses})
                    )
                    .map_err(|error| error.to_string())?
                );
            } else {
                for status in statuses {
                    println!(
                        "{}  {}  next={}  checkpoint={}",
                        status.definition.id,
                        if status.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        status
                            .next_run_unix_ms
                            .map_or_else(|| "-".into(), |value| value.to_string()),
                        status.checkpoint.as_deref().unwrap_or("-")
                    );
                    for occurrence in status.recent_occurrences {
                        println!(
                            "  {}  {}  run={}",
                            occurrence.id,
                            occurrence.state,
                            occurrence
                                .run_id
                                .as_ref()
                                .map_or("-", crate::run::RunId::as_str)
                        );
                    }
                }
            }
            Ok(())
        }
        WorkflowCommand::TriggerRunNow { id, json } => {
            let ledger = crate::run::RunLedger::user()?;
            if !ledger.cutover_complete()? {
                return Err("workflow execution is not cut over; run `prism config migrate-workflows` first".to_string());
            }
            let engine = crate::trigger::TriggerEngine::new(ledger.clone())?;
            let trigger = engine.get(&id)?;
            let preview = catalog
                .preview(&trigger.definition_selector)
                .map_err(format_workflow_diagnostics)?;
            if preview
                .snapshot
                .content
                .inputs
                .values()
                .any(|port| port.required)
            {
                return Err("trigger run-now requires a definition with no required inputs; provider intake supplies item inputs through polling".to_string());
            }
            let native = format!("manual:{}", crate::run::now_ms());
            let occurrence = engine.record_occurrence(
                &id,
                crate::trigger::OccurrenceIdentity {
                    native_occurrence: &native,
                    provider_item: None,
                    observation_revision: None,
                    definition_digest: &preview.snapshot.digest,
                    input_digest: &crate::run::sha256(b"{}"),
                },
            )?;
            let repository_id = repository
                .as_deref()
                .map(|path| ledger.repository_id(path))
                .transpose()?;
            let started = ledger.start(crate::run::StartRun {
                actor_capabilities: preview.snapshot.content.transitive_capabilities.clone(),
                snapshot: preview.snapshot,
                repository_id,
                inputs: Vec::new(),
                idempotency_key: Some(format!("trigger:{}", occurrence.occurrence_key)),
                actor: current_workflow_actor(),
            })?;
            engine.attach_run(&occurrence.id, &started.run_id)?;
            if json {
                println!("{}",serde_json::to_string_pretty(&serde_json::json!({"schema_version":1,"occurrence":occurrence,"run":started})).map_err(|error|error.to_string())?);
            } else {
                println!("{}  run={}", occurrence.id, started.run_id.as_str());
            }
            crate::worker::ensure_running()?;
            crate::worker::wake()
        }
        WorkflowCommand::Schema | WorkflowCommand::Example => {
            unreachable!("handled before discovery")
        }
    }
}

fn format_workflow_diagnostics(diagnostics: Vec<crate::definition::Diagnostic>) -> String {
    diagnostics
        .into_iter()
        .map(|diagnostic| diagnostic.message)
        .collect::<Vec<_>>()
        .join("; ")
}

fn parse_workflow_inputs(
    snapshot: &crate::definition::DefinitionSnapshot,
    values: &[String],
) -> Result<Vec<crate::run::ArtifactInput>, String> {
    let mut parsed = std::collections::BTreeMap::new();
    for value in values {
        let (name, json) = value
            .split_once('=')
            .ok_or_else(|| format!("workflow input '{value}' must use name=<json>"))?;
        if name.is_empty() {
            return Err("workflow input name cannot be empty".to_string());
        }
        let payload = serde_json::from_str(json)
            .map_err(|error| format!("parse workflow input '{name}': {error}"))?;
        if parsed.insert(name.to_string(), payload).is_some() {
            return Err(format!(
                "workflow input '{name}' was supplied more than once"
            ));
        }
    }
    let unexpected = parsed
        .keys()
        .filter(|name| !snapshot.content.inputs.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !unexpected.is_empty() {
        return Err(format!(
            "workflow has no input port(s): {}",
            unexpected.join(", ")
        ));
    }
    snapshot
        .content
        .inputs
        .iter()
        .filter_map(|(name, port)| match parsed.remove(name) {
            Some(payload) => Some(Ok(crate::run::ArtifactInput {
                name: name.clone(),
                artifact_type: port.artifact_type.clone(),
                payload,
                trust: crate::run::TrustClass::Trusted,
                sensitivity: crate::run::Sensitivity::Internal,
            })),
            None if port.required => {
                Some(Err(format!("required workflow input '{name}' is missing")))
            }
            None => None,
        })
        .collect()
}

fn current_workflow_actor() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "local-user".to_string())
}

fn control_workflow_run(run_id: String, control: &str) -> Result<(), String> {
    let run = crate::run::RunId(run_id.clone());
    let command = match control {
        "pause_requested" => crate::operations::WorkflowCommand::Pause(run),
        "running" => crate::operations::WorkflowCommand::Resume(run),
        "cancel_requested" => crate::operations::WorkflowCommand::Cancel(run),
        _ => return Err(format!("unsupported workflow control '{control}'")),
    };
    crate::operations::WorkflowOperations::new(crate::run::RunLedger::user()?).execute(command)?;
    println!("{run_id}  {control}");
    crate::worker::ensure_running()?;
    crate::worker::wake()
}

fn workflow_repository_root(
    repository: Option<&std::path::Path>,
) -> Result<Option<std::path::PathBuf>, String> {
    let start = match repository {
        Some(path) => path
            .canonicalize()
            .map_err(|error| format!("resolve repository path {}: {error}", path.display()))?,
        None => std::env::current_dir().map_err(|error| format!("current directory: {error}"))?,
    };
    Ok(start
        .ancestors()
        .find(|path| path.join(".git").exists())
        .map(std::path::Path::to_path_buf))
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
        ConfigCommand::MigrateWorkflows { .. } => unreachable!("handled before repository loading"),
    }
}

fn run_workflow_migration(
    selected_repository: Option<&std::path::Path>,
    json: bool,
) -> Result<(), String> {
    let health = crate::worker::probe_health()?;
    if health.state != crate::worker::DaemonState::Stopped {
        crate::worker::shutdown()?;
    }

    let mut repositories = if let Some(path) = selected_repository {
        vec![Repository {
            root: workflow_repository_root(Some(path))?
                .ok_or_else(|| format!("not inside a Git repository: {}", path.display()))?,
        }]
    } else {
        workspace::discover_valid_entries(workspace::load_entries()?)
            .into_iter()
            .map(|entry| entry.repo)
            .collect::<Vec<_>>()
    };
    repositories.sort_by(|left, right| left.root.cmp(&right.root));
    repositories.dedup_by(|left, right| left.root == right.root);

    let database_paths = repositories
        .iter()
        .map(observability::db_path)
        .collect::<Vec<_>>();
    let ledger = crate::run::RunLedger::user()?;
    let report = crate::migration::import_repositories(&ledger, database_paths)?;
    emit_migrated_check_definition(&repositories)?;
    ledger.complete_cutover()?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "workflow cutover complete: {} imported, {} already imported across {} source database(s)",
            report.imported_runs,
            report.already_imported_runs,
            report.sources.len()
        );
        println!(
            "legacy runs are historical only; active runs must be restarted as new bundled workflows"
        );
    }
    Ok(())
}

fn emit_migrated_check_definition(repositories: &[Repository]) -> Result<(), String> {
    let Some(repository) = repositories.first() else {
        return Ok(());
    };
    let config = Config::load(repository);
    if !config.config_errors.is_empty() {
        return Err(format!(
            "legacy configuration cannot be migrated until these errors are fixed: {}",
            config.config_errors.join("; ")
        ));
    }
    let mut commands = Vec::new();
    for (group, values) in [
        ("pre-pr", &config.checks.pre_pr),
        ("pre-push", &config.checks.pre_push),
        ("review-fix", &config.checks.review_fix),
    ] {
        for (index, command) in values.iter().enumerate() {
            let argv = crate::process::parse_command_words(command).map_err(|error| {
                format!("legacy checks.{group}[{index}] is not representable as structured argv: {error}; rewrite it as a Workflow command Step")
            })?;
            if argv.is_empty() {
                return Err(format!(
                    "legacy checks.{group}[{index}] is empty and cannot be migrated"
                ));
            }
            commands.push((format!("{group}-{}", index + 1), argv));
        }
    }
    if commands.is_empty() {
        let mut paths = repositories
            .iter()
            .map(|repository| repository.prism_dir().join("config.toml"))
            .collect::<Vec<_>>();
        paths.push(config.user_path.clone());
        paths.sort();
        paths.dedup();
        for path in paths {
            migrate_legacy_config_file(&path)?;
        }
        return Ok(());
    }

    let mut output = String::from(
        "schema_version = 1\nname = \"migrated-checks\"\ndescription = \"Explicit structured command Steps migrated from legacy check settings.\"\nordered = true\ncapabilities = [\"repository_read\", \"process_execute\"]\n\n[inputs.task]\nartifact_type = \"builtin:task@1\"\n\n[budgets]\nmax_attempts = 32\nmax_fan_out = 1\nmax_child_depth = 0\nmax_mutations = 0\n",
    );
    for (id, argv) in commands {
        output.push_str("\n[[steps]]\n");
        output.push_str(&format!("id = {}\n", toml::Value::String(id)));
        output.push_str("class = \"action\"\nimplementation = \"builtin:command@1\"\ncapabilities = [\"repository_read\", \"process_execute\"]\n[steps.inputs.task]\nfrom = \"run.task\"\nartifact_type = \"builtin:task@1\"\n[steps.outputs.result]\nartifact_type = \"builtin:task@1\"\n[steps.settings]\ncommand = ");
        output.push_str(
            &toml::Value::Array(argv.into_iter().map(toml::Value::String).collect()).to_string(),
        );
        output.push('\n');
    }
    let directory = crate::util::prism_config_dir().join("workflows");
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create migrated Workflow directory: {error}"))?;
    crate::file_persistence::update(
        &directory.join("migrated-checks.toml"),
        crate::file_persistence::UpdateOptions::important_toml(),
        |_| Ok(((), Some(output.clone().into_bytes()))),
    )
    .map_err(|error| format!("write migrated Workflow definition: {error}"))?;

    let mut paths = repositories
        .iter()
        .map(|repository| repository.prism_dir().join("config.toml"))
        .collect::<Vec<_>>();
    paths.push(config.user_path.clone());
    paths.sort();
    paths.dedup();
    for path in paths {
        migrate_legacy_config_file(&path)?;
    }
    Ok(())
}

fn migrate_legacy_config_file(path: &std::path::Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    crate::file_persistence::update(
        path,
        crate::file_persistence::UpdateOptions::important_toml(),
        |contents| {
            let bytes = contents.as_bytes().unwrap_or_default();
            let text = std::str::from_utf8(bytes)
                .map_err(|error| Box::new(error) as crate::file_persistence::BoxError)?;
            let mut document = text
                .parse::<toml_edit::DocumentMut>()
                .map_err(|error| Box::new(error) as crate::file_persistence::BoxError)?;
            document.remove("auto");
            if let Some(checks) = document
                .get_mut("checks")
                .and_then(toml_edit::Item::as_table_mut)
            {
                checks.remove("pre_pr");
                checks.remove("pre_push");
                checks.remove("review_fix");
            }
            if document
                .get("checks")
                .and_then(toml_edit::Item::as_table)
                .is_some_and(toml_edit::Table::is_empty)
            {
                document.remove("checks");
            }
            let replacement = document.to_string();
            Ok(((), (replacement != text).then(|| replacement.into_bytes())))
        },
    )
    .map_err(|error| {
        format!(
            "remove migrated legacy keys from {}: {error}",
            path.display()
        )
    })
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

fn cutover_ledger() -> Result<Option<crate::run::RunLedger>, String> {
    let path = crate::util::prism_config_dir().join("workflow.db");
    if !path.exists() {
        return Ok(None);
    }
    let ledger = crate::run::RunLedger::open(path)?;
    ledger
        .cutover_complete()
        .map(|complete| complete.then_some(ledger))
}

fn resolve_generic_run(
    ledger: &crate::run::RunLedger,
    repository: Option<&std::path::Path>,
    selector: Option<&str>,
) -> Result<crate::run::RunId, String> {
    let runs = match repository {
        Some(repository) => ledger.list_for_repository(repository, 1000)?,
        None => ledger.list(1000)?,
    };
    match selector {
        None => runs
            .first()
            .map(|run| run.id.clone())
            .ok_or_else(|| "no Workflow Runs found".to_string()),
        Some(selector) => {
            let selector = selector.strip_prefix("run:").unwrap_or(selector);
            let matches = runs
                .iter()
                .filter(|run| run.id.as_str() == selector || run.id.as_str().starts_with(selector))
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [run] => Ok(run.id.clone()),
                [] => Err(format!("Workflow Run selector '{selector}' did not match")),
                _ => Err(format!("Workflow Run selector '{selector}' is ambiguous")),
            }
        }
    }
}

fn run_list_command(repo: Option<&std::path::Path>, options: InspectOptions) -> Result<(), String> {
    if let Some(ledger) = cutover_ledger()? {
        let runs = match repo {
            Some(repository) => {
                ledger.list_for_repository(repository, if options.all { 1000 } else { 200 })?
            }
            None => ledger.list(if options.all { 1000 } else { 200 })?,
        };
        if options.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({"schema_version":1,"runs":runs}))
                    .map_err(|error| error.to_string())?
            );
        } else {
            for run in runs {
                println!(
                    "{}  {}  {}",
                    run.id.as_str(),
                    run.state.label(),
                    run.definition
                );
            }
        }
        return Ok(());
    }
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
    if let Some(ledger) = cutover_ledger()? {
        let run_id = resolve_generic_run(&ledger, repo, options.selector.as_deref())?;
        let projection = ledger.inspect(&run_id)?;
        if options.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&projection).map_err(|error| error.to_string())?
            );
        } else {
            println!(
                "{}  {}  {}",
                projection.run.id.as_str(),
                projection.run.state.label(),
                projection.run.definition
            );
            for step in projection.steps {
                println!(
                    "  {}  {}  {}",
                    step.definition_step_id,
                    step.state.label(),
                    step.blocker.unwrap_or_default()
                );
            }
        }
        return Ok(());
    }
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
    if let Some(ledger) = cutover_ledger()? {
        let run_id = resolve_generic_run(&ledger, repo, selector.as_deref())?;
        let command = match action {
            ControlAction::Pause => crate::operations::WorkflowCommand::Pause(run_id.clone()),
            ControlAction::Resume => crate::operations::WorkflowCommand::Resume(run_id.clone()),
            ControlAction::Stop => crate::operations::WorkflowCommand::Cancel(run_id.clone()),
            ControlAction::Recover => return Err("generic recovery targets an exact Attempt; use `prism workflow recover-attempt <attempt-id>`".to_string()),
        };
        crate::operations::WorkflowOperations::new(ledger).execute(command)?;
        println!(
            "workflow = {}\nstate = {}",
            run_id.as_str(),
            match action {
                ControlAction::Pause => "pause_requested",
                ControlAction::Resume => "running",
                ControlAction::Stop => "cancel_requested",
                ControlAction::Recover => unreachable!(),
            }
        );
        return Ok(());
    }
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
    if let Some(ledger) = cutover_ledger()? {
        if selector.is_some() {
            return run_control_command(repo, ControlAction::Recover, selector);
        }
        let attention = ledger.attention(200)?;
        for run in attention.runs {
            println!(
                "{}  {}  {}",
                run.id.as_str(),
                run.state.label(),
                run.definition
            );
        }
        return Ok(());
    }
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
    let count = observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select count(*) from workflow_execution where dispatch_state = 'recovery_pending'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| format!("count interrupted workflows: {error}"))
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
    if !crate::run::RunLedger::user()?.cutover_complete()? {
        return Err("legacy TUI workflow actions are disabled; run `prism config migrate-workflows` before opening Prism".to_string());
    }
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

fn launch_bundled_plan(
    repo: &Repository,
    path: Option<&std::path::Path>,
    ledger: crate::run::RunLedger,
) -> Result<(), String> {
    let path = path.ok_or_else(|| {
        "after Workflow cutover, `prism run-plan` requires an explicit immutable Plan path; use `prism workflow launch builtin:plan` for other launch forms".to_string()
    })?;
    let task = crate::plan_artifact::PlanManifest::launch_task_from_file(path, None, 8)?;
    launch_bundled(repo, ledger, "builtin:plan", vec![task])
}

fn launch_bundled_coding(
    repo: &Repository,
    command: AutoCommand,
    ledger: crate::run::RunLedger,
) -> Result<(), String> {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "implementation_source".to_string(),
        serde_json::Value::String(
            match command.source {
                AutoCommandSource::Prompt => "prompt",
                AutoCommandSource::ExistingPlan => "existing_plan",
                AutoCommandSource::DraftPlan => "draft_plan",
                AutoCommandSource::ExistingPullRequest => "existing_change_request",
            }
            .to_string(),
        ),
    );
    if let Some(prompt) = command.prompt {
        payload.insert("task".to_string(), serde_json::Value::String(prompt));
    }
    if let Some(path) = command.plan_path {
        let content = std::fs::read_to_string(&path)
            .map_err(|error| format!("read immutable Plan input {}: {error}", path.display()))?;
        payload.insert("plan".to_string(), serde_json::Value::String(content));
        payload.insert(
            "plan_display".to_string(),
            serde_json::Value::String(path.display().to_string()),
        );
    }
    launch_bundled(
        repo,
        ledger,
        "builtin:coding",
        vec![crate::run::ArtifactInput {
            name: "task".to_string(),
            artifact_type: "builtin:task@1".to_string(),
            payload: serde_json::Value::Object(payload),
            trust: crate::run::TrustClass::Trusted,
            sensitivity: crate::run::Sensitivity::Internal,
        }],
    )
}

fn launch_bundled(
    repo: &Repository,
    ledger: crate::run::RunLedger,
    selector: &str,
    inputs: Vec<crate::run::ArtifactInput>,
) -> Result<(), String> {
    let snapshot = crate::definition::DefinitionCatalog::discover(Some(&repo.root))
        .resolve(selector)
        .map_err(format_workflow_diagnostics)?;
    let repository_id = ledger.repository_id(&repo.root)?;
    let run = ledger.start(crate::run::StartRun {
        actor_capabilities: snapshot.content.transitive_capabilities.clone(),
        snapshot,
        repository_id: Some(repository_id),
        inputs,
        idempotency_key: None,
        actor: current_workflow_actor(),
    })?;
    println!("workflow_run_id = {}", run.run_id.as_str());
    crate::worker::ensure_running()?;
    crate::worker::wake()
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
            println!(
                "workflow_db_path = {}",
                crate::run::RunLedger::user()?.path().display()
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
            match crate::run::RunLedger::user().and_then(|ledger| ledger.health()) {
                Ok(health) => {
                    println!("workflow_integrity_ok = {}", health.integrity_ok);
                    println!("workflow_active_leases = {}", health.active_leases);
                    println!("workflow_dangling_claims = {}", health.dangling_claims);
                    println!(
                        "workflow_quarantined_workspaces = {}",
                        health.quarantined_workspaces
                    );
                    println!("workflow_overdue_waits = {}", health.overdue_waits);
                    println!(
                        "workflow_recovery_required_attempts = {}",
                        health.recovery_required_attempts
                    );
                    println!(
                        "workflow_unresolved_effects = {}",
                        health.unresolved_effects
                    );
                    println!("workflow_enabled_triggers = {}", health.enabled_triggers);
                }
                Err(error) => println!("workflow_health_error = {error}"),
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
