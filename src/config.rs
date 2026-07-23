use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use toml_edit::{Array, DocumentMut, Item, Table, value};

use crate::agent::{PromptMode, builtin_prompt_mode, detected_agents};
use crate::harness::{
    BUILTIN_HARNESS_IDS, Harness, HarnessConfig, OutputFormat, PromptTransport, builtin_adapter,
};
use crate::process::{command_exists, command_version, run_capture};
use crate::repo::Repository;
use crate::session::discover_sessions;
use crate::util::prism_config_dir;

pub const AGENT_CANDIDATES: [&str; 1] = ["opencode"];
pub const CONFIG_SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/NathanaelRea/prism/main/schemas/config.schema.json";
pub const CONFIG_SCHEMA_JSON: &str = include_str!("../schemas/config.schema.json");

pub fn config_example() -> String {
    format!("#:schema {CONFIG_SCHEMA_URL}\n")
        + r#"
# Prism config. Harness settings are global; other settings may be repository overrides.
default_harness = "opencode"
default_base = "main"
merge_method = "squash" # squash, merge, or rebase
escape_key = "esc-esc" # esc-esc or ctrl-space
plan_dir = "plans"
review_packet_dir = ".agent/review"
worktree_command = "wt"
opencode_port_base = 41000
opencode_port_span = 1000
opencode_shutdown_owned_servers = false
opencode_plan_plugin = false

[ui]
icon_style = "unicode" # or "nerd-font"

[worktrees]
columns = []

[harnesses.opencode]
program = "opencode"

[tools]
gh = "gh"
git = "git"
tmux = "tmux"
wt = "wt"
lazygit = "lazygit"
fzf = "fzf"

[checks]
pre_pr = []
pre_push = []
review_fix = []

[auto]
merge = false
cleanup_after_merge = false
require_review_approval = false
push_initial = true
push_repairs = false
review_wait_enabled = true
review_reviewer_identities = ["Copilot", "github-copilot"]
review_max_wait_seconds = 300
review_poll_interval_seconds = 30
review_continue_on_timeout = true
ci_wait_enabled = true
ci_max_wait_seconds = 1800
ci_poll_interval_seconds = 30

[prompt_templates]
auto_create_plan = "Create an implementation plan at `{{plan_path}}`. Do not implement or commit. Include phases, tests, verification, risks, observability, and architecture fit.\n\nTask:\n{{task}}\n\nMode: {{mode}}\nVariant: {{variant}}\nAgent profile: {{agent_profile}}"
auto_review_plan = "Review `{{plan_path}}` and edit it in place. Do not implement or commit. Check phases, risks, tests, observability, restartability, safety, and architecture fit.\n\nTask:\n{{task}}"
auto_implement = "Implement this task in the current worktree. Stop after implementation; do not commit, push, create a pull request, or merge.\n\nTask:\n{{task}}"
auto_fix_local_verify = "Fix the local verification failures, then stop without committing.\n\nOriginal task:\n{{task}}\n\nFailure context:\n{{context}}"
auto_fix_review = "Resolve the review feedback, then stop without committing.\n\nOriginal task:\n{{task}}\n\nReview context:\n{{context}}"
auto_fix_ci = "Fix the CI failure, then stop without committing.\n\nOriginal task:\n{{task}}\n\nCI context:\n{{context}}"
review_fix = "Here are review comments on PR {pr_number}.\n\nIf they are applicable, fix them. Otherwise, say why not.\n\n---\n\n{comments}"
ci_failure = "Here are CI failures on PR {pr_number}.\n\nFix the failing checks. Use the log tails below as the primary clues.\n\nPR: {url}\nBranch: {branch}\nHead SHA: {head_sha}\n\n---\n\n{failures}"
repair_commit_review = "fix: cr"
repair_commit_ci = "fix: ci"
repair_commit_merge = "fix: merge"
"#
}

pub fn user_config_template() -> String {
    config_example()
}

pub fn repo_config_template(include_worktree_columns: bool) -> String {
    let mut text = format!(
        "#:schema {CONFIG_SCHEMA_URL}\n\n# Repository overrides. Unspecified values inherit the global config.\n"
    );
    if include_worktree_columns {
        text.push_str("\n[worktrees]\ncolumns = []\n");
    }
    text
}

