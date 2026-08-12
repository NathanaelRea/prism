//! AI-authored, Worktree Session-scoped one-off Workflow drafts.

use std::fs;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::agent_phase::{
    AgentCancellation, AgentExecutor as _, AgentRequest, HarnessAgentExecutor,
};
use super::source::{CompiledWorkflow, TriggerCatalog, compile_workflow};

const MAX_DESCRIPTION_BYTES: usize = 16 * 1024;
const MAX_SOURCE_BYTES: usize = 128 * 1024;
const MAX_GENERATED_STEPS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OneOffWorkflowDraft {
    pub name: String,
    pub description: String,
    pub source: String,
    pub worktree: PathBuf,
    pub worktree_incarnation: String,
    pub updated_unix_ms: i64,
}

pub(crate) fn draft_path(
    repository: &crate::repo::Repository,
    worktree: &Path,
    incarnation: &str,
) -> PathBuf {
    let identity = format!("{}:{incarnation}", worktree.display());
    repository.prism_dir().join("workflow-drafts").join(format!(
        "{:016x}.json",
        crate::util::stable_hash(Path::new(&identity))
    ))
}

pub(crate) fn load_draft(path: &Path) -> Result<Option<OneOffWorkflowDraft>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| format!("read one-off Workflow draft {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "read one-off Workflow draft {}: {error}",
            path.display()
        )),
    }
}

pub(crate) fn remove_drafts_for_worktree(
    repository: &crate::repo::Repository,
    worktree: &Path,
) -> Result<(), String> {
    let directory = repository.prism_dir().join("workflow-drafts");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("list one-off Workflow drafts: {error}")),
    };
    for entry in entries {
        let entry = entry.map_err(|error| format!("list one-off Workflow drafts: {error}"))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "read one-off Workflow draft {}: {error}",
                    path.display()
                ));
            }
        };
        let Ok(draft) = serde_json::from_slice::<OneOffWorkflowDraft>(&bytes) else {
            continue;
        };
        if draft.worktree == worktree {
            fs::remove_file(&path).map_err(|error| {
                format!("remove one-off Workflow draft {}: {error}", path.display())
            })?;
        }
    }
    Ok(())
}

pub(crate) fn save_draft(path: &Path, draft: &OneOffWorkflowDraft) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("draft path {} has no parent", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("create one-off Workflow draft directory: {error}"))?;
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(draft)
        .map_err(|error| format!("serialize one-off Workflow draft: {error}"))?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    use std::io::Write as _;
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("write one-off Workflow draft: {error}"))?;
    file.write_all(&bytes)
        .map_err(|error| format!("write one-off Workflow draft: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("sync one-off Workflow draft: {error}"))?;
    fs::rename(&temporary, path)
        .map_err(|error| format!("commit one-off Workflow draft: {error}"))?;
    let mut permissions = fs::metadata(path)
        .map_err(|error| format!("inspect one-off Workflow draft: {error}"))?
        .permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions)
        .map_err(|error| format!("secure one-off Workflow draft: {error}"))
}

pub(crate) async fn generate_source(
    repository: &crate::repo::Repository,
    config: &crate::config::Config,
    worktree: &Path,
    description: &str,
    previous_error: Option<&str>,
    cancellation: AgentCancellation,
) -> Result<String, String> {
    let description = description.trim();
    if description.is_empty() {
        return Err("describe the one-off Workflow before generating it".into());
    }
    if description.len() > MAX_DESCRIPTION_BYTES {
        return Err(format!(
            "Workflow description exceeds {MAX_DESCRIPTION_BYTES} bytes"
        ));
    }
    let prompt = generator_prompt(description, previous_error);
    let harness = config
        .workflow_ai
        .harness
        .clone()
        .unwrap_or_else(|| config.default_harness.clone());
    let outcome = HarnessAgentExecutor {
        timeout: std::time::Duration::from_secs(5 * 60),
        stdout_bytes: MAX_SOURCE_BYTES * 2,
        stderr_bytes: 256 * 1024,
    }
    .execute(AgentRequest {
        run_id: "workflow-ai".into(),
        step_key: "generate".into(),
        attempt_id: format!("draft-{}", crate::workflow::prompt_worker::now_unix_ms()),
        repository: repository.root.clone(),
        worktree: worktree.to_path_buf(),
        harness: Some(harness),
        model: config.workflow_ai.model.clone(),
        variant: config.workflow_ai.variant.clone(),
        prompt,
        resume_session_id: None,
        require_resumable_session: false,
        cancellation,
    })
    .await
    .map_err(|error| format!("generate one-off Workflow: {error}"))?;
    normalize_generated_source(&outcome.final_text)
}

