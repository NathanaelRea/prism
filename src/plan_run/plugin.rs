use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanExecutorConfig {
    pub harness_id: String,
    pub harness_config: crate::harness::HarnessConfig,
    pub server_url: Option<String>,
    pub scope_path: PathBuf,
    pub title_prefix: String,
    pub max_output_lines_per_step: usize,
    pub plugin_config_dir: Option<PathBuf>,
    pub plugin_event_log_path: Option<PathBuf>,
    pub agent_variant: Option<String>,
}

pub const DEFAULT_PLAN_AGENT_VARIANT: &str = "medium";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanPluginConfig {
    pub config_dir: PathBuf,
    pub plugin_path: PathBuf,
    pub event_log_path: PathBuf,
}

impl PlanExecutorConfig {
    pub fn new(
        opencode_program: impl Into<String>,
        server_url: Option<String>,
        scope_path: impl Into<PathBuf>,
        title_prefix: impl Into<String>,
    ) -> Self {
        let opencode_program = opencode_program.into();
        Self {
            harness_id: "opencode".to_string(),
            harness_config: crate::harness::HarnessConfig::opencode(opencode_program.clone()),
            server_url,
            scope_path: scope_path.into(),
            title_prefix: title_prefix.into(),
            max_output_lines_per_step: DEFAULT_OUTPUT_LINES_PER_STEP,
            plugin_config_dir: None,
            plugin_event_log_path: None,
            agent_variant: Some(DEFAULT_PLAN_AGENT_VARIANT.to_string()),
        }
    }

    pub fn for_harness(
        harness_id: impl Into<String>,
        harness_config: crate::harness::HarnessConfig,
        server_url: Option<String>,
        scope_path: impl Into<PathBuf>,
        title_prefix: impl Into<String>,
    ) -> Self {
        Self {
            harness_id: harness_id.into(),
            harness_config,
            server_url,
            scope_path: scope_path.into(),
            title_prefix: title_prefix.into(),
            max_output_lines_per_step: DEFAULT_OUTPUT_LINES_PER_STEP,
            plugin_config_dir: None,
            plugin_event_log_path: None,
            agent_variant: Some(DEFAULT_PLAN_AGENT_VARIANT.to_string()),
        }
    }

    pub fn with_plugin_config(mut self, plugin: PlanPluginConfig) -> Self {
        self.plugin_config_dir = Some(plugin.config_dir);
        self.plugin_event_log_path = Some(plugin.event_log_path);
        self
    }
}

pub fn prepare_plan_plugin_config(repo_prism_dir: &Path) -> Result<PlanPluginConfig, String> {
    let config_dir = repo_prism_dir.join("opencode-plan-plugin");
    let plugin_path = config_dir.join("prism-plan-plugin.js");
    let event_log_path = config_dir.join("events.jsonl");
    fs::create_dir_all(&config_dir)
        .map_err(|error| format!("create OpenCode plan plugin directory: {error}"))?;
    fs::write(
        config_dir.join("opencode.json"),
        opencode_plan_plugin_config_json(),
    )
    .map_err(|error| format!("write OpenCode plan plugin config: {error}"))?;
    fs::write(&plugin_path, opencode_plan_plugin_js())
        .map_err(|error| format!("write OpenCode plan plugin: {error}"))?;
    Ok(PlanPluginConfig {
        config_dir,
        plugin_path,
        event_log_path,
    })
}

pub(super) fn opencode_plan_plugin_config_json() -> &'static str {
    r#"{
  "$schema": "https://opencode.ai/config.json",
  "plugin": ["./prism-plan-plugin.js"]
}
"#
}

pub(super) fn opencode_plan_plugin_js() -> &'static str {
    r#"import fs from "node:fs";

pub(super) const hookLogPath = process.env.PRISM_PLAN_HOOK_LOG;

function summarize(value) {
  if (value === undefined || value === null) return value;
  if (typeof value === "string") return value.length > 500 ? `${value.slice(0, 500)}...` : value;
  if (Array.isArray(value)) return value.slice(0, 20).map(summarize);
  if (typeof value !== "object") return value;
  const out = {};
  for (const [key, child] of Object.entries(value)) {
    if (/token|secret|password|authorization|cookie/i.test(key)) {
      out[key] = "[redacted]";
    } else if (/command|args|input|patch|diff|content|text/i.test(key)) {
      out[key] = summarize(child);
    } else if (["id", "sessionID", "sessionId", "status", "title", "name", "tool"].includes(key)) {
      out[key] = summarize(child);
    }
  }
  return out;
}

function writeHook(type, payload) {
  if (!hookLogPath) return;
  const event = {
    type,
    time_unix_ms: Date.now(),
    properties: summarize(payload),
  };
  fs.appendFileSync(hookLogPath, `${JSON.stringify(event)}\n`, { mode: 0o600 });
}

export default async function PrismPlanPlugin() {
  return {
    event(input) {
      writeHook(input?.event?.type || input?.type || "event", input?.event || input);
    },
    "tool.execute.before"(input) {
      writeHook("tool.execute.before", input);
    },
    "tool.execute.after"(input) {
      writeHook("tool.execute.after", input);
    },
    "session.diff"(input) {
      writeHook("session.diff", input);
    },
    "session.compacted"(input) {
      writeHook("session.compacted", input);
    },
  };
}
"#
}