#[derive(Clone, Debug, Default)]
pub struct Checks {
    pub pre_pr: Vec<String>,
    pub pre_push: Vec<String>,
    pub review_fix: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoConfig {
    pub merge: bool,
    pub cleanup_after_merge: bool,
    pub require_review_approval: bool,
    pub push_initial: bool,
    pub push_repairs: bool,
    pub review_wait_enabled: bool,
    pub review_reviewer_identities: Vec<String>,
    pub review_max_wait_seconds: u64,
    pub review_poll_interval_seconds: u64,
    pub review_continue_on_timeout: bool,
    pub ci_wait_enabled: bool,
    pub ci_max_wait_seconds: u64,
    pub ci_poll_interval_seconds: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayoutConfig {
    pub sidebar_width: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconStyle {
    Unicode,
    NerdFont,
}

impl IconStyle {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "unicode" => Some(Self::Unicode),
            "nerd-font" | "nerdfont" | "nerd_font" => Some(Self::NerdFont),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Unicode => "unicode",
            Self::NerdFont => "nerd-font",
        }
    }
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            merge: false,
            cleanup_after_merge: false,
            require_review_approval: false,
            push_initial: true,
            push_repairs: false,
            review_wait_enabled: true,
            review_reviewer_identities: vec!["Copilot".to_string(), "github-copilot".to_string()],
            review_max_wait_seconds: 300,
            review_poll_interval_seconds: 30,
            review_continue_on_timeout: true,
            ci_wait_enabled: true,
            ci_max_wait_seconds: 1800,
            ci_poll_interval_seconds: 30,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscapeKey {
    EscEsc,
    CtrlSpace,
}

impl EscapeKey {
    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "esc-esc" | "escape-escape" => Some(Self::EscEsc),
            "ctrl-space" | "control-space" => Some(Self::CtrlSpace),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::EscEsc => "esc-esc",
            Self::CtrlSpace => "ctrl-space",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMethod {
    Squash,
    Merge,
    Rebase,
}

impl MergeMethod {
    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "squash" => Some(Self::Squash),
            "merge" => Some(Self::Merge),
            "rebase" => Some(Self::Rebase),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Squash => "squash",
            Self::Merge => "merge",
            Self::Rebase => "rebase",
        }
    }

    pub fn gh_flag(self) -> &'static str {
        match self {
            Self::Squash => "--squash",
            Self::Merge => "--merge",
            Self::Rebase => "--rebase",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub default_harness: String,
    pub harnesses: BTreeMap<String, HarnessConfig>,
    pub config_errors: Vec<String>,
    pub default_agent: String,
    pub default_base: Option<String>,
    pub plan_dir: String,
    pub review_packet_dir: String,
    pub worktree_command: String,
    pub opencode_port_base: u16,
    pub opencode_port_span: u16,
    pub opencode_shutdown_owned_servers: bool,
    pub opencode_plan_plugin: bool,
    pub escape_key: EscapeKey,
    pub merge_method: MergeMethod,
    pub icon_style: IconStyle,
    pub icon_style_configured: bool,
    pub auto: AutoConfig,
    pub layout: LayoutConfig,
    pub checks: Checks,
    pub worktree_columns: Vec<String>,
    pub tools: BTreeMap<String, String>,
    pub agent_commands: BTreeMap<String, String>,
    pub agent_prompt_modes: BTreeMap<String, PromptMode>,
    pub prompt_templates: BTreeMap<String, String>,
    pub user_path: PathBuf,
    pub repo_config_path: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    default_harness: Option<String>,
    harnesses: Option<BTreeMap<String, RawHarnessConfig>>,
    default_agent: Option<String>,
    default_base: Option<String>,
    plan_dir: Option<String>,
    review_packet_dir: Option<String>,
    worktree_command: Option<String>,
    opencode_port_base: Option<u16>,
    opencode_port_span: Option<u16>,
    opencode_shutdown_owned_servers: Option<bool>,
    opencode_plan_plugin: Option<bool>,
    escape_key: Option<String>,
    merge_method: Option<String>,
    ui: Option<RawUiConfig>,
    checks: Option<RawChecks>,
    auto: Option<RawAutoConfig>,
    layout: Option<RawLayoutConfig>,
    worktrees: Option<RawWorktrees>,
    tools: Option<BTreeMap<String, String>>,
    agents: Option<BTreeMap<String, RawAgentConfig>>,
    prompt_templates: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Default, Deserialize)]
struct RawUiConfig {
    icon_style: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawChecks {
    pre_pr: Option<Vec<String>>,
    pre_push: Option<Vec<String>>,
    review_fix: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct RawAutoConfig {
    merge: Option<bool>,
    cleanup_after_merge: Option<bool>,
    require_review_approval: Option<bool>,
    push_initial: Option<bool>,
    push_repairs: Option<bool>,
    review_wait_enabled: Option<bool>,
    review_reviewer_identities: Option<Vec<String>>,
    review_max_wait_seconds: Option<u64>,
    review_poll_interval_seconds: Option<u64>,
    review_continue_on_timeout: Option<bool>,
    ci_wait_enabled: Option<bool>,
    ci_max_wait_seconds: Option<u64>,
    ci_poll_interval_seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct RawLayoutConfig {
    sidebar_width: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
struct RawWorktrees {
    columns: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct RawAgentConfig {
    command: Option<String>,
    prompt_mode: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawHarnessConfig {
    adapter: Option<String>,
    program: Option<String>,
    arguments: Option<Vec<String>>,
    interactive_command: Option<Vec<String>>,
    interactive_prompt_transport: Option<String>,
    headless_command: Option<Vec<String>>,
    headless_prompt_transport: Option<String>,
    output_format: Option<String>,
    environment: Option<BTreeMap<String, String>>,
}

fn harness_config_from_raw(id: &str, raw: RawHarnessConfig) -> Result<HarnessConfig, String> {
    let adapter = raw
        .adapter
        .unwrap_or_else(|| builtin_adapter(id).unwrap_or("generic").to_string());
    let interactive_command = if adapter != "generic" {
        if raw.interactive_command.is_some() {
            return Err(format!(
                "harness '{id}' uses the {adapter} adapter; configure program instead of interactive_command"
            ));
        }
        vec![raw.program.unwrap_or_else(|| adapter.clone())]
    } else {
        if raw.program.is_some() {
            return Err(format!(
                "generic harness '{id}' uses interactive_command, not program"
            ));
        }
        raw.interactive_command.unwrap_or_default()
    };
    let parse_transport = |field: &str, value: Option<String>| {
        value
            .map(|value| {
                PromptTransport::parse(&value)
                    .ok_or_else(|| format!("harness '{id}' has invalid {field} '{value}'"))
            })
            .transpose()
    };
    let output_format = match raw.output_format.as_deref().unwrap_or("text") {
        "text" => OutputFormat::Text,
        other => {
            return Err(format!(
                "harness '{id}' has unsupported output_format '{other}'; generic harnesses support text"
            ));
        }
    };
    let output_format = if matches!(adapter.as_str(), "opencode" | "codex" | "claude" | "pi") {
        OutputFormat::JsonLines
    } else {
        output_format
    };
    let config = HarnessConfig {
        adapter,
        interactive_command,
        arguments: raw.arguments.unwrap_or_default(),
        interactive_prompt_transport: parse_transport(
            "interactive_prompt_transport",
            raw.interactive_prompt_transport,
        )?,
        headless_command: raw.headless_command,
        headless_prompt_transport: parse_transport(
            "headless_prompt_transport",
            raw.headless_prompt_transport,
        )?,
        output_format,
        environment: raw.environment.unwrap_or_default(),
    };
    config.validate(id)?;
    Ok(config)
}

impl Config {
    pub fn load(repo: &Repository) -> Self {
        let user_path = prism_config_dir().join("config.toml");
        let repo_config_path = repo.prism_dir().join("config.toml");
        let mut config = Self::defaults(user_path, repo_config_path);

        let user_path = config.user_path.clone();
        config.apply_file(&user_path);
        let repo_config_path = config.repo_config_path.clone();
        config.apply_file(&repo_config_path);
        config.default_agent = config.default_harness.clone();
        for (id, harness) in &config.harnesses {
            if let Err(error) = harness.validate(id) {
                config.config_errors.push(error);
            }
        }
        if !config.harnesses.contains_key(&config.default_harness) {
            config.config_errors.push(format!(
                "default_harness '{}' is not configured in [harnesses.{}]",
                config.default_harness, config.default_harness
            ));
        }
        config
    }

    fn defaults(user_path: PathBuf, repo_config_path: PathBuf) -> Self {
        let tools = [
            ("wt", "wt"),
            ("gh", "gh"),
            ("git", "git"),
            ("tmux", "tmux"),
            ("lazygit", "lazygit"),
            ("fzf", "fzf"),
            ("opencode", "opencode"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();

        let harnesses = BUILTIN_HARNESS_IDS
            .into_iter()
            .map(|adapter| {
                (
                    adapter.to_string(),
                    HarnessConfig::builtin(adapter, adapter),
                )
            })
            .collect();
        Self {
            default_harness: "opencode".to_string(),
            harnesses,
            config_errors: Vec::new(),
            default_agent: "opencode".to_string(),
            default_base: Some("main".to_string()),
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            opencode_port_base: 41_000,
            opencode_port_span: 1_000,
            opencode_shutdown_owned_servers: false,
            opencode_plan_plugin: false,
            escape_key: EscapeKey::EscEsc,
            merge_method: MergeMethod::Squash,
            icon_style: IconStyle::Unicode,
            icon_style_configured: false,
            auto: AutoConfig::default(),
            layout: LayoutConfig::default(),
            checks: Checks::default(),
            worktree_columns: Vec::new(),
            tools,
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            prompt_templates: BTreeMap::new(),
            user_path,
            repo_config_path,
        }
    }

    fn apply_file(&mut self, path: &Path) {
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        let raw = match toml::from_str::<RawConfig>(&text) {
            Ok(raw) => raw,
            Err(error) => {
                self.config_errors
                    .push(format!("parse {}: {error}", path.display()));
                return;
            }
        };
        let is_user_config = path == self.user_path;
        if raw.default_agent.is_some() || raw.agents.is_some() {
            self.config_errors.push(format!(
                "{} uses obsolete default_agent/[agents.*] settings; replace them with default_harness/[harnesses.*]",
                path.display()
            ));
        }
        if raw
            .tools
            .as_ref()
            .is_some_and(|tools| tools.contains_key("opencode"))
        {
            self.config_errors.push(format!(
                "{} uses obsolete [tools].opencode; configure [harnesses.opencode].program instead",
                path.display()
            ));
        }
        if !is_user_config && (raw.default_harness.is_some() || raw.harnesses.is_some()) {
            self.config_errors.push(format!(
                "{} configures default_harness/[harnesses.*], but harness selection is global; move these settings to {}",
                path.display(),
                self.user_path.display()
            ));
        }
        self.apply_raw_config(raw, is_user_config);
    }

    fn apply_raw_config(&mut self, raw: RawConfig, apply_harnesses: bool) {
        if apply_harnesses {
            if let Some(value) = raw.default_harness {
                self.default_harness = value;
            }
            if let Some(harnesses) = raw.harnesses {
                for (id, raw) in harnesses {
                    match harness_config_from_raw(&id, raw) {
                        Ok(harness) => {
                            self.harnesses.insert(id, harness);
                        }
                        Err(error) => self.config_errors.push(error),
                    }
                }
            }
        }
        if let Some(value) = raw.default_agent {
            self.default_agent = value;
        }
        if let Some(value) = raw.default_base {
            self.default_base = Some(value);
        }
        if let Some(value) = raw.plan_dir {
            self.plan_dir = value;
        }
        if let Some(value) = raw.review_packet_dir {
            self.review_packet_dir = value;
        }
        if let Some(value) = raw.worktree_command {
            self.worktree_command = value;
        }
        if let Some(port) = raw.opencode_port_base {
            self.opencode_port_base = port;
        }
        if let Some(span) = raw.opencode_port_span.filter(|span| *span > 0) {
            self.opencode_port_span = span;
        }
        if let Some(shutdown) = raw.opencode_shutdown_owned_servers {
            self.opencode_shutdown_owned_servers = shutdown;
        }
        if let Some(enabled) = raw.opencode_plan_plugin {
            self.opencode_plan_plugin = enabled;
        }
        if let Some(value) = raw
            .merge_method
            .and_then(|value| MergeMethod::parse(&value))
        {
            self.merge_method = value;
        }
        if let Some(value) = raw.escape_key.and_then(|value| EscapeKey::parse(&value)) {
            self.escape_key = value;
        }
        if let Some(value) = raw.ui.and_then(|ui| ui.icon_style)
            && let Some(style) = IconStyle::parse(&value)
        {
            self.icon_style = style;
            self.icon_style_configured = true;
        }
        if let Some(checks) = raw.checks {
            if let Some(values) = checks.pre_pr {
                self.checks.pre_pr = values;
            }
            if let Some(values) = checks.pre_push {
                self.checks.pre_push = values;
            }
            if let Some(values) = checks.review_fix {
                self.checks.review_fix = values;
            }
        }
        if let Some(auto) = raw.auto {
            if let Some(enabled) = auto.merge {
                self.auto.merge = enabled;
            }
            if let Some(enabled) = auto.cleanup_after_merge {
                self.auto.cleanup_after_merge = enabled;
            }
            if let Some(enabled) = auto.require_review_approval {
                self.auto.require_review_approval = enabled;
            }
            if let Some(enabled) = auto.push_initial {
                self.auto.push_initial = enabled;
            }
            if let Some(enabled) = auto.push_repairs {
                self.auto.push_repairs = enabled;
            }
            if let Some(enabled) = auto.review_wait_enabled {
                self.auto.review_wait_enabled = enabled;
            }
            if let Some(values) = auto.review_reviewer_identities {
                self.auto.review_reviewer_identities = values;
            }
            if let Some(seconds) = auto.review_max_wait_seconds {
                self.auto.review_max_wait_seconds = seconds;
            }
            if let Some(seconds) = auto.review_poll_interval_seconds {
                self.auto.review_poll_interval_seconds = seconds.max(1);
            }
            if let Some(value) = auto.review_continue_on_timeout {
                self.auto.review_continue_on_timeout = value;
            }
            if let Some(enabled) = auto.ci_wait_enabled {
                self.auto.ci_wait_enabled = enabled;
            }
            if let Some(seconds) = auto.ci_max_wait_seconds {
                self.auto.ci_max_wait_seconds = seconds;
            }
            if let Some(seconds) = auto.ci_poll_interval_seconds {
                self.auto.ci_poll_interval_seconds = seconds.max(1);
            }
        }
        if let Some(layout) = raw.layout
            && let Some(width) = layout.sidebar_width
        {
            self.layout.sidebar_width = Some(width.clamp(20, 120));
        }
        if let Some(worktrees) = raw.worktrees
            && let Some(values) = worktrees.columns
        {
            self.worktree_columns = values;
        }
        if let Some(tools) = raw.tools {
            self.tools.extend(tools);
        }
        if let Some(templates) = raw.prompt_templates {
            self.prompt_templates.extend(templates);
        }
        if let Some(agents) = raw.agents {
            for (name, agent) in agents {
                if let Some(command) = agent.command {
                    self.agent_commands.insert(name.clone(), command);
                }
                if let Some(mode) = agent
                    .prompt_mode
                    .and_then(|value| PromptMode::parse(&value))
                {
                    self.agent_prompt_modes.insert(name, mode);
                }
            }
        }
    }

    pub fn tool(&self, name: &str) -> String {
        if name == "opencode"
            && let Some(program) = self
                .harnesses
                .get(&self.default_harness)
                .filter(|harness| harness.adapter == "opencode")
                .and_then(|harness| harness.interactive_command.first())
            && program != "opencode"
        {
            return program.clone();
        }
        self.tools
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    pub fn agent_command(&self, name: &str) -> String {
        if let Some(command) = self.agent_commands.get(name) {
            return command.clone();
        }
        if name == "opencode" {
            return format!("{} run --format json", self.tool("opencode"));
        }
        self.tool(name)
    }

    pub fn harness(&self, id: &str) -> Result<Harness<'_>, String> {
        self.harnesses
            .get(id)
            .map(|config| Harness::new(id, config))
            .ok_or_else(|| format!("harness '{id}' is not configured"))
    }

    pub fn selected_harness(&self) -> Result<Harness<'_>, String> {
        self.harness(&self.default_harness)
    }

    pub fn save_user_default_harness(&self, harness_id: &str) -> Result<(), String> {
        if !self.harnesses.contains_key(harness_id) {
            return Err(format!("harness '{harness_id}' is not configured"));
        }
        update_user_harness_config(&self.user_path, harness_id, None)
    }

    pub fn save_user_generic_harness(
        &self,
        harness_id: &str,
        harness: &HarnessConfig,
    ) -> Result<(), String> {
        validate_new_generic_harness_id(harness_id, &self.harnesses)?;
        harness.validate(harness_id)?;
        update_user_harness_config(&self.user_path, harness_id, Some(harness))
    }

    pub(crate) fn for_harness(&self, harness_id: &str) -> Result<Self, String> {
        if !self.harnesses.contains_key(harness_id) {
            return Err(format!(
                "worktree is bound to harness '{harness_id}', but [harnesses.{harness_id}] is not configured; restore it or migrate the worktree"
            ));
        }
        let mut config = self.clone();
        config.default_harness = harness_id.to_string();
        if self.default_agent == self.default_harness
            || !self.agent_commands.contains_key(&self.default_agent)
        {
            config.default_agent = harness_id.to_string();
        }
        Ok(config)
    }

    pub fn selected_adapter_is(&self, adapter: &str) -> bool {
        if self.default_agent != self.default_harness
            && self.agent_commands.contains_key(&self.default_agent)
        {
            return false;
        }
        self.harnesses
            .get(&self.default_harness)
            .is_some_and(|harness| harness.adapter == adapter)
    }

    pub fn harness_config(&self, id: &str) -> Result<HarnessConfig, String> {
        let mut harness = self
            .harnesses
            .get(id)
            .cloned()
            .ok_or_else(|| format!("harness '{id}' is not configured"))?;
        if harness.adapter == "opencode"
            && harness
                .interactive_command
                .first()
                .is_some_and(|program| program == "opencode")
        {
            harness.interactive_command = vec![
                self.tools
                    .get("opencode")
                    .cloned()
                    .unwrap_or_else(|| "opencode".to_string()),
            ];
        }
        Ok(harness)
    }

    pub fn harness_adapter(&self, id: &str) -> Result<String, String> {
        self.harnesses
            .get(id)
            .map(|harness| harness.adapter.clone())
            .ok_or_else(|| format!("harness '{id}' is not configured"))
    }

    pub fn recorded_harness_config(
        &self,
        harness_id: &str,
        adapter_id: &str,
    ) -> Result<HarnessConfig, String> {
        let harness = self.harness_config(harness_id)?;
        if harness.adapter != adapter_id {
            return Err(format!(
                "harness '{harness_id}' was recorded with adapter '{adapter_id}', but it is now configured as '{}'",
                harness.adapter
            ));
        }
        Ok(harness)
    }

    pub fn agent_prompt_mode(&self, name: &str) -> PromptMode {
        self.agent_prompt_modes
            .get(name)
            .copied()
            .unwrap_or_else(|| builtin_prompt_mode(name))
    }

    pub fn prompt_template(&self, name: &str) -> Option<&str> {
        self.prompt_templates.get(name).map(String::as_str)
    }

    pub fn is_default_branch(&self, branch: &str) -> bool {
        self.default_base
            .as_deref()
            .map(|base| !base.trim().is_empty() && branch == base)
            .unwrap_or(false)
    }

    pub fn save_user_icon_style(&self, style: IconStyle) -> Result<(), String> {
        save_user_icon_style(&self.user_path, style)
    }
}

pub fn validate_new_generic_harness_id(
    id: &str,
    configured: &BTreeMap<String, HarnessConfig>,
) -> Result<(), String> {
    let mut chars = id.chars();
    let valid = chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_')
        });
    if !valid {
        return Err(
            "harness ID must start with a lowercase letter and contain only lowercase letters, digits, '-' or '_'"
                .to_string(),
        );
    }
    if builtin_adapter(id).is_some() {
        return Err(format!(
            "harness ID '{id}' is reserved for a built-in adapter"
        ));
    }
    if configured.contains_key(id) {
        return Err(format!("harness '{id}' is already configured"));
    }
    Ok(())
}

fn update_user_harness_config(
    path: &Path,
    default_harness: &str,
    generic: Option<&HarnessConfig>,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create config dir: {error}"))?;
    }
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("read config file: {error}")),
    };
    let updated = update_user_harness_config_text(&text, default_harness, generic)?;
    let write_path = fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| metadata.file_type().is_symlink())
        .map(|_| fs::canonicalize(path).map_err(|error| format!("resolve config symlink: {error}")))
        .transpose()?
        .unwrap_or_else(|| path.to_path_buf());
    write_config_atomically(&write_path, updated.as_bytes())
}

fn write_config_atomically(path: &Path, contents: &[u8]) -> Result<(), String> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    for _ in 0..100 {
        let staging = path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = match options.open(&staging) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create staged config file: {error}")),
        };
        let result = file
            .write_all(contents)
            .and_then(|()| file.sync_all())
            .and_then(|()| {
                if let Some(permissions) = permissions.clone() {
                    fs::set_permissions(&staging, permissions)?;
                }
                fs::rename(&staging, path)
            });
        if let Err(error) = result {
            let _ = fs::remove_file(&staging);
            return Err(format!("write config file: {error}"));
        }
        return Ok(());
    }
    Err("create unique staged config file".to_string())
}

