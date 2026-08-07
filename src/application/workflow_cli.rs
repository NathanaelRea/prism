use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};

use crate::extension::{
    DescriptorRegistry, ExtensionClient, ExtensionOperations, HostLimits, NoHostOperations,
};
use crate::package::{
    PackageInstaller, PackageLock, PackageManifest, SourceLimits, SourceResolver, WorkingCopy,
};
use crate::resource::{DiscoveredResource, ResourceKind, ResourceScope, TrustStore, discover};
use crate::workflow::definition::{
    DefinitionAuthoringOperations, DefinitionCatalog, ExecutableResolution, LaunchMode,
    WorkflowDefinition, diagnose_source,
};
use crate::{ApprovalDecision, LaunchWorkflow, WorkflowCommand, WorkflowOperations};

const JSON_SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
struct JsonEnvelope<T> {
    schema_version: u32,
    kind: String,
    data: T,
}

#[derive(Serialize)]
struct ResourceView {
    id: String,
    scope: &'static str,
    path: PathBuf,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct SkillInstallation {
    skill_id: String,
    source: PathBuf,
    destination: PathBuf,
    sha256: String,
}

pub(crate) fn run_workflow(repo: Option<&Path>, arguments: &[String]) -> Result<(), String> {
    finish_family(
        "workflow",
        arguments,
        block_on(run_workflow_async(repo, arguments)),
    )
}

pub(crate) fn run_extension(repo: Option<&Path>, arguments: &[String]) -> Result<(), String> {
    finish_family(
        "extension",
        arguments,
        block_on(run_extension_async(repo, arguments)),
    )
}

pub(crate) fn run_package(repo: Option<&Path>, arguments: &[String]) -> Result<(), String> {
    finish_family("package", arguments, run_package_inner(repo, arguments))
}

fn run_package_inner(repo: Option<&Path>, arguments: &[String]) -> Result<(), String> {
    let context = ResourceContext::load(repo)?;
    let (arguments, json_output) = split_json(arguments);
    match arguments.first().map(String::as_str) {
        Some("new") => {
            let id = required(&arguments, 1, "package new requires <id>")?;
            let destination = arguments
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(id));
            if destination.exists() {
                return Err(format!(
                    "package destination {} already exists",
                    destination.display()
                ));
            }
            fs::create_dir_all(&destination).map_err(string_error)?;
            fs::write(destination.join("prism-package.toml"), format!("schema_version = 1\nid = \"{id}\"\nversion = \"0.1.0\"\nresources = []\nextensions = []\ndependencies = []\n")).map_err(string_error)?;
            output(
                json_output,
                "package.new",
                &json!({"id": id, "path": destination}),
                || format!("created {id} at {}", destination.display()),
            )
        }
        Some("validate") => {
            let path = PathBuf::from(required(&arguments, 1, "package validate requires <path>")?);
            let manifest = manifest_at(&path)?;
            output(
                json_output,
                "package.validation",
                &json!({"valid": true, "package": manifest}),
                || format!("valid: {} ({})", manifest.id, path.display()),
            )
        }
        Some("install") => {
            let source = required(&arguments, 1, "package install requires <source>")?;
            let resolved =
                SourceResolver::new(context.global.join("staging"), SourceLimits::default())
                    .resolve(source)
                    .map_err(|error| error.to_string())?;
            let installed = PackageInstaller::new(
                &context.global,
                context.global.join("state"),
                context.global.join("store"),
            )
            .install(&resolved, Some(target_triple()))
            .map_err(|error| error.to_string())?;
            output(
                json_output,
                "package.install",
                &json!({"id": installed.package_id, "revision": installed.revision.to_string(), "path": installed.working_copy}),
                || format!("installed {} {}", installed.package_id, installed.revision),
            )
        }
        Some("list") => {
            let lock = read_package_lock(&context.global)?;
            output(json_output, "package.list", &lock.packages, || {
                lock.packages
                    .iter()
                    .map(|package| format!("{}\t{}", package.id, package.revision))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        Some("show") => {
            let id = required(&arguments, 1, "package show requires <id>")?;
            let root = context.global.join("packages").join(id);
            let manifest = manifest_at(&root)?;
            output(json_output, "package.show", &manifest, || {
                fs::read_to_string(root.join("prism-package.toml")).unwrap_or_default()
            })
        }
        Some("update") => {
            let id = required(&arguments, 1, "package update requires <id> [source]")?;
            let mut lock = read_package_lock(&context.global)?;
            let locked = lock
                .packages
                .iter()
                .find(|package| package.id == id)
                .cloned()
                .ok_or_else(|| format!("package {id} is not installed"))?;
            let source = arguments
                .get(2)
                .map(String::as_str)
                .unwrap_or(&locked.source);
            if source.starts_with("embedded:") {
                if id != "prism.standard" || arguments.get(2).is_some() {
                    return Err(format!(
                        "package {id} uses embedded source {source}; provide an explicit update source"
                    ));
                }
                let updated = crate::package::bootstrap_standard_pack(&context.global)
                    .map_err(|error| error.to_string())?;
                return output(
                    json_output,
                    "package.update",
                    &json!({"id":id,"updated":updated,"source":source}),
                    || {
                        if updated {
                            "updated prism.standard from the bundled incoming revision".into()
                        } else {
                            "prism.standard is current or has preserved update conflicts".into()
                        }
                    },
                );
            }
            let resolved =
                SourceResolver::new(context.global.join("staging"), SourceLimits::default())
                    .resolve(source)
                    .map_err(|error| error.to_string())?;
            let working = WorkingCopy::new(
                context.global.join("packages").join(id),
                context.global.join("state/package-bases").join(id),
                context.global.join("state/package-updates").join(id),
            );
            let plan = working
                .plan_update(&resolved.root)
                .map_err(|error| error.to_string())?;
            let conflicts = plan.has_conflicts();
            working
                .apply_update(&resolved.root, &plan, |candidate| {
                    manifest_at(candidate).map(|_| ())
                })
                .map_err(|error| error.to_string())?;
            if !conflicts {
                let locked = lock
                    .packages
                    .iter_mut()
                    .find(|package| package.id == id)
                    .expect("installed package remains locked");
                locked.source = resolved.origin;
                locked.revision = resolved.revision;
                locked.sha256 = resolved
                    .digest
                    .as_str()
                    .trim_start_matches("sha256:")
                    .into();
                lock.validate().map_err(|error| error.to_string())?;
                write_package_lock(&context.global, &lock)?;
            }
            output(
                json_output,
                "package.update",
                &json!({"id": id, "conflicts": conflicts, "dirty": plan.dirty}),
                || {
                    if conflicts {
                        format!("update for {id} has conflicts; local files were preserved")
                    } else {
                        format!("updated {id}")
                    }
                },
            )
        }
        Some("remove") => {
            let id = required(&arguments, 1, "package remove requires <id>")?;
            PackageInstaller::new(
                &context.global,
                context.global.join("state"),
                context.global.join("store"),
            )
            .remove(id)
            .map_err(|error| error.to_string())?;
            output(
                json_output,
                "package.remove",
                &json!({"id": id, "removed": true}),
                || format!("removed {id}"),
            )
        }
        Some(other) => Err(format!("unknown package subcommand: {other}")),
        None => Err("package requires a subcommand".into()),
    }
}

async fn run_trigger_command(arguments: &[String], json_output: bool) -> Result<(), String> {
    let operations = WorkflowOperations::open_default()
        .await
        .map_err(|error| error.to_string())?;
    match arguments.first().map(String::as_str) {
        Some("list") => {
            let triggers = operations
                .list_triggers()
                .await
                .map_err(|error| error.to_string())?;
            output(json_output, "workflow.trigger.list", &triggers, || {
                triggers
                    .iter()
                    .map(|trigger| {
                        format!(
                            "{}\t{}",
                            trigger.id,
                            if trigger.enabled {
                                "enabled"
                            } else {
                                "disabled"
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        Some("show") => {
            let id = required(arguments, 1, "workflow trigger show requires <id>")?;
            let trigger = operations
                .show_trigger(id)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("unknown Trigger {id}"))?;
            output(json_output, "workflow.trigger.show", &trigger, || {
                serde_json::to_string_pretty(&trigger).unwrap_or_default()
            })
        }
        Some(command @ ("enable" | "disable")) => {
            let id = required(
                arguments,
                1,
                "workflow trigger enable/disable requires <id>",
            )?;
            let enabled = command == "enable";
            operations
                .set_trigger_enabled(id, enabled, now_ms())
                .await
                .map_err(|error| error.to_string())?;
            output(
                json_output,
                "workflow.trigger.control",
                &json!({"id": id, "enabled": enabled}),
                || format!("{id} {}", if enabled { "enabled" } else { "disabled" }),
            )
        }
        Some("run-now") => {
            let id = required(arguments, 1, "workflow trigger run-now requires <id>")?;
            let now = now_ms();
            let occurrence = format!("manual:{id}:{now}");
            let created = operations
                .run_trigger_now(id, &occurrence, now)
                .await
                .map_err(|error| error.to_string())?;
            output(
                json_output,
                "workflow.trigger.run_now",
                &json!({"id": id, "occurrence_id": occurrence, "created": created}),
                || {
                    format!(
                        "{} {occurrence}",
                        if created { "created" } else { "deduplicated" }
                    )
                },
            )
        }
        Some("history") => {
            let id = required(arguments, 1, "workflow trigger history requires <id>")?;
            let history = operations
                .trigger_history(id, 100)
                .await
                .map_err(|error| error.to_string())?;
            output(json_output, "workflow.trigger.history", &history, || {
                history
                    .iter()
                    .map(|item| format!("{}\t{:?}", item.id, item.status))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        Some("doctor") => {
            let diagnostics = operations
                .trigger_doctor(now_ms())
                .await
                .map_err(|error| error.to_string())?;
            output(json_output, "workflow.trigger.doctor", &diagnostics, || {
                if diagnostics.is_empty() {
                    "Triggers healthy".into()
                } else {
                    diagnostics
                        .iter()
                        .map(|item| {
                            format!("{}\t{}\t{}", item.trigger_id, item.severity, item.message)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            })
        }
        Some(other) => Err(format!("unknown workflow trigger subcommand: {other}")),
        None => Err("workflow trigger requires a subcommand".into()),
    }
}

pub(crate) fn run_skill(repo: Option<&Path>, arguments: &[String]) -> Result<(), String> {
    let result = ResourceContext::load(repo)
        .and_then(|context| run_copyable_resource(&context, arguments, ResourceKind::Skill));
    finish_family("skill", arguments, result)
}

pub(crate) fn run_template(repo: Option<&Path>, arguments: &[String]) -> Result<(), String> {
    let result = ResourceContext::load(repo)
        .and_then(|context| run_copyable_resource(&context, arguments, ResourceKind::Template));
    finish_family("template", arguments, result)
}

async fn run_workflow_async(repo: Option<&Path>, arguments: &[String]) -> Result<(), String> {
    let context = ResourceContext::load(repo)?;
    let (arguments, json_output) = split_json(arguments);
    match arguments.first().map(String::as_str) {
        Some("trigger") => run_trigger_command(&arguments[1..], json_output).await,
        Some("list") => {
            let resources = resource_views(&context, ResourceKind::Workflow)?;
            output(json_output, "workflow.list", &resources, || {
                resources
                    .iter()
                    .map(|item| format!("{}\t{}\t{}", item.id, item.scope, item.path.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        Some("show") => {
            let id = required(&arguments, 1, "workflow show requires <id>")?;
            let resource = find_resource(&context, ResourceKind::Workflow, id)?;
            let definition = parse_workflow(&resource.path)?;
            output(
                json_output,
                "workflow.show",
                &json!({"definition": definition, "path": resource.path, "scope": scope_name(resource.scope)}),
                || fs::read_to_string(&resource.path).unwrap_or_default(),
            )
        }
        Some("new") => {
            let id = required(&arguments, 1, "workflow new requires <id>")?;
            let name = option_value(&arguments, "--name")
                .unwrap_or_else(|| id.rsplit('/').next().unwrap_or(id));
            let path = DefinitionAuthoringOperations::new(context.global.join("workflows"))
                .create(id, name)
                .map_err(|error| error.to_string())?;
            output(
                json_output,
                "workflow.new",
                &json!({"id": id, "path": path}),
                || format!("created {id} at {}", path.display()),
            )
        }
        Some("copy") => {
            let source = required(&arguments, 1, "workflow copy requires <source-id> <new-id>")?;
            let new_id = required(&arguments, 2, "workflow copy requires <source-id> <new-id>")?;
            let source = find_resource(&context, ResourceKind::Workflow, source)?;
            let name = option_value(&arguments, "--name")
                .unwrap_or_else(|| new_id.rsplit('/').next().unwrap_or(new_id));
            let path = DefinitionAuthoringOperations::new(context.global.join("workflows"))
                .copy(&source.path, new_id, name)
                .map_err(|error| error.to_string())?;
            output(
                json_output,
                "workflow.copy",
                &json!({"id": new_id, "path": path}),
                || format!("copied {new_id} to {}", path.display()),
            )
        }
        Some("edit") => {
            let id = required(&arguments, 1, "workflow edit requires <id>")?;
            let resource = find_resource(&context, ResourceKind::Workflow, id)?;
            edit(&resource.path)
        }
        Some("validate") => {
            let target = required(&arguments, 1, "workflow validate requires <id-or-path>")?;
            let path = workflow_path(&context, target)?;
            match fs::read_to_string(&path)
                .map_err(string_error)
                .and_then(|source| {
                    diagnose_source(&path, &source).map_err(|diagnostics| {
                        diagnostics
                            .iter()
                            .map(|item| format!("{}: {}", item.path.display(), item.message))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                }) {
                Ok(definition) => output(
                    json_output,
                    "workflow.validation",
                    &json!({"valid": true, "id": definition.id, "path": path, "diagnostics": []}),
                    || format!("valid: {} ({})", definition.id, path.display()),
                ),
                Err(error) if json_output => output(
                    true,
                    "workflow.validation",
                    &json!({"valid": false, "path": path, "diagnostics": [error]}),
                    String::new,
                ),
                Err(error) => Err(error),
            }
        }
        Some("preview") => {
            let id = required(&arguments, 1, "workflow preview requires <id>")?;
            let catalog = load_catalog(&context).await?;
            let preview = catalog.preview(id).map_err(|error| error.to_string())?;
            output(json_output, "workflow.preview", &preview, || {
                serde_json::to_string_pretty(&preview).unwrap_or_default()
            })
        }
        Some("run") => {
            let id = required(&arguments, 1, "workflow run requires <id>")?;
            let catalog = load_catalog(&context).await?;
            let snapshot = catalog.compile(id).map_err(|error| error.to_string())?;
            if !snapshot.definition.launch.contains(&LaunchMode::Manual) {
                return Err(format!("workflow {id} does not allow manual launch"));
            }
            let inputs = typed_inputs(&arguments, &snapshot.definition.inputs, &snapshot.schemas)?;
            let operations = WorkflowOperations::open_default()
                .await
                .map_err(|error| error.to_string())?;
            // Discovery happens for every launch. Registration only pins the freshly compiled
            // filesystem state so this run remains reproducible after later edits.
            crate::register_catalog_snapshots(&operations, &catalog)
                .await
                .map_err(|error| error.to_string())?;
            let now = now_ms();
            let key = option_value(&arguments, "--idempotency-key")
                .map(str::to_owned)
                .unwrap_or_else(|| format!("cli:{id}:{now}"));
            let run_id = format!(
                "workflow-{:016x}-{now}",
                crate::util::stable_hash(Path::new(&key))
            );
            let input_json = serde_json::to_string(&inputs).map_err(string_error)?;
            let launched = operations
                .launch(LaunchWorkflow {
                    run_id: &run_id,
                    definition_snapshot_id: &snapshot.digest,
                    repository: context
                        .repository
                        .as_ref()
                        .map(|path| path.to_string_lossy())
                        .as_deref(),
                    idempotency_key: &key,
                    input_json: &input_json,
                    now_unix_ms: now,
                })
                .await
                .map_err(|error| error.to_string())?;
            output(
                json_output,
                "workflow.run",
                &json!({"run_id": launched, "definition_id": id, "definition_snapshot_id": snapshot.digest}),
                || format!("run_id = {launched}\nstatus = queued"),
            )
        }
        Some("history") => {
            let operations = WorkflowOperations::open_default()
                .await
                .map_err(|error| error.to_string())?;
            let runs = operations
                .list(
                    context
                        .repository
                        .as_ref()
                        .map(|path| path.to_string_lossy())
                        .as_deref(),
                    100,
                )
                .await
                .map_err(|error| error.to_string())?;
            let selected = if let Some(id) = arguments.get(1) {
                runs.into_iter()
                    .filter(|run| run.id == *id || run.definition_name == *id)
                    .collect()
            } else {
                runs
            };
            output(json_output, "workflow.history", &selected, || {
                selected
                    .iter()
                    .map(|run| format!("{}\t{}\t{}", run.id, run.definition_name, run.status))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        Some("migrate") => {
            let target = required(&arguments, 1, "workflow migrate requires <id-or-path>")?;
            let path = workflow_path(&context, target)?;
            let authoring = DefinitionAuthoringOperations::new(context.global.join("workflows"));
            let preview = authoring
                .migration_preview(&path)
                .map_err(|error| error.to_string())?;
            let backup = if arguments.iter().any(|value| value == "--apply") {
                authoring
                    .apply_migration(&preview)
                    .map_err(|error| error.to_string())?
            } else {
                None
            };
            output(
                json_output,
                "workflow.migration",
                &json!({"preview": preview, "applied": backup.is_some(), "backup": backup}),
                || {
                    if preview.changed {
                        format!(
                            "{} -> {}{}",
                            preview.from_version,
                            preview.to_version,
                            if backup.is_some() {
                                " (applied)"
                            } else {
                                " (preview)"
                            }
                        )
                    } else {
                        "already current".into()
                    }
                },
            )
        }
        Some("updates") => {
            let catalog = load_catalog(&context).await?;
            output(
                json_output,
                "workflow.updates",
                &catalog.updates(&BTreeMap::new()),
                || {
                    catalog
                        .list()
                        .iter()
                        .map(|item| format!("{}\t{}", item.id, item.revision))
                        .collect::<Vec<_>>()
                        .join("\n")
                },
            )
        }
        Some(command @ ("pause" | "resume" | "cancel" | "retry")) => {
            let run_id = required(&arguments, 1, "workflow control requires <run-id>")?;
            let command = match command {
                "pause" => WorkflowCommand::Pause,
                "resume" => WorkflowCommand::Resume,
                "cancel" => WorkflowCommand::Cancel,
                _ => WorkflowCommand::Retry,
            };
            WorkflowOperations::open_default()
                .await
                .map_err(|error| error.to_string())?
                .command(run_id, command, now_ms())
                .await
                .map_err(|error| error.to_string())?;
            output(
                json_output,
                "workflow.control",
                &json!({"run_id": run_id, "command": command}),
                || format!("{command:?} requested for {run_id}"),
            )
        }
        Some(command @ ("restart" | "skip")) => {
            let run_id = required(
                &arguments,
                1,
                "workflow restart|skip requires <run-id> <step>",
            )?;
            let step = required(
                &arguments,
                2,
                "workflow restart|skip requires <run-id> <step>",
            )?;
            let operations = WorkflowOperations::open_default()
                .await
                .map_err(|error| error.to_string())?;
            let projection = operations
                .inspect(run_id)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("unknown Workflow Run '{run_id}'"))?;
            let selected = projection
                .steps
                .iter()
                .find(|candidate| candidate.key == step)
                .ok_or_else(|| format!("unknown Step '{step}' in Workflow Run '{run_id}'"))?;
            if command == "skip" && !selected.skippable {
                return Err(format!("Step '{step}' is not marked skippable"));
            }
            let invalidated = downstream_steps(&projection.steps, step);
            let preview = arguments.iter().any(|value| value == "--preview");
            if !preview {
                if command == "restart" {
                    operations.restart_from_step(run_id, step, now_ms()).await
                } else {
                    operations.skip_step(run_id, step, now_ms()).await
                }
                .map_err(|error| error.to_string())?;
            }
            output(
                json_output,
                "workflow.step_control",
                &json!({"run_id": run_id, "step": step, "command": command, "preview": preview, "invalidated_steps": invalidated}),
                || {
                    format!(
                        "{command} {} for {run_id} from {step}\ninvalidates: {}",
                        if preview { "preview" } else { "requested" },
                        invalidated.join(", ")
                    )
                },
            )
        }
        Some(command @ ("approve" | "reject")) => {
            let approval = required(&arguments, 1, "workflow approval requires <approval-id>")?;
            let actor = option_value(&arguments, "--by").unwrap_or("cli");
            let note = option_value(&arguments, "--note");
            let decision = if command == "approve" {
                ApprovalDecision::Approve
            } else {
                ApprovalDecision::Reject
            };
            WorkflowOperations::open_default()
                .await
                .map_err(|error| error.to_string())?
                .decide_approval(approval, decision, actor, note, now_ms())
                .await
                .map_err(|error| error.to_string())?;
            output(
                json_output,
                "workflow.approval",
                &json!({"approval_id": approval, "decision": command, "decided_by": actor}),
                || format!("{command}d {approval}"),
            )
        }
        Some(other) => Err(format!("unknown workflow subcommand: {other}")),
        None => Err("workflow requires a subcommand".into()),
    }
}

async fn run_extension_async(repo: Option<&Path>, arguments: &[String]) -> Result<(), String> {
    let context = ResourceContext::load(repo)?;
    let (arguments, json_output) = split_json(arguments);
    let operations = ExtensionOperations::new(
        context.global.join("extensions"),
        context.global.join("state"),
    );
    match arguments.first().map(String::as_str) {
        Some("list") => {
            let resources = resource_views(&context, ResourceKind::Extension)?;
            output(json_output, "extension.list", &resources, || {
                resources
                    .iter()
                    .map(|item| format!("{}\t{}", item.id, item.path.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        Some("show") => {
            let id = required(&arguments, 1, "extension show requires <id>")?;
            let resource = find_resource(&context, ResourceKind::Extension, id)?;
            output(
                json_output,
                "extension.show",
                &json!({"id": id, "path": resource.path, "scope": scope_name(resource.scope)}),
                || resource.path.display().to_string(),
            )
        }
        Some("new") => {
            let id = required(&arguments, 1, "extension new requires <id>")?;
            let path = operations.scaffold(id).map_err(|error| error.to_string())?;
            output(
                json_output,
                "extension.new",
                &json!({"id": id, "path": path}),
                || format!("created {id} at {}", path.display()),
            )
        }
        Some("edit") => {
            let id = required(&arguments, 1, "extension edit requires <id>")?;
            let resource = find_resource(&context, ResourceKind::Extension, id)?;
            edit(&if resource.path.is_dir() {
                resource.path.join("src/main.rs")
            } else {
                resource.path
            })
        }
        Some("check") => {
            let path = PathBuf::from(required(&arguments, 1, "extension check requires <path>")?);
            operations.check(&path).map_err(|error| error.to_string())?;
            output(
                json_output,
                "extension.check",
                &json!({"path": path, "valid": true}),
                || "extension check passed".into(),
            )
        }
        Some("build") => {
            let id = required(&arguments, 1, "extension build requires <id> [path]")?;
            let path = arguments
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| context.global.join("extensions").join(id));
            let (revision, executable) = operations
                .build(id, &path)
                .map_err(|error| error.to_string())?;
            output(
                json_output,
                "extension.build",
                &json!({"id": id, "revision": revision.to_string(), "executable": executable}),
                || format!("built {id} {revision}"),
            )
        }
        Some("reload") => {
            let targets = extension_targets(&context, arguments.get(1), arguments.get(2))?;
            let mut results = Vec::new();
            for (id, path) in targets {
                let client = operations
                    .reload(&id, path, Arc::new(NoHostOperations), HostLimits::default())
                    .await
                    .map_err(|error| error.to_string())?;
                results.push(json!({"id": id, "revision": client.revision(), "diagnostics": client.diagnostics()}));
                client.shutdown().await.map_err(|error| error.to_string())?;
            }
            output(json_output, "extension.reload", &results, || {
                format!("reloaded {} extension(s)", results.len())
            })
        }
        Some("doctor") => {
            let targets = extension_targets(&context, arguments.get(1), arguments.get(2))?;
            let mut reports = Vec::new();
            for (id, path) in targets {
                let executable = if path.is_dir() {
                    operations
                        .build(&id, &path)
                        .map_err(|error| error.to_string())?
                        .1
                } else {
                    path
                };
                reports.push(
                    operations
                        .doctor(
                            &id,
                            executable,
                            Arc::new(NoHostOperations),
                            HostLimits::default(),
                        )
                        .await,
                );
            }
            output(json_output, "extension.doctor", &reports, || {
                reports
                    .iter()
                    .map(|report| {
                        format!(
                            "{}\t{}",
                            report.extension_id,
                            if report.healthy {
                                "healthy"
                            } else {
                                "unhealthy"
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        Some(other) => Err(format!("unknown extension subcommand: {other}")),
        None => Err("extension requires a subcommand".into()),
    }
}

struct ResourceContext {
    global: PathBuf,
    repository: Option<PathBuf>,
}

fn extension_targets(
    context: &ResourceContext,
    id: Option<&String>,
    explicit_path: Option<&String>,
) -> Result<Vec<(String, PathBuf)>, String> {
    if let Some(id) = id {
        let path = if let Some(path) = explicit_path {
            PathBuf::from(path)
        } else {
            find_resource(context, ResourceKind::Extension, id)?.path
        };
        return Ok(vec![(id.clone(), path)]);
    }
    resource_views(context, ResourceKind::Extension).map(|resources| {
        resources
            .into_iter()
            .map(|resource| (resource.id, resource.path))
            .collect()
    })
}

impl ResourceContext {
    fn load(repo: Option<&Path>) -> Result<Self, String> {
        let global = crate::util::prism_config_dir();
        crate::package::bootstrap_standard_pack(&global).map_err(|error| error.to_string())?;
        crate::resource::ensure_global_drop_in_directories(&global)
            .map_err(|error| error.to_string())?;
        let repository = match repo {
            Some(path) => Some(crate::repo::Repository::discover(Some(path))?.root),
            None => crate::repo::Repository::discover(None)
                .ok()
                .map(|repo| repo.root),
        };
        Ok(Self { global, repository })
    }

    fn repository_resources(&self) -> Option<PathBuf> {
        self.repository.as_ref().map(|root| root.join(".prism"))
    }

    fn resources(&self) -> Result<Vec<DiscoveredResource>, String> {
        discover(&self.global, self.repository_resources().as_deref())
            .map_err(|error| error.to_string())
    }
}

async fn load_catalog(context: &ResourceContext) -> Result<DefinitionCatalog, String> {
    let executable = standard_extension_executable(context);
    let client = ExtensionClient::launch(
        &executable,
        Arc::new(NoHostOperations),
        HostLimits::default(),
    )
    .await
    .map_err(|error| {
        format!(
            "load Standard Extension descriptor from {}: {error}",
            executable.display()
        )
    })?;
    let mut registry = DescriptorRegistry::default();
    registry
        .register(client.descriptor())
        .map_err(|error| error.to_string())?;
    let executable_revisions = client
        .descriptor()
        .implementations
        .iter()
        .map(|implementation| {
            (
                implementation.id.clone(),
                ExecutableResolution {
                    revision: client.revision().to_string(),
                    trusted: true,
                },
            )
        })
        .collect();
    let _ = client.shutdown().await;
    DefinitionCatalog::discover(
        &context.global,
        context.repository_resources().as_deref(),
        &TrustStore::new(context.global.join("trust.json")),
        registry,
        executable_revisions,
    )
    .map_err(|error| error.to_string())
}

fn standard_extension_executable(context: &ResourceContext) -> PathBuf {
    if let Some(path) = std::env::var_os("PRISM_STANDARD_EXTENSION") {
        return PathBuf::from(path);
    }
    if let Ok(current) = std::env::current_exe() {
        let direct = current.with_file_name("prism-standard-extension");
        if direct.is_file() {
            return direct;
        }
        if let Some(debug_root) = current.parent().and_then(Path::parent) {
            let workspace = debug_root.join("prism-standard-extension");
            if workspace.is_file() {
                return workspace;
            }
        }
    }
    context
        .global
        .join("packages/prism.standard/extensions/prism-standard-extension")
}

fn run_copyable_resource(
    context: &ResourceContext,
    arguments: &[String],
    kind: ResourceKind,
) -> Result<(), String> {
    let (arguments, json_output) = split_json(arguments);
    let family = kind_name(kind);
    match arguments.first().map(String::as_str) {
        Some("list") => {
            let resources = resource_views(context, kind)?;
            output(json_output, &format!("{family}.list"), &resources, || {
                resources
                    .iter()
                    .map(|item| format!("{}\t{}", item.id, item.path.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        Some("show") => {
            let id = required(&arguments, 1, &format!("{family} show requires <id>"))?;
            let resource = find_resource(context, kind, id)?;
            let content = fs::read_to_string(&resource.path).map_err(string_error)?;
            output(
                json_output,
                &format!("{family}.show"),
                &json!({"id": id, "path": resource.path, "content": content}),
                || content,
            )
        }
        Some("copy") if kind == ResourceKind::Template => {
            let id = required(&arguments, 1, "template copy requires <id> <destination>")?;
            let destination = PathBuf::from(required(
                &arguments,
                2,
                "template copy requires <id> <destination>",
            )?);
            if destination.exists() {
                return Err(format!(
                    "template destination {} already exists",
                    destination.display()
                ));
            }
            let resource = find_resource(context, kind, id)?;
            fs::copy(&resource.path, &destination).map_err(string_error)?;
            output(
                json_output,
                "template.copy",
                &json!({"id": id, "destination": destination}),
                || format!("copied {id} to {}", destination.display()),
            )
        }
        Some("install") if kind == ResourceKind::Skill => {
            install_skill(context, &arguments, json_output)
        }
        Some("remove") if kind == ResourceKind::Skill => {
            remove_skill(context, &arguments, json_output)
        }
        Some(other) => Err(format!("unknown {family} subcommand: {other}")),
        None => Err(format!("{family} requires a subcommand")),
    }
}

fn install_skill(
    context: &ResourceContext,
    arguments: &[String],
    json_output: bool,
) -> Result<(), String> {
    let id = required(arguments, 1, "skill install requires <id>")?;
    let harness = option_value(arguments, "--harness").unwrap_or("pi");
    if harness != "pi" {
        return Err(format!(
            "unsupported skill harness '{harness}'; expected pi"
        ));
    }
    let resource = find_resource(context, ResourceKind::Skill, id)?;
    let destination_root = std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| crate::util::home_dir().map(|home| home.join(".pi/agent")))
        .ok_or_else(|| "cannot locate Pi agent directory; set PI_CODING_AGENT_DIR".to_string())?;
    let destination = destination_root
        .join("skills")
        .join(id.replace(['/', '.'], "-"))
        .join("SKILL.md");
    if destination.exists() {
        return Err(format!(
            "skill destination {} already exists; Prism will not overwrite harness files",
            destination.display()
        ));
    }
    fs::create_dir_all(destination.parent().expect("skill parent")).map_err(string_error)?;
    let bytes = fs::read(&resource.path).map_err(string_error)?;
    fs::write(&destination, &bytes).map_err(string_error)?;
    let installation = SkillInstallation {
        skill_id: id.into(),
        source: resource.path,
        destination: destination.clone(),
        sha256: crate::resource::ContentRevision::digest(&bytes).to_string(),
    };
    let mut installations = read_skill_installations(context)?;
    installations.retain(|item| item.skill_id != id);
    installations.push(installation.clone());
    write_skill_installations(context, &installations)?;
    output(json_output, "skill.install", &installation, || {
        format!("installed {id} for Pi at {}", destination.display())
    })
}

fn remove_skill(
    context: &ResourceContext,
    arguments: &[String],
    json_output: bool,
) -> Result<(), String> {
    let id = required(arguments, 1, "skill remove requires <id>")?;
    let mut installations = read_skill_installations(context)?;
    let installation = installations
        .iter()
        .find(|item| item.skill_id == id)
        .cloned()
        .ok_or_else(|| format!("skill {id} is not installed by Prism"))?;
    let current = fs::read(&installation.destination).map_err(string_error)?;
    if crate::resource::ContentRevision::digest(&current).to_string() != installation.sha256 {
        return Err(format!(
            "installed skill {} was modified; refusing to remove it",
            installation.destination.display()
        ));
    }
    fs::remove_file(&installation.destination).map_err(string_error)?;
    installations.retain(|item| item.skill_id != id);
    write_skill_installations(context, &installations)?;
    output(
        json_output,
        "skill.remove",
        &json!({"id": id, "removed": true}),
        || format!("removed {id} from Pi"),
    )
}

fn read_skill_installations(context: &ResourceContext) -> Result<Vec<SkillInstallation>, String> {
    let path = context.global.join("state/skill-installations.json");
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(string_error),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.to_string()),
    }
}

fn write_skill_installations(
    context: &ResourceContext,
    installations: &[SkillInstallation],
) -> Result<(), String> {
    let path = context.global.join("state/skill-installations.json");
    fs::create_dir_all(path.parent().expect("installation parent")).map_err(string_error)?;
    fs::write(
        path,
        serde_json::to_vec_pretty(installations).map_err(string_error)?,
    )
    .map_err(string_error)
}

fn resource_views(
    context: &ResourceContext,
    kind: ResourceKind,
) -> Result<Vec<ResourceView>, String> {
    Ok(context
        .resources()?
        .into_iter()
        .filter(|resource| resource.kind == kind)
        .map(|resource| ResourceView {
            id: resource.identity.to_string(),
            scope: scope_name(resource.scope),
            path: resource.path,
        })
        .collect())
}

fn find_resource(
    context: &ResourceContext,
    kind: ResourceKind,
    id: &str,
) -> Result<DiscoveredResource, String> {
    context
        .resources()?
        .into_iter()
        .find(|resource| resource.kind == kind && resource.identity.as_str() == id)
        .ok_or_else(|| format!("unknown {} {id}", kind_name(kind)))
}

fn workflow_path(context: &ResourceContext, value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path.is_file() {
        Ok(path)
    } else {
        Ok(find_resource(context, ResourceKind::Workflow, value)?.path)
    }
}

fn parse_workflow(path: &Path) -> Result<WorkflowDefinition, String> {
    WorkflowDefinition::parse(&fs::read_to_string(path).map_err(string_error)?)
        .map_err(|error| error.to_string())
}

fn typed_inputs(
    arguments: &[String],
    ports: &BTreeMap<String, crate::PortDefinition>,
    schemas: &BTreeMap<String, Value>,
) -> Result<BTreeMap<String, Value>, String> {
    let mut values = BTreeMap::new();
    let mut index = 0;
    while index < arguments.len() {
        if arguments[index] == "--input" {
            let input = arguments
                .get(index + 1)
                .ok_or_else(|| "--input requires <name>=<json>".to_string())?;
            let (name, raw) = input
                .split_once('=')
                .ok_or_else(|| "--input requires <name>=<json>".to_string())?;
            if !ports.contains_key(name) {
                return Err(format!("workflow has no input named '{name}'"));
            }
            let value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.into()));
            if let Some(schema) = schemas.get(&ports[name].schema) {
                crate::workflow::schema::validate_value(&value, schema)
                    .map_err(|error| format!("workflow input '{name}': {error}"))?;
            }
            values.insert(name.into(), value);
            index += 2;
        } else {
            index += 1;
        }
    }
    let missing = ports
        .iter()
        .filter(|(name, port)| port.required && !values.contains_key(*name))
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "missing required workflow inputs: {}",
            missing.join(", ")
        ));
    }
    Ok(values)
}

fn edit(path: &Path) -> Result<(), String> {
    if !crate::terminal::stdin_is_tty() {
        return Err("interactive editing requires a TTY".into());
    }
    let argv = crate::terminal::editor_argv_from_env()?
        .ok_or_else(|| "no editor found; set VISUAL or EDITOR".to_string())?;
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .arg(path)
        .status()
        .map_err(string_error)?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("editor exited with {status}"))
    }
}

fn manifest_at(path: &Path) -> Result<PackageManifest, String> {
    let path = if path.is_dir() {
        path.join("prism-package.toml")
    } else {
        path.into()
    };
    PackageManifest::parse(&fs::read_to_string(&path).map_err(string_error)?)
        .map_err(|error| error.to_string())
}

fn read_package_lock(root: &Path) -> Result<PackageLock, String> {
    let path = root.join("package.lock");
    match fs::read_to_string(path) {
        Ok(source) => PackageLock::parse(&source).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            PackageLock::parse("schema_version = 1\npackages = []\n")
                .map_err(|error| error.to_string())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn write_package_lock(root: &Path, lock: &PackageLock) -> Result<(), String> {
    let path = root.join("package.lock");
    let candidate = root.join(format!(".package-lock-cli-{}.tmp", std::process::id()));
    fs::write(
        &candidate,
        toml::to_string_pretty(lock).map_err(string_error)?,
    )
    .map_err(string_error)?;
    fs::rename(&candidate, &path).map_err(string_error)
}

fn split_json(arguments: &[String]) -> (Vec<String>, bool) {
    (
        arguments
            .iter()
            .filter(|argument| argument.as_str() != "--json")
            .cloned()
            .collect(),
        arguments.iter().any(|argument| argument == "--json"),
    )
}

fn option_value<'a>(arguments: &'a [String], option: &str) -> Option<&'a str> {
    arguments
        .iter()
        .position(|argument| argument == option)
        .and_then(|index| arguments.get(index + 1))
        .map(String::as_str)
}

fn required<'a>(arguments: &'a [String], index: usize, message: &str) -> Result<&'a str, String> {
    arguments
        .get(index)
        .map(String::as_str)
        .ok_or_else(|| message.into())
}

fn output<T: Serialize>(
    json_output: bool,
    kind: &str,
    data: &T,
    text: impl FnOnce() -> String,
) -> Result<(), String> {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&JsonEnvelope {
                schema_version: JSON_SCHEMA_VERSION,
                kind: kind.into(),
                data
            })
            .map_err(string_error)?
        );
    } else {
        println!("{}", text());
    }
    Ok(())
}

fn finish_family(
    family: &str,
    arguments: &[String],
    result: Result<(), String>,
) -> Result<(), String> {
    if let Err(error) = &result
        && arguments.iter().any(|argument| argument == "--json")
    {
        let _ = output(
            true,
            &format!("{family}.error"),
            &json!({"message": error}),
            String::new,
        );
    }
    result
}

fn block_on<T>(future: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(string_error)?
        .block_on(future)
}

fn scope_name(scope: ResourceScope) -> &'static str {
    match scope {
        ResourceScope::Global => "global",
        ResourceScope::Repository => "repository",
    }
}
fn kind_name(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Workflow => "workflow",
        ResourceKind::Extension => "extension",
        ResourceKind::Skill => "skill",
        ResourceKind::Template => "template",
        ResourceKind::ArtifactSchema => "artifact_schema",
        ResourceKind::Trigger => "trigger",
        ResourceKind::Notification => "notification",
    }
}
fn downstream_steps(steps: &[crate::WorkflowStepProjection], selected: &str) -> Vec<String> {
    let mut invalidated = std::collections::BTreeSet::from([selected.to_string()]);
    loop {
        let before = invalidated.len();
        for step in steps {
            if step
                .dependencies
                .iter()
                .any(|dependency| invalidated.contains(dependency))
            {
                invalidated.insert(step.key.clone());
            }
        }
        if invalidated.len() == before {
            break;
        }
    }
    invalidated.into_iter().collect()
}
fn string_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|value| i64::try_from(value.as_millis()).ok())
        .unwrap_or(0)
}
fn target_triple() -> &'static str {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else {
        "x86_64-apple-darwin"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_inputs_reject_unknown_missing_and_wrong_schema_values() {
        let ports = BTreeMap::from([(
            "task".into(),
            crate::PortDefinition {
                schema: "acme.task/v1".into(),
                required: true,
                from_context: false,
                from: None,
            },
        )]);
        let schemas = BTreeMap::from([(
            "acme.task/v1".into(),
            json!({"type": "object", "required": ["title"]}),
        )]);
        assert!(typed_inputs(&[], &ports, &schemas).is_err());
        assert!(typed_inputs(&["--input".into(), "unknown={}".into()], &ports, &schemas).is_err());
        assert!(
            typed_inputs(
                &["--input".into(), "task=\"text\"".into()],
                &ports,
                &schemas
            )
            .is_err()
        );
        assert!(
            typed_inputs(
                &["--input".into(), "task={\"title\":\"work\"}".into()],
                &ports,
                &schemas
            )
            .is_ok()
        );
    }
}