pub(crate) fn compile_generated(
    name: &str,
    source: &str,
    triggers: &TriggerCatalog,
) -> Result<CompiledWorkflow, String> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(format!(
            "generated Workflow exceeds {MAX_SOURCE_BYTES} bytes"
        ));
    }
    let path = PathBuf::from(format!("{name}.toml"));
    let workflow = compile_workflow(&path, source, triggers).map_err(|diagnostics| {
        diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    if !workflow.inputs.is_empty() {
        return Err("AI-created one-off Workflows cannot declare launch inputs".into());
    }
    if workflow.steps.len() > MAX_GENERATED_STEPS {
        return Err(format!(
            "generated Workflow has {} Steps; the one-off limit is {MAX_GENERATED_STEPS}",
            workflow.steps.len()
        ));
    }
    if workflow.steps.iter().any(|step| step.trigger.is_some()) {
        return Err("AI-created one-off Workflows cannot use full-trust Triggers".into());
    }
    if workflow.steps.iter().any(|step| {
        step.agent.harness.is_some() || step.agent.model.is_some() || step.agent.variant.is_some()
    }) {
        return Err(
            "AI-created one-off Workflows cannot override harness, model, or variant".into(),
        );
    }
    if workflow.steps.iter().any(|step| !step.followups.is_empty()) {
        return Err("AI-created one-off Workflows cannot use followups".into());
    }
    Ok(workflow)
}

fn normalize_generated_source(output: &str) -> Result<String, String> {
    let trimmed = output.trim();
    let source = if let Some(body) = trimmed.strip_prefix("```toml") {
        body.strip_suffix("```").map(str::trim)
    } else if let Some(body) = trimmed.strip_prefix("```") {
        body.strip_suffix("```").map(str::trim)
    } else {
        Some(trimmed)
    }
    .ok_or_else(|| "generator returned an unterminated Markdown fence".to_string())?;
    if source.is_empty() {
        return Err("generator returned an empty Workflow".into());
    }
    if source.len() > MAX_SOURCE_BYTES {
        return Err(format!(
            "generated Workflow exceeds {MAX_SOURCE_BYTES} bytes"
        ));
    }
    Ok(format!("{}\n", source.trim_end()))
}

fn generator_prompt(description: &str, previous_error: Option<&str>) -> String {
    let schema = crate::workflow::source::prompt_workflow_schema();
    let correction = previous_error.map_or(String::new(), |error| {
        format!("\nThe prior draft failed validation. Correct this error:\n{error}\n")
    });
    format!(
        "You generate one Prism prompt Workflow as TOML. Return only TOML, with no Markdown fence or explanation.\n\
Use [[step]] nodes. A plain list is serial. For parallel waves, give every node a stable id, use depends_on = [] for roots, give concurrent nodes the same predecessor dependencies, and make joins depend on every branch. Use context only for explicit ancestor IDs whose final messages are needed.\n\
Do not use trigger, inputs, harness, model, or followups. Keep prompts concrete and tell the final join to reconcile concurrent edits and run repository checks. Parallel agents intentionally share one worktree, so partition their files or responsibilities where practical. Use at most {MAX_GENERATED_STEPS} Steps.\n\
The accepted JSON Schema is:\n{schema}\n{correction}\nUser description:\n{description}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fenced_toml_is_normalized() {
        assert_eq!(
            normalize_generated_source("```toml\n[[step]]\nprompt='x'\n```\n").unwrap(),
            "[[step]]\nprompt='x'\n"
        );
    }

    #[test]
    fn draft_is_scoped_to_worktree_incarnation() {
        let repository = crate::repo::Repository {
            root: PathBuf::from("/repo"),
        };
        assert_ne!(
            draft_path(&repository, Path::new("/repo/wt"), "first"),
            draft_path(&repository, Path::new("/repo/wt"), "replacement")
        );
    }

    #[test]
    fn generated_triggers_are_rejected() {
        let error = compile_generated(
            "ai-test",
            "[[step]]\ntrigger='ready_to_merge'\n",
            &TriggerCatalog::builtins(),
        )
        .unwrap_err();
        assert!(error.contains("cannot use full-trust Triggers"));
    }

    #[test]
    fn generated_agent_overrides_and_followups_are_rejected() {
        for field in ["harness='opencode'", "model='test-model'", "variant='high'"] {
            let source = format!("[[step]]\nprompt='work'\n{field}\n");
            let error =
                compile_generated("ai-test", &source, &TriggerCatalog::builtins()).unwrap_err();
            assert!(error.contains("cannot override harness, model, or variant"));
        }

        let error = compile_generated(
            "ai-test",
            "[[step]]\nprompt='work'\nfollowups=['continue']\n",
            &TriggerCatalog::builtins(),
        )
        .unwrap_err();
        assert!(error.contains("cannot use followups"));
    }

    #[test]
    fn corrupt_unrelated_draft_does_not_block_worktree_cleanup() {
        let root = std::env::temp_dir().join(format!(
            "prism-workflow-draft-cleanup-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let repository = crate::repo::Repository { root: root.clone() };
        let target_worktree = root.join("target");
        let target_path = draft_path(&repository, &target_worktree, "target-incarnation");
        save_draft(
            &target_path,
            &OneOffWorkflowDraft {
                name: "target".into(),
                description: "target".into(),
                source: "[[step]]\nprompt='work'\n".into(),
                worktree: target_worktree.clone(),
                worktree_incarnation: "target-incarnation".into(),
                updated_unix_ms: 1,
            },
        )
        .unwrap();
        let corrupt_path = target_path.with_file_name("corrupt.json");
        fs::write(&corrupt_path, b"not json").unwrap();

        remove_drafts_for_worktree(&repository, &target_worktree).unwrap();

        assert!(!target_path.exists());
        assert!(corrupt_path.exists());
        fs::remove_dir_all(target_path.parent().unwrap()).unwrap();
    }
}