fn update_user_harness_config_text(
    text: &str,
    default_harness: &str,
    generic: Option<&HarnessConfig>,
) -> Result<String, String> {
    let mut document = if text.trim().is_empty() {
        DocumentMut::new()
    } else {
        text.parse::<DocumentMut>()
            .map_err(|error| format!("parse user config: {error}"))?
    };
    if let Some(current) = document
        .get_mut("default_harness")
        .and_then(Item::as_value_mut)
    {
        let decor = current.decor().clone();
        *current = toml_edit::Value::from(default_harness);
        *current.decor_mut() = decor;
    } else {
        document["default_harness"] = value(default_harness);
    }

    if let Some(generic) = generic {
        if generic.adapter != "generic" {
            return Err(
                "only generic harnesses can be added through the harness dialog".to_string(),
            );
        }
        let harnesses_item = document
            .entry("harnesses")
            .or_insert_with(|| Item::Table(Table::new()));
        if let Some(inline) = harnesses_item.as_inline_table().cloned() {
            let mut table = Table::new();
            for (key, value_) in inline.iter() {
                table[key] = Item::Value(value_.clone());
            }
            *harnesses_item = Item::Table(table);
        }
        let harnesses = harnesses_item
            .as_table_mut()
            .ok_or_else(|| "harnesses must be a table".to_string())?;
        if harnesses.contains_key(default_harness) {
            return Err(format!("harness '{default_harness}' is already configured"));
        }
        let mut table = Table::new();
        table["adapter"] = value("generic");
        table["interactive_command"] = value(string_array(&generic.interactive_command));
        if let Some(transport) = generic.interactive_prompt_transport {
            table["interactive_prompt_transport"] = value(transport.label());
        }
        if let Some(command) = &generic.headless_command {
            table["headless_command"] = value(string_array(command));
        }
        if let Some(transport) = generic.headless_prompt_transport {
            table["headless_prompt_transport"] = value(transport.label());
        }
        table["output_format"] = value("text");
        if !generic.environment.is_empty() {
            let mut environment = Table::new();
            for (key, value_) in &generic.environment {
                environment[key] = value(value_);
            }
            table["environment"] = Item::Table(environment);
        }
        harnesses[default_harness] = Item::Table(table);
    }
    Ok(document.to_string())
}

