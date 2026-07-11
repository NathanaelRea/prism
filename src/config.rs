use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::agent::{PromptMode, agent_command_exists, builtin_prompt_mode, detected_agents};
use crate::process::{command_exists, command_version, run_capture};
use crate::repo::Repository;
use crate::session::discover_sessions;
use crate::util::prism_config_dir;

pub const AGENT_CANDIDATES: [&str; 1] = ["opencode"];
pub const CONFIG_SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/NathanaelRea/prism/main/schemas/config.schema.json";
pub const CONFIG_SCHEMA_JSON: &str = include_str!("../schemas/config.schema.json");

pub fn config_example() -> String {
    format!("#:schema {CONFIG_SCHEMA_URL}\n").to_string()
        + r#"
# Prism config. Use this file globally or as a repository override.
default_agent = "opencode"
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

[tools]
opencode = "opencode"
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

[agents.opencode]
command = "opencode run --format json"
prompt_mode = "argument"

[prompt_templates]
auto_create_plan = "Create an implementation plan at `{{plan_path}}`. Do not implement or commit. Include phases, tests, verification, risks, observability, and architecture fit.\n\nTask:\n{{task}}\n\nMode: {{mode}}\nVariant: {{variant}}\nAgent profile: {{agent_profile}}"
auto_review_plan = "Review `{{plan_path}}` and edit it in place. Do not implement or commit. Check phases, risks, tests, observability, restartability, safety, and architecture fit.\n\nTask:\n{{task}}"
auto_implement = "Implement this task in the current worktree. Stop after implementation; do not commit, push, create a pull request, or merge.\n\nTask:\n{{task}}"
auto_fix_local_verify = "Fix the local verification failures, then stop without committing.\n\nOriginal task:\n{{task}}\n\nFailure context:\n{{context}}"
auto_fix_review = "Resolve the review feedback, then stop without committing.\n\nOriginal task:\n{{task}}\n\nReview context:\n{{context}}"
auto_fix_ci = "Fix the CI failure, then stop without committing.\n\nOriginal task:\n{{task}}\n\nCI context:\n{{context}}"
review_fix = "Here are review comments on PR {{pr_number}}.\n\nIf they are applicable, fix them. Otherwise, say why not.\n\n---\n\n{{comments}}"
ci_failure = "Here are CI failures on PR {{pr_number}}.\n\nFix the failing checks. Use the log tails below as the primary clues.\n\nPR: {{url}}\nBranch: {{branch}}\nHead SHA: {{head_sha}}\n\n---\n\n{{failures}}"
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

impl Config {
    pub fn load(repo: &Repository) -> Self {
        let user_path = prism_config_dir().join("config.toml");
        let repo_config_path = repo.prism_dir().join("config.toml");
        let mut config = Self::defaults(user_path, repo_config_path);

        let user_path = config.user_path.clone();
        config.apply_file(&user_path);
        let repo_config_path = config.repo_config_path.clone();
        config.apply_file(&repo_config_path);
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

        Self {
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
        let Ok(raw) = toml::from_str::<RawConfig>(&text) else {
            return;
        };
        self.apply_raw_config(raw);
    }

    fn apply_raw_config(&mut self, raw: RawConfig) {
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
    println!("default_agent = {}", config.default_agent);
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
    if !config.agent_commands.is_empty() {
        println!("[agents]");
        for (key, value) in &config.agent_commands {
            println!("{key}.command = {value}");
            println!(
                "{key}.prompt_mode = {}",
                config.agent_prompt_mode(key).label()
            );
        }
    }
}

pub fn doctor(repo: &Repository, config: &mut Config) -> Result<(), String> {
    println!("Prism doctor");
    println!("repo: {}", repo.root.display());
    println!("user config: {}", config.user_path.display());
    println!("repo config: {}", config.repo_config_path.display());
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

    let detected = detected_agents(config);
    if detected.is_empty() {
        println!("agents: none found");
        println!(
            "Install or configure one of: {}",
            AGENT_CANDIDATES.join(", ")
        );
    } else {
        println!("agents:");
        for agent in &detected {
            println!("  ok {agent} ({})", config.agent_prompt_mode(agent).label());
        }
    }

    if config.default_agent == "ask" {
        if let Some(agent) = detected.first() {
            println!("default agent: ask (would select {agent} on first interactive run)");
        } else {
            println!("default agent: ask (blocked until an agent is configured)");
        }
    } else {
        if config.default_agent == "opencode" {
            let exists = agent_command_exists(config, &config.default_agent);
            println!(
                "default agent: {} ({})",
                config.default_agent,
                if exists { "available" } else { "missing" }
            );
        } else {
            println!("default agent: {} (unsupported)", config.default_agent);
        }
    }

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
    if config.default_agent != "opencode" {
        return Err(format!(
            "unsupported default_agent '{}'; Prism uses opencode so it can observe agent status and output",
            config.default_agent
        ));
    }
    if agent_command_exists(config, &config.default_agent) {
        return Ok(());
    }
    Err(format!(
        "configured default_agent '{}' was not found on PATH",
        config.default_agent
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
    fn config_toml_supports_comments_escaped_strings_arrays_and_agent_tables() {
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
default_agent = "opencode"
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

[agents.opencode]
command = "opencode run --format json"
prompt_mode = "argument"

[prompt_templates]
review = "fix\nreview"
"#,
        )
        .unwrap();
        let mut config = Config::defaults(PathBuf::from("/tmp/user.toml"), path.clone());

        config.apply_file(&path);

        assert_eq!(config.default_base.as_deref(), Some("release/main"));
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

        config.apply_raw_config(RawConfig {
            layout: Some(RawLayoutConfig {
                sidebar_width: Some(4),
            }),
            ..RawConfig::default()
        });
        assert_eq!(config.layout.sidebar_width, Some(20));

        config.apply_raw_config(RawConfig {
            layout: Some(RawLayoutConfig {
                sidebar_width: Some(999),
            }),
            ..RawConfig::default()
        });
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
    fn rejects_non_opencode_default_agent() {
        let mut config = Config::defaults(
            PathBuf::from("/tmp/user.toml"),
            PathBuf::from("/tmp/prism-repo-config.toml"),
        );
        config.default_agent = "other-agent".to_string();

        let error = ensure_configured_default_agent(&config).unwrap_err();

        assert!(error.contains("unsupported default_agent"));
        assert!(error.contains("opencode"));
    }
}
