use std::collections::BTreeMap;
use std::fs;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::json;

use crate::resource::{DiscoveredResource, ResourceKind, ResourceScope, discover};

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
    crate::seed_editable_defaults(&context.global).map_err(|error| error.to_string())?;
    let (arguments, json_output) = split_json(arguments);
    let repository_resources = context.repository_resources();
    let repository_trusted = context.repository_resources_trusted()?;
    let discover = || {
        crate::PromptWorkflowCatalog::discover(
            &context.global,
            repository_resources.as_deref(),
            repository_trusted,
        )
        .map_err(format_workflow_diagnostics)
    };
    match arguments.first().map(String::as_str) {
        Some("trust-repository") => {
            let repository = context.repository.as_ref().ok_or_else(|| {
                "workflow trust-repository requires a repository; use --repo <path>".to_string()
            })?;
            let resources = repository_resources
                .as_ref()
                .expect("repository has resource root");
            let revision = crate::repository_resource_revision(resources)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| {
                    "repository has no Workflow or Trigger resources to trust".to_string()
                })?;
            let apply = arguments.iter().any(|argument| argument == "--apply");
            if apply {
                crate::trust_repository_resources(&context.global, repository, resources)
                    .map_err(|error| error.to_string())?;
            }
            output(
                json_output,
                "workflow.trust_repository",
                &json!({"repository": repository, "revision": revision.to_string(), "applied": apply}),
                || {
                    if apply {
                        format!("trusted repository Workflow resources at revision {revision}")
                    } else {
                        format!(
                            "Preview trust for {} at revision {revision}.\nRun again with --apply to allow its full-trust Workflow and Trigger resources.",
                            repository.display()
                        )
                    }
                },
            )
        }
        Some("list") => {
            let workflows = discover()?.list();
            output(json_output, "workflow.list", &workflows, || {
                workflows
                    .iter()
                    .map(|workflow| {
                        format!(
                            "{}\t{:?}\t{}",
                            workflow.name,
                            workflow.scope,
                            workflow.path.display()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        Some("show") => {
            let name = required(&arguments, 1, "workflow show requires <name>")?;
            let catalog = discover()?;
            let workflow = catalog
                .get(name)
                .ok_or_else(|| format!("unknown workflow '{name}'"))?;
            output(json_output, "workflow.show", workflow, || {
                workflow.source.clone()
            })
        }
        Some("validate") => {
            if let Some(target) = arguments.get(1) {
                let path = prompt_workflow_path(&context, target)?;
                let source = fs::read_to_string(&path).map_err(string_error)?;
                let triggers = crate::StepTriggerCatalog::discover(
                    &context.global,
                    repository_resources.as_deref(),
                    repository_trusted,
                )
                .map_err(|error| error.to_string())?;
                match crate::compile_workflow(&path, &source, &triggers) {
                    Ok(workflow) => output(
                        json_output,
                        "workflow.validation",
                        &json!({"valid": true, "name": workflow.name, "path": path, "diagnostics": []}),
                        || format!("valid: {} ({})", workflow.name, path.display()),
                    ),
                    Err(diagnostics) if json_output => output(
                        true,
                        "workflow.validation",
                        &json!({"valid": false, "path": path, "diagnostics": diagnostics}),
                        String::new,
                    ),
                    Err(diagnostics) => Err(format_workflow_diagnostics(diagnostics)),
                }
            } else {
                let workflows = discover()?.list();
                output(
                    json_output,
                    "workflow.validation",
                    &json!({"valid": true, "workflows": workflows, "diagnostics": []}),
                    || format!("valid: {} Workflow(s)", workflows.len()),
                )
            }
        }
        Some("new") => {
            let name = required(&arguments, 1, "workflow new requires <name>")?;
            validate_prompt_workflow_name(name)?;
            let destination = context
                .global
                .join("workflows")
                .join(format!("{name}.toml"));
            fs::create_dir_all(destination.parent().expect("Workflow parent"))
                .map_err(string_error)?;
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            use std::io::Write as _;
            let mut file = options.open(&destination).map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    format!("Workflow {} already exists", destination.display())
                } else {
                    error.to_string()
                }
            })?;
            file.write_all(crate::PROMPT_WORKFLOW_TEMPLATE.as_bytes())
                .map_err(string_error)?;
            output(
                json_output,
                "workflow.new",
                &json!({"name": name, "path": destination}),
                || format!("created {name} at {}", destination.display()),
            )
        }
        Some("edit") => {
            let name = required(&arguments, 1, "workflow edit requires <name>")?;
            let catalog = discover()?;
            let workflow = catalog
                .get(name)
                .ok_or_else(|| format!("unknown workflow '{name}'"))?;
            edit(&workflow.source_path)
        }
        Some("copy-example") => {
            let name = required(&arguments, 1, "workflow copy-example requires <name>")?;
            let destination = crate::copy_workflow_example(&context.global, name)
                .map_err(|error| error.to_string())?;
            output(
                json_output,
                "workflow.copy_example",
                &json!({"name": name, "path": destination}),
                || format!("copied {name} to {}", destination.display()),
            )
        }
        Some("reset") => {
            let name = required(&arguments, 1, "workflow reset requires <name>")?;
            if name != "stabilize" {
                return Err(format!("no bundled default named '{name}'"));
            }
            let destination = context.global.join("workflows/stabilize.toml");
            let apply = arguments.iter().any(|argument| argument == "--apply");
            if apply {
                fs::create_dir_all(destination.parent().expect("Workflow parent"))
                    .map_err(string_error)?;
                fs::write(
                    &destination,
                    crate::workflow::source::DEFAULT_STABILIZE_SOURCE,
                )
                .map_err(string_error)?;
            }
            output(
                json_output,
                "workflow.reset",
                &json!({"name": name, "path": destination, "applied": apply, "source": crate::workflow::source::DEFAULT_STABILIZE_SOURCE}),
                || {
                    if apply {
                        format!("reset {name} at {}", destination.display())
                    } else {
                        format!(
                            "Preview reset for {}:\n{}\nRun again with --apply to replace the file.",
                            destination.display(),
                            crate::workflow::source::DEFAULT_STABILIZE_SOURCE
                        )
                    }
                },
            )
        }
        Some("run") => {
            let name = required(&arguments, 1, "workflow run requires <name>")?;
            let catalog = discover()?;
            let workflow = catalog
                .get(name)
                .cloned()
                .ok_or_else(|| format!("unknown workflow '{name}'"))?;
            if workflow.steps.iter().any(|step| {
                step.trigger
                    .as_ref()
                    .is_some_and(|trigger| trigger.executable.is_some())
            }) {
                eprintln!(
                    "warning: this Workflow runs full-trust Trigger executables with your OS-user authority"
                );
            }
            let repository_root = context.repository.as_ref().ok_or_else(|| {
                "workflow run requires a repository; use --repo <path>".to_string()
            })?;
            let repository = crate::repo::Repository {
                root: repository_root.clone(),
            };
            let config = crate::config::Config::load(&repository);
            let mut workflow = workflow;
            crate::resolve_workflow_agent_selection(&mut workflow, &config)
                .map_err(format_workflow_diagnostics)?;
            let launch_arguments = parse_workflow_run_arguments(&arguments[2..])?;
            let worktree = launch_arguments
                .worktree
                .unwrap_or(std::env::current_dir().map_err(string_error)?);
            let worktree = if worktree.exists() {
                worktree.canonicalize().map_err(string_error)?
            } else {
                return Err(format!("worktree {} does not exist", worktree.display()));
            };
            let workflow = bind_launch_inputs(
                &workflow,
                launch_arguments.inputs,
                &worktree,
                &config,
                json_output,
            )?;
            let cached_subject = prompt_change_request_subject(&repository, &config, &worktree);
            let change_request = launch_arguments.change_request.or(cached_subject.0);
            let change_request_head = launch_arguments.change_request_head.or(cached_subject.1);
            let now = now_ms();
            let run_id = format!(
                "run-{:016x}-{now}",
                crate::util::stable_hash(Path::new(&format!(
                    "{}:{}:{}",
                    workflow.digest,
                    worktree.display(),
                    now
                )))
            );
            let subject = crate::TriggerSubject {
                repository: repository_root.clone(),
                worktree,
                change_request,
                change_request_head,
            };
            let launched = crate::worker::launch_prompt_workflow(&workflow, &run_id, &subject)?;
            output(
                json_output,
                "workflow.run",
                &json!({"run_id": launched, "workflow": name, "inputs": workflow.input_values, "status": "queued"}),
                || format!("run_id = {launched}\nstatus = queued"),
            )
        }
        Some("history") => {
            crate::worker::ensure_running()?;
            let selected = if let Some(run_id) = arguments.get(1) {
                vec![
                    crate::worker::inspect_prompt_workflow(run_id)?
                        .ok_or_else(|| format!("unknown Workflow Run {run_id}"))?,
                ]
            } else {
                crate::worker::list_prompt_workflows(context.repository.as_deref(), 100)?
            };
            output(json_output, "workflow.history", &selected, || {
                selected
                    .iter()
                    .map(|run| format!("{}\t{}\t{:?}", run.id, run.workflow_name, run.status))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        Some(command @ ("pause" | "resume" | "cancel" | "retry")) => {
            let run_id = required(&arguments, 1, "workflow control requires <run-id>")?;
            let command_value = match command {
                "pause" => crate::worker::PromptWorkflowControl::Pause,
                "resume" => crate::worker::PromptWorkflowControl::Resume,
                "cancel" => crate::worker::PromptWorkflowControl::Cancel,
                _ => crate::worker::PromptWorkflowControl::Retry,
            };
            crate::worker::command_prompt_workflow(run_id, command_value)?;
            output(
                json_output,
                "workflow.control",
                &json!({"run_id": run_id, "command": command}),
                || format!("{command} requested for {run_id}"),
            )
        }
        Some(other) => Err(format!("unknown workflow subcommand: {other}")),
        None => Err("workflow requires a subcommand".into()),
    }
}

fn format_workflow_diagnostics(diagnostics: Vec<crate::WorkflowDiagnostic>) -> String {
    diagnostics
        .into_iter()
        .map(|diagnostic| {
            let location = diagnostic.byte_start.map_or_else(
                || diagnostic.path.display().to_string(),
                |start| format!("{}:{start}", diagnostic.path.display()),
            );
            format!("{location}: {}", diagnostic.message)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn validate_prompt_workflow_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Err("Workflow name must use letters, numbers, '.', '_' or '-'".into())
    } else {
        Ok(())
    }
}

fn prompt_workflow_path(context: &ResourceContext, target: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(target);
    if path.is_file() {
        return Ok(path);
    }
    let catalog = crate::PromptWorkflowCatalog::discover(
        &context.global,
        context.repository_resources().as_deref(),
        context.repository_resources_trusted()?,
    )
    .map_err(format_workflow_diagnostics)?;
    catalog
        .get(target)
        .map(|workflow| workflow.source_path.clone())
        .ok_or_else(|| format!("unknown workflow '{target}'"))
}

fn prompt_change_request_subject(
    repository: &crate::repo::Repository,
    config: &crate::config::Config,
    worktree: &Path,
) -> (Option<String>, Option<String>) {
    let Some(branch) = crate::git::current_branch_name(worktree, config)
        .ok()
        .flatten()
    else {
        return (None, None);
    };
    let cache = crate::remote::load_pr_cache(repository, &branch);
    let Some(summary) = cache.summary() else {
        return (None, None);
    };
    let Some(identity) = summary.change_request_identity.as_ref() else {
        return (None, None);
    };
    (
        Some(format!(
            "{}:{}:{}:change_request:{}",
            identity.provider().config_label(),
            identity.canonical_host(),
            identity.project_path(),
            identity.native_id()
        )),
        (!summary.head_sha.is_empty()).then(|| summary.head_sha.clone()),
    )
}

struct ResourceContext {
    global: PathBuf,
    repository: Option<PathBuf>,
}

impl ResourceContext {
    fn load(repo: Option<&Path>) -> Result<Self, String> {
        let global = crate::util::prism_config_dir();
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

    fn repository_resources_trusted(&self) -> Result<bool, String> {
        match (self.repository.as_deref(), self.repository_resources()) {
            (Some(repository), Some(resources)) => {
                crate::repository_resources_are_trusted(&self.global, repository, &resources)
                    .map_err(|error| error.to_string())
            }
            _ => Ok(false),
        }
    }

    fn resources(&self) -> Result<Vec<DiscoveredResource>, String> {
        discover(&self.global, self.repository_resources().as_deref())
            .map_err(|error| error.to_string())
    }
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

#[derive(Debug, Default)]
struct WorkflowRunArguments {
    worktree: Option<PathBuf>,
    inputs: BTreeMap<String, String>,
    change_request: Option<String>,
    change_request_head: Option<String>,
}

fn parse_workflow_run_arguments(arguments: &[String]) -> Result<WorkflowRunArguments, String> {
    let mut parsed = WorkflowRunArguments::default();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--worktree" => {
                let value = arguments
                    .get(index + 1)
                    .ok_or_else(|| "--worktree requires a path".to_string())?;
                if parsed.worktree.replace(PathBuf::from(value)).is_some() {
                    return Err("--worktree may be specified only once".into());
                }
                index += 2;
            }
            "--change-request" => {
                let value = arguments
                    .get(index + 1)
                    .ok_or_else(|| "--change-request requires an identity".to_string())?;
                if parsed.change_request.replace(value.clone()).is_some() {
                    return Err("--change-request may be specified only once".into());
                }
                index += 2;
            }
            "--change-request-head" => {
                let value = arguments
                    .get(index + 1)
                    .ok_or_else(|| "--change-request-head requires a commit".to_string())?;
                if parsed.change_request_head.replace(value.clone()).is_some() {
                    return Err("--change-request-head may be specified only once".into());
                }
                index += 2;
            }
            "--input" => {
                let assignment = arguments
                    .get(index + 1)
                    .ok_or_else(|| "--input requires <name>=<value>".to_string())?;
                let (name, value) = assignment
                    .split_once('=')
                    .ok_or_else(|| "--input requires <name>=<value>".to_string())?;
                if name.is_empty() || value.is_empty() {
                    return Err("--input requires non-empty <name>=<value>".into());
                }
                if parsed.inputs.insert(name.into(), value.into()).is_some() {
                    return Err(format!(
                        "Workflow input '{name}' was provided more than once"
                    ));
                }
                index += 2;
            }
            argument => return Err(format!("unknown workflow run argument: {argument}")),
        }
    }
    Ok(parsed)
}

fn bind_launch_inputs(
    workflow: &crate::CompiledWorkflow,
    mut supplied: BTreeMap<String, String>,
    worktree: &Path,
    config: &crate::config::Config,
    json_output: bool,
) -> Result<crate::CompiledWorkflow, String> {
    for (name, input) in &workflow.inputs {
        if supplied.contains_key(name) || input.default_value().is_some() {
            continue;
        }
        if json_output || !std::io::stdin().is_terminal() {
            return Err(format!(
                "missing required Workflow input '{name}' ({type_name}); pass --input {name}=<value>",
                type_name = input.type_name()
            ));
        }
        let value = select_workflow_input(&config.tool("fzf"), name, worktree, input)?;
        supplied.insert(name.clone(), value);
    }
    crate::bind_workflow_inputs(workflow, worktree, &supplied).map_err(|error| error.to_string())
}

fn select_workflow_input(
    fzf: &str,
    name: &str,
    worktree: &Path,
    input: &crate::CompiledWorkflowInput,
) -> Result<String, String> {
    match input {
        crate::CompiledWorkflowInput::File { glob, .. } => {
            let candidates = crate::workflow_file_input_candidates(worktree, input)
                .map_err(|error| error.to_string())?;
            if candidates.is_empty() {
                return Err(format!(
                    "Workflow input '{name}' found no files matching '{glob}' under {}",
                    worktree.display()
                ));
            }
            select_fzf_value(
                fzf,
                name,
                &format!("Select a file matching {glob}"),
                &candidates,
            )
        }
        crate::CompiledWorkflowInput::Enum { options, .. } => {
            select_fzf_value(fzf, name, "Select one option", options)
        }
        crate::CompiledWorkflowInput::Bool { .. } => select_fzf_value(
            fzf,
            name,
            "Select true or false",
            &["true".into(), "false".into()],
        ),
        crate::CompiledWorkflowInput::String { .. } => {
            prompt_typed_workflow_input(name, "text", worktree, input)
        }
        crate::CompiledWorkflowInput::Number { .. } => {
            prompt_typed_workflow_input(name, "number", worktree, input)
        }
    }
}

fn select_fzf_value(
    fzf: &str,
    name: &str,
    header: &str,
    candidates: &[String],
) -> Result<String, String> {
    let mut child = Command::new(fzf)
        .args([
            &format!("--prompt={name}> "),
            &format!("--header={header}"),
            "--height=80%",
            "--reverse",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start fzf '{fzf}' for Workflow input '{name}': {error}"))?;
    {
        let stdin = child.stdin.as_mut().expect("fzf stdin is piped");
        for candidate in candidates {
            writeln!(stdin, "{candidate}")
                .map_err(|error| format!("write Workflow input candidates: {error}"))?;
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for Workflow input picker: {error}"))?;
    if !output.status.success() {
        return Err(format!("Workflow input '{name}' selection cancelled"));
    }
    let selected = String::from_utf8(output.stdout)
        .map_err(|error| format!("Workflow input picker returned invalid UTF-8: {error}"))?;
    let selected = selected.trim_end_matches(['\r', '\n']);
    if selected.is_empty() {
        return Err(format!("Workflow input '{name}' selection was empty"));
    }
    Ok(selected.to_string())
}

fn prompt_typed_workflow_input(
    name: &str,
    type_name: &str,
    worktree: &Path,
    input: &crate::CompiledWorkflowInput,
) -> Result<String, String> {
    loop {
        eprint!("Workflow input {name} ({type_name}): ");
        std::io::stderr()
            .flush()
            .map_err(|error| format!("flush Workflow input prompt: {error}"))?;
        let mut value = String::new();
        std::io::stdin()
            .read_line(&mut value)
            .map_err(|error| format!("read Workflow input '{name}': {error}"))?;
        if value.is_empty() {
            return Err(format!("Workflow input '{name}' reached end of input"));
        }
        let value = value.trim_end_matches(['\r', '\n']);
        match crate::validate_workflow_input(worktree, input, value) {
            Ok(value) => return Ok(value),
            Err(problem) => eprintln!("Invalid value for {name}: {problem}"),
        }
    }
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
        ResourceKind::Skill => "skill",
        ResourceKind::Template => "template",
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_run_arguments_accept_repeatable_named_inputs() {
        let arguments = [
            "--input",
            "plan=plan-workflows.md",
            "--worktree",
            "/repo/wt",
            "--change-request",
            "github:github.com:example/repo:change_request:PR_42",
            "--change-request-head",
            "abc123",
            "--input",
            "publish=false",
        ]
        .map(str::to_string);
        let parsed = parse_workflow_run_arguments(&arguments).unwrap();
        assert_eq!(parsed.worktree, Some(PathBuf::from("/repo/wt")));
        assert_eq!(parsed.inputs["plan"], "plan-workflows.md");
        assert_eq!(parsed.inputs["publish"], "false");
        assert_eq!(
            parsed.change_request.as_deref(),
            Some("github:github.com:example/repo:change_request:PR_42")
        );
        assert_eq!(parsed.change_request_head.as_deref(), Some("abc123"));
    }

    #[test]
    fn workflow_run_arguments_reject_duplicates_and_unknown_flags() {
        let duplicate = ["--input", "plan=a.md", "--input", "plan=b.md"].map(str::to_string);
        assert!(
            parse_workflow_run_arguments(&duplicate)
                .unwrap_err()
                .contains("more than once")
        );
        assert!(
            parse_workflow_run_arguments(&["--unknown".to_string()])
                .unwrap_err()
                .contains("unknown")
        );
    }
}