fn string_array(items: &[String]) -> Array {
    let mut array = Array::new();
    for item in items {
        array.push(item.as_str());
    }
    array
}

fn save_user_icon_style(path: &Path, style: IconStyle) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create config dir: {error}"))?;
    }
    let mut text = fs::read_to_string(path).unwrap_or_default();
    if text.contains("icon_style") {
        return Ok(());
    }
    let setting = format!("icon_style = \"{}\"\n", style.label());
    if let Some(index) = ui_table_insert_index(&text) {
        text.insert_str(index, &setting);
    } else {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[ui]\n");
        text.push_str(&setting);
    }
    fs::write(path, text).map_err(|error| format!("write user config: {error}"))
}

fn ui_table_insert_index(text: &str) -> Option<usize> {
    let mut offset = 0;
    let mut in_ui = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed == "[ui]" {
            in_ui = true;
            offset += line.len();
            continue;
        }
        if in_ui && trimmed.starts_with('[') {
            return Some(offset);
        }
        offset += line.len();
    }
    in_ui.then_some(offset)
}

pub fn print_config(repo: &Repository, config: &Config) {
    println!("repo_root = {}", repo.root.display());
    println!("user_config = {}", config.user_path.display());
    println!("repo_config = {}", config.repo_config_path.display());
    println!("default_harness = {}", config.default_harness);
    println!(
        "default_base = {}",
        config.default_base.as_deref().unwrap_or("")
    );
    println!("plan_dir = {}", config.plan_dir);
    println!("review_packet_dir = {}", config.review_packet_dir);
    println!("worktree_command = {}", config.worktree_command);
    println!("opencode_port_base = {}", config.opencode_port_base);
    println!("opencode_port_span = {}", config.opencode_port_span);
    println!(
        "opencode_shutdown_owned_servers = {}",
        config.opencode_shutdown_owned_servers
    );
    println!("opencode_plan_plugin = {}", config.opencode_plan_plugin);
    println!("escape_key = {}", config.escape_key.label());
    println!("merge_method = {}", config.merge_method.label());
    println!("ui.icon_style = {}", config.icon_style.label());
    println!(
        "layout.sidebar_width = {}",
        config
            .layout
            .sidebar_width
            .map(|width| width.to_string())
            .unwrap_or_default()
    );
    println!("auto.merge = {}", config.auto.merge);
    println!(
        "auto.cleanup_after_merge = {}",
        config.auto.cleanup_after_merge
    );
    println!(
        "auto.require_review_approval = {}",
        config.auto.require_review_approval
    );
    println!("auto.push_initial = {}", config.auto.push_initial);
    println!("auto.push_repairs = {}", config.auto.push_repairs);
    println!(
        "auto.review_wait_enabled = {}",
        config.auto.review_wait_enabled
    );
    println!(
        "auto.review_reviewer_identities = {:?}",
        config.auto.review_reviewer_identities
    );
    println!(
        "auto.review_max_wait_seconds = {}",
        config.auto.review_max_wait_seconds
    );
    println!(
        "auto.review_poll_interval_seconds = {}",
        config.auto.review_poll_interval_seconds
    );
    println!(
        "auto.review_continue_on_timeout = {}",
        config.auto.review_continue_on_timeout
    );
    println!("auto.ci_wait_enabled = {}", config.auto.ci_wait_enabled);
    println!(
        "auto.ci_max_wait_seconds = {}",
        config.auto.ci_max_wait_seconds
    );
    println!(
        "auto.ci_poll_interval_seconds = {}",
        config.auto.ci_poll_interval_seconds
    );
    println!("worktree_columns = {:?}", config.worktree_columns);
    println!(
        "prompt_templates = {:?}",
        config.prompt_templates.keys().collect::<Vec<_>>()
    );
    println!("[tools]");
    for (key, value) in &config.tools {
        println!("{key} = {value}");
    }
    println!("[checks]");
    println!("pre_pr = {:?}", config.checks.pre_pr);
    println!("pre_push = {:?}", config.checks.pre_push);
    println!("review_fix = {:?}", config.checks.review_fix);
    println!("[harnesses]");
    for (id, harness) in &config.harnesses {
        println!("{id}.adapter = {}", harness.adapter);
        println!(
            "{id}.interactive_command = {:?}",
            harness.interactive_command
        );
        if let Some(command) = &harness.headless_command {
            println!("{id}.headless_command = {command:?}");
        }
        if let Some(transport) = harness.headless_prompt_transport {
            println!("{id}.headless_prompt_transport = {}", transport.label());
        }
    }
}

pub fn doctor(repo: &Repository, config: &mut Config) -> Result<(), String> {
    println!("Prism doctor");
    println!("repo: {}", repo.root.display());
    println!("user config: {}", config.user_path.display());
    println!("repo config: {}", config.repo_config_path.display());
    println!();

    if let Ok(harness) = config.selected_harness() {
        let description = harness.describe();
        let configured = &config.harnesses[&config.default_harness];
        let program = configured
            .interactive_command
            .first()
            .map(String::as_str)
            .unwrap_or("-");
        println!("selected harness: {}", description.id);
        println!("adapter: {}", description.adapter);
        println!(
            "harness configuration source: {}",
            harness_config_source(config)
        );
        println!("supported version: {}", description.supported_version);
        println!("program: {program}");
        println!(
            "resolved program: {}",
            resolve_executable(program)
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "unavailable".to_string())
        );
        println!(
            "capabilities: interactive={} initial_prompt={} headless={} structured_events={} persistent_sessions={} interactive_resume={} observe={} submit={} cancel_session={}",
            description.interactive,
            description.initial_prompt,
            description.headless,
            description.structured_events,
            description.persistent_sessions,
            description.interactive_resume,
            description.observe,
            description.submit,
            description.cancel_session
        );
        print_tool_status("harness", program, true);
        if let Some(headless_program) = configured
            .headless_command
            .as_ref()
            .and_then(|command| command.first())
            && headless_program != program
        {
            print_tool_status("harness headless", headless_program, true);
        }
        for (capability, supported, reason) in [
            (
                "initial prompt",
                description.initial_prompt,
                "no reliable startup prompt transport is configured",
            ),
            (
                "managed Plan/Auto Flow",
                description.headless,
                "no headless command is configured",
            ),
            (
                "interactive resume",
                description.interactive_resume,
                "adapter has no persistent resumable session contract",
            ),
            (
                "live observation",
                description.observe,
                "adapter exposes process-level status only",
            ),
            (
                "later prompt submission",
                description.submit,
                "adapter has no supported live submission protocol",
            ),
            (
                "native cancellation",
                description.cancel_session,
                "only the owned local process can be terminated",
            ),
        ] {
            if !supported {
                println!("unavailable: {capability}: {reason}");
            }
        }
    }
    for error in &config.config_errors {
        println!("config error: {error}");
    }
    println!();

    print_tool_status("git", &config.tool("git"), true);
    print_tool_status("gh", &config.tool("gh"), true);
    print_tool_status("tmux", &config.tool("tmux"), true);
    print_tool_status(
        &config.worktree_command,
        &config.tool(&config.worktree_command),
        true,
    );
    print_tool_status("fzf", &config.tool("fzf"), false);
    println!();

    println!();
    match run_capture(Command::new(config.tool("gh")).arg("auth").arg("status")) {
        Ok(_) => println!("gh auth: ok"),
        Err(error) => println!("gh auth: {error}"),
    }

    println!();
    println!(
        "checks: pre_pr={} pre_push={} review_fix={}",
        config.checks.pre_pr.len(),
        config.checks.pre_push.len(),
        config.checks.review_fix.len()
    );

    println!();
    match discover_sessions(repo, config) {
        Ok(sessions) => {
            println!("worktrees: {}", sessions.len());
            for session in sessions {
                println!(
                    "  {}  {}  {}",
                    session.branch, session.status_label, session.path_display
                );
            }
        }
        Err(error) => println!("worktrees: {error}"),
    }

    Ok(())
}

fn harness_config_source(config: &Config) -> String {
    let configured_by_user = fs::read_to_string(&config.user_path)
        .ok()
        .and_then(|text| toml::from_str::<RawConfig>(&text).ok())
        .is_some_and(|raw| {
            raw.default_harness.is_some()
                || raw
                    .harnesses
                    .is_some_and(|harnesses| harnesses.contains_key(&config.default_harness))
        });
    if configured_by_user {
        config.user_path.display().to_string()
    } else {
        "built-in defaults".to_string()
    }
}

fn resolve_executable(program: &str) -> Option<PathBuf> {
    let path = PathBuf::from(program);
    if path.components().count() > 1 {
        return path.exists().then_some(path);
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

pub fn ensure_required_tools(config: &Config) -> Result<(), String> {
    let required = [
        ("git", config.tool("git")),
        ("gh", config.tool("gh")),
        ("tmux", config.tool("tmux")),
        (
            config.worktree_command.as_str(),
            config.tool(&config.worktree_command),
        ),
    ];
    let missing = required
        .into_iter()
        .filter(|(_, command)| !command_exists(command))
        .map(|(label, command)| format!("{label} ({command})"))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "missing required tool(s): {}. Install them or configure [tools] in {} or {}",
            missing.join(", "),
            config.user_path.display(),
            config.repo_config_path.display()
        ))
    }
}

fn print_tool_status(label: &str, command: &str, required: bool) {
    let prefix = if command_exists(command) {
        "ok"
    } else {
        "missing"
    };
    let required = if required { "required" } else { "optional" };
    let version = command_version(command).unwrap_or_else(|| "-".to_string());
    println!("{prefix:7} {label:12} {command:18} {required:8} {version}");
}

pub fn ensure_default_agent(config: &mut Config) -> Result<(), String> {
    if config.default_agent != "ask" {
        return ensure_configured_default_agent(config);
    }

    let detected = detected_agents(config);
    if detected.is_empty() {
        return Err(format!(
            "no agent backend found; install or configure one of: {}",
            AGENT_CANDIDATES.join(", ")
        ));
    }

    if !crate::terminal::stdin_is_tty() {
        config.default_agent = detected[0].clone();
        return Ok(());
    }

    println!("Select default Prism agent backend:");
    for (index, agent) in detected.iter().enumerate() {
        println!("  {}. {}", index + 1, agent);
    }
    print!("Choice [1]: ");
    use std::io::Write;
    std::io::stdout()
        .flush()
        .map_err(|error| error.to_string())?;
    let mut choice = String::new();
    std::io::stdin()
        .read_line(&mut choice)
        .map_err(|error| error.to_string())?;
    let selected = choice
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|number| detected.get(number.saturating_sub(1)))
        .unwrap_or(&detected[0])
        .clone();
    config.default_agent = selected.clone();
    save_user_default_agent(config, &selected)?;
    Ok(())
}

pub fn ensure_default_agent_noninteractive(config: &mut Config) -> Result<(), String> {
    if config.default_agent != "ask" {
        return ensure_configured_default_agent(config);
    }

    let detected = detected_agents(config);
    if detected.is_empty() {
        return Err(format!(
            "no agent backend found; install or configure one of: {}",
            AGENT_CANDIDATES.join(", ")
        ));
    }
    config.default_agent = detected[0].clone();
    Ok(())
}

fn ensure_configured_default_agent(config: &Config) -> Result<(), String> {
    if !config.config_errors.is_empty() {
        return Err(config.config_errors.join("\n"));
    }
    let harness = config.selected_harness()?;
    let description = harness.describe();
    let command = config.harnesses[&config.default_harness]
        .interactive_command
        .first()
        .ok_or_else(|| "selected harness has no interactive command".to_string())?;
    if command_exists(command) {
        return Ok(());
    }
    Err(format!(
        "configured harness '{}' ({}) was not found on PATH",
        description.id, command
    ))
}

fn save_user_default_agent(config: &Config, selected: &str) -> Result<(), String> {
    if let Some(parent) = config.user_path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create config dir: {error}"))?;
    }
    let mut text = if config.user_path.exists() {
        fs::read_to_string(&config.user_path).unwrap_or_default()
    } else {
        String::new()
    };
    if text
        .lines()
        .any(|line| line.trim_start().starts_with("default_agent"))
    {
        text = text
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("default_agent") {
                    format!("default_agent = \"{selected}\"")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        text.push('\n');
    } else {
        if !text.ends_with('\n') && !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("default_agent = \"{selected}\"\n"));
    }
    fs::write(&config.user_path, text).map_err(|error| format!("write config: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_escape_key() {
        assert_eq!(EscapeKey::parse("ctrl-space"), Some(EscapeKey::CtrlSpace));
        assert_eq!(EscapeKey::parse("esc-esc"), Some(EscapeKey::EscEsc));
    }

    #[test]
    fn parses_merge_method() {
        assert_eq!(MergeMethod::parse("squash"), Some(MergeMethod::Squash));
        assert_eq!(MergeMethod::parse("merge"), Some(MergeMethod::Merge));
        assert_eq!(MergeMethod::parse("rebase"), Some(MergeMethod::Rebase));
        assert_eq!(MergeMethod::parse("unknown"), None);
    }

    #[test]
    fn defaults_to_opencode_json_run_backend() {
        let config = Config::defaults(
            PathBuf::from("/tmp/user.toml"),
            PathBuf::from("/tmp/prism-repo-config.toml"),
        );

        assert_eq!(AGENT_CANDIDATES, ["opencode"]);
        assert_eq!(config.default_agent, "opencode");
        assert_eq!(config.default_base.as_deref(), Some("main"));
        assert_eq!(config.merge_method, MergeMethod::Squash);
        assert_eq!(config.icon_style, IconStyle::Unicode);
        assert!(!config.icon_style_configured);
        assert_eq!(config.layout.sidebar_width, None);
        assert!(config.worktree_columns.is_empty());
        assert_eq!(config.opencode_port_base, 41_000);
        assert_eq!(config.opencode_port_span, 1_000);
        assert!(!config.opencode_shutdown_owned_servers);
        assert!(!config.opencode_plan_plugin);
        assert!(config.is_default_branch("main"));
        assert_eq!(
            config.agent_command("opencode"),
            "opencode run --format json"
        );
        assert_eq!(config.agent_prompt_mode("opencode"), PromptMode::Argument);
    }

    #[test]
    fn repo_config_overrides_default_base() {
        let path = std::env::temp_dir().join(format!(
            "prism-config-override-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, r#"default_base = "develop""#).unwrap();
        let mut config = Config::defaults(PathBuf::from("/tmp/user.toml"), path.clone());

        config.apply_file(&path);

        assert_eq!(config.default_base.as_deref(), Some("develop"));
        assert!(config.is_default_branch("develop"));
        assert!(!config.is_default_branch("main"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn repo_config_overrides_opencode_runtime_settings() {
        let path = std::env::temp_dir().join(format!(
            "prism-config-opencode-runtime-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &path,
            "opencode_port_base = 42000\nopencode_port_span = 50\nopencode_shutdown_owned_servers = true\nopencode_plan_plugin = true\n",
        )
        .unwrap();
        let mut config = Config::defaults(PathBuf::from("/tmp/user.toml"), path.clone());

        config.apply_file(&path);

        assert_eq!(config.opencode_port_base, 42_000);
        assert_eq!(config.opencode_port_span, 50);
        assert!(config.opencode_shutdown_owned_servers);
        assert!(config.opencode_plan_plugin);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn repo_config_overrides_merge_method() {
        let path = std::env::temp_dir().join(format!(
            "prism-config-merge-method-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, r#"merge_method = "merge""#).unwrap();
        let mut config = Config::defaults(PathBuf::from("/tmp/user.toml"), path.clone());

        config.apply_file(&path);

        assert_eq!(config.merge_method, MergeMethod::Merge);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn config_toml_supports_comments_escaped_strings_arrays_and_harness_tables() {
        let path = std::env::temp_dir().join(format!(
            "prism-config-structured-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &path,
            r#"
# top-level comment
default_harness = "company-agent"
default_base = "release/main"
review_packet_dir = ".agent/review \"packets\""
escape_key = "ctrl-space"

[layout]
sidebar_width = 64

[ui]
icon_style = "nerd-font"

[checks]
pre_pr = ["cargo test", "printf \"done\""] # trailing comment

[worktrees]
columns = ["url", "ci.status"]

[tools]
gh = "/opt/tools/gh"

[harnesses.company-agent]
adapter = "generic"
interactive_command = ["company-agent"]
headless_command = ["company-agent", "run", "{prompt}"]
headless_prompt_transport = "argument"

[prompt_templates]
review = "fix\nreview"
"#,
        )
        .unwrap();
        let mut config = Config::defaults(path.clone(), PathBuf::from("/tmp/repo.toml"));

        config.apply_file(&path);

        assert_eq!(config.default_base.as_deref(), Some("release/main"));
        assert_eq!(config.default_harness, "company-agent");
        assert_eq!(
            config.harnesses["company-agent"].headless_prompt_transport,
            Some(PromptTransport::Argument)
        );
        assert_eq!(config.review_packet_dir, ".agent/review \"packets\"");
        assert_eq!(config.escape_key, EscapeKey::CtrlSpace);
        assert_eq!(config.icon_style, IconStyle::NerdFont);
        assert!(config.icon_style_configured);
        assert_eq!(config.layout.sidebar_width, Some(64));
        assert_eq!(config.checks.pre_pr, vec!["cargo test", "printf \"done\""]);
        assert_eq!(config.worktree_columns, vec!["url", "ci.status"]);
        assert_eq!(config.tool("gh"), "/opt/tools/gh");
        assert_eq!(config.prompt_template("review"), Some("fix\nreview"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn layout_sidebar_width_is_bounded() {
        let mut config = Config::defaults(
            PathBuf::from("/tmp/user.toml"),
            PathBuf::from("/tmp/prism-repo-config.toml"),
        );

        config.apply_raw_config(
            RawConfig {
                layout: Some(RawLayoutConfig {
                    sidebar_width: Some(4),
                }),
                ..RawConfig::default()
            },
            false,
        );
        assert_eq!(config.layout.sidebar_width, Some(20));

        config.apply_raw_config(
            RawConfig {
                layout: Some(RawLayoutConfig {
                    sidebar_width: Some(999),
                }),
                ..RawConfig::default()
            },
            false,
        );
        assert_eq!(config.layout.sidebar_width, Some(120));
    }

    #[test]
    fn saves_icon_style_in_existing_ui_table() {
        let path = std::env::temp_dir().join(format!(
            "prism-config-icon-style-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, "[ui]\nother = true\n[tools]\ngh = \"gh\"\n").unwrap();

        save_user_icon_style(&path, IconStyle::NerdFont).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[ui]"));
        assert!(text.contains("icon_style = \"nerd-font\""));
        assert!(text.contains("[tools]"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn accepts_configured_generic_default_harness() {
        let mut config = Config::defaults(
            PathBuf::from("/tmp/user.toml"),
            PathBuf::from("/tmp/prism-repo-config.toml"),
        );
        config.default_harness = "other-agent".to_string();
        config.default_agent = config.default_harness.clone();
        config.harnesses.insert(
            "other-agent".to_string(),
            HarnessConfig {
                adapter: "generic".to_string(),
                interactive_command: vec!["/bin/sh".to_string()],
                arguments: Vec::new(),
                interactive_prompt_transport: None,
                headless_command: None,
                headless_prompt_transport: None,
                output_format: OutputFormat::Text,
                environment: BTreeMap::new(),
            },
        );

        ensure_configured_default_agent(&config).unwrap();
    }

    #[test]
    fn selecting_builtin_codex_does_not_require_an_explicit_harness_table() {
        let mut config = Config::defaults(
            PathBuf::from("/tmp/user.toml"),
            PathBuf::from("/tmp/prism-repo-config.toml"),
        );
        config.apply_raw_config(
            RawConfig {
                default_harness: Some("codex".to_string()),
                ..RawConfig::default()
            },
            true,
        );

        let harness = config.selected_harness().unwrap();

        assert_eq!(harness.describe().adapter, "codex");
    }

    #[test]
    fn reserved_builtin_ids_cannot_be_redefined_as_generic() {
        for id in ["opencode", "codex", "claude", "pi"] {
            let error = harness_config_from_raw(
                id,
                RawHarnessConfig {
                    adapter: Some("generic".to_string()),
                    interactive_command: Some(vec!["other-agent".to_string()]),
                    ..RawHarnessConfig::default()
                },
            )
            .unwrap_err();

            assert!(error.contains("reserved"), "{id}: {error}");
        }
    }

    #[test]
    fn custom_ids_cannot_alias_builtin_adapters() {
        let error = harness_config_from_raw(
            "codex-fast",
            RawHarnessConfig {
                adapter: Some("codex".to_string()),
                ..RawHarnessConfig::default()
            },
        )
        .unwrap_err();

        assert!(error.contains("fixed harness ID 'codex'"), "{error}");
    }

    #[test]
    fn harness_config_writer_preserves_comments_and_root_tables() {
        let input = "# keep me\ndefault_harness = \"opencode\" # selected\n\n[ui]\nicon_style = \"unicode\"\n";

        let updated = update_user_harness_config_text(input, "codex", None).unwrap();
        let parsed = updated.parse::<toml_edit::DocumentMut>().unwrap();

        assert_eq!(parsed["default_harness"].as_str(), Some("codex"));
        assert_eq!(parsed["ui"]["icon_style"].as_str(), Some("unicode"));
        assert!(updated.contains("# keep me"));
        assert!(updated.contains("# selected"));
    }

    #[test]
    fn harness_config_writer_adds_validated_generic_harness_and_selects_it() {
        let generic = HarnessConfig {
            adapter: "generic".to_string(),
            interactive_command: vec!["company-agent".to_string()],
            arguments: Vec::new(),
            interactive_prompt_transport: None,
            headless_command: Some(vec!["company-agent".to_string(), "run".to_string()]),
            headless_prompt_transport: Some(PromptTransport::Stdin),
            output_format: OutputFormat::Text,
            environment: BTreeMap::new(),
        };

        let updated = update_user_harness_config_text(
            "[ui]\nicon_style = \"unicode\"\n",
            "company-agent",
            Some(&generic),
        )
        .unwrap();
        let parsed = toml::from_str::<RawConfig>(&updated).unwrap();
        let raw = parsed.harnesses.unwrap().remove("company-agent").unwrap();
        let parsed_generic = harness_config_from_raw("company-agent", raw).unwrap();

        assert_eq!(parsed.default_harness.as_deref(), Some("company-agent"));
        assert_eq!(parsed_generic, generic);
    }

    #[test]
    fn harness_config_writer_extends_an_inline_harnesses_table() {
        let generic = HarnessConfig {
            adapter: "generic".to_string(),
            interactive_command: vec!["second-agent".to_string()],
            arguments: Vec::new(),
            interactive_prompt_transport: None,
            headless_command: None,
            headless_prompt_transport: None,
            output_format: OutputFormat::Text,
            environment: BTreeMap::new(),
        };
        let input = "harnesses = { first = { adapter = \"generic\", interactive_command = [\"first-agent\"] } }\n";

        let updated = update_user_harness_config_text(input, "second", Some(&generic)).unwrap();
        let parsed = toml::from_str::<RawConfig>(&updated).unwrap();
        let harnesses = parsed.harnesses.unwrap();

        assert!(harnesses.contains_key("first"));
        assert!(harnesses.contains_key("second"));
    }

    #[test]
    #[cfg(unix)]
    fn harness_config_writer_preserves_a_symlinked_user_config() {
        use std::os::unix::fs::symlink;

        let directory = std::env::temp_dir().join(format!(
            "prism-harness-config-symlink-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let target = directory.join("managed.toml");
        let link = directory.join("config.toml");
        fs::write(&target, "default_harness = \"opencode\"\n").unwrap();
        symlink(&target, &link).unwrap();

        update_user_harness_config(&link, "codex", None).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            fs::read_to_string(&target)
                .unwrap()
                .contains("default_harness = \"codex\"")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn session_specific_config_does_not_change_global_default() {
        let mut config = Config::defaults(
            PathBuf::from("/tmp/user.toml"),
            PathBuf::from("/tmp/repo.toml"),
        );
        config.harnesses.insert(
            "codex".to_string(),
            HarnessConfig {
                adapter: "codex".to_string(),
                interactive_command: vec!["codex".to_string()],
                arguments: Vec::new(),
                interactive_prompt_transport: None,
                headless_command: None,
                headless_prompt_transport: None,
                output_format: OutputFormat::JsonLines,
                environment: BTreeMap::new(),
            },
        );
        let selected = config.for_harness("codex").unwrap();
        assert_eq!(selected.default_harness, "codex");
        assert_eq!(selected.default_agent, "codex");
        assert_eq!(config.default_harness, "opencode");
        assert!(
            config
                .for_harness("missing")
                .unwrap_err()
                .contains("migrate")
        );
        assert!(config.recorded_harness_config("codex", "codex").is_ok());
        config.harnesses.get_mut("codex").unwrap().adapter = "generic".to_string();
        assert!(
            config
                .recorded_harness_config("codex", "codex")
                .unwrap_err()
                .contains("now configured as 'generic'")
        );
    }

    #[test]
    fn harness_configuration_source_distinguishes_defaults_from_user_config() {
        let path = std::env::temp_dir().join(format!(
            "prism-harness-source-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config = Config::defaults(path.clone(), PathBuf::from("/tmp/repo.toml"));
        assert_eq!(harness_config_source(&config), "built-in defaults");

        fs::write(&path, "default_base = 'main'\n").unwrap();
        assert_eq!(harness_config_source(&config), "built-in defaults");

        fs::write(&path, "default_harness = 'opencode'\n").unwrap();
        assert_eq!(harness_config_source(&config), path.display().to_string());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn obsolete_agent_settings_report_the_source_and_replacements() {
        let path = std::env::temp_dir().join(format!(
            "prism-obsolete-agent-config-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &path,
            "default_agent = 'opencode'\n[tools]\nopencode = 'opencode'\n[agents.opencode]\ncommand = 'opencode run'\n",
        )
        .unwrap();
        let mut config = Config::defaults(path.clone(), PathBuf::from("/tmp/repo.toml"));

        config.apply_file(&path);
        let error = ensure_configured_default_agent(&config).unwrap_err();

        assert!(error.contains(&path.display().to_string()));
        assert!(error.contains("default_harness/[harnesses.*]"));
        assert!(error.contains("[harnesses.opencode].program"));
        let _ = fs::remove_file(path);
    }
}
