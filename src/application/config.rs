#![allow(
    dead_code,
    reason = "configuration exposes optional harness adapter capabilities"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use toml_edit::{Array, DocumentMut, Item, Table, value};

use crate::agent::{PromptMode, builtin_prompt_mode, detected_agents};
use crate::file_persistence::{self, BoxError, FileContents, UpdateOptions};
use crate::harness::{
    BUILTIN_HARNESS_IDS, Harness, HarnessConfig, OutputFormat, PromptTransport, builtin_adapter,
};
use crate::process::{command_exists, command_version};
use crate::remote::{HostIdentity, HostProfile, ProviderKind, RemoteBase, RemoteDiscovery};
use crate::repo::Repository;
use crate::session::discover_sessions;
use crate::util::prism_config_dir;

pub const AGENT_CANDIDATES: [&str; 1] = ["opencode"];
pub const CONFIG_SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/NathanaelRea/prism/main/schemas/config.schema.json";
pub const CONFIG_SCHEMA_JSON: &str = include_str!("../../schemas/config.schema.json");

pub fn config_example() -> String {
    format!("#:schema {CONFIG_SCHEMA_URL}\n")
        + r#"
# Prism config. Harness settings are global; other settings may be repository overrides.
default_harness = "opencode"
default_base = "main"
merge_method = "squash" # squash, merge, or rebase
escape_key = "esc-esc" # esc-esc or ctrl-space
review_packet_dir = ".agent/review"
worktree_command = "wt"
opencode_port_base = 41000
opencode_port_span = 1000
opencode_shutdown_owned_servers = false

[ui]
icon_style = "unicode" # or "nerd-font"

[notifications]
enabled = true
needs_input = true
completed = false
failed = true

[worktrees]
columns = []

[harnesses.opencode]
program = "opencode"

[tools]
gh = "gh"
glab = "glab"
git = "git"
tmux = "tmux"
wt = "wt"
lazygit = "lazygit"
fzf = "fzf"

# Self-hosted remotes must be mapped explicitly. github.com, gitlab.com, and
# codeberg.org have built-in profiles.
# [remote_hosts."git.example.com"]
# provider = "forgejo"
# credential_env = "FORGEJO_TOKEN" # variable name only; never put a token here

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayoutConfig {
    pub sidebar_width: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotificationConfig {
    pub enabled: bool,
    pub needs_input: bool,
    pub completed: bool,
    pub failed: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            needs_input: true,
            completed: false,
            failed: true,
        }
    }
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
    pub review_packet_dir: String,
    pub worktree_command: String,
    pub opencode_port_base: u16,
    pub opencode_port_span: u16,
    pub opencode_shutdown_owned_servers: bool,
    pub escape_key: EscapeKey,
    pub merge_method: MergeMethod,
    pub icon_style: IconStyle,
    pub icon_style_configured: bool,
    pub layout: LayoutConfig,
    pub notifications: NotificationConfig,
    pub worktree_columns: Vec<String>,
    pub tools: BTreeMap<String, String>,
    pub(crate) remote_hosts: BTreeMap<String, RemoteHostConfig>,
    pub agent_commands: BTreeMap<String, String>,
    pub agent_prompt_modes: BTreeMap<String, PromptMode>,
    pub prompt_templates: BTreeMap<String, String>,
    pub user_path: PathBuf,
    pub repo_config_path: PathBuf,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawConfig {
    default_harness: Option<String>,
    harnesses: Option<BTreeMap<String, RawHarnessConfig>>,
    default_agent: Option<String>,
    default_base: Option<String>,
    review_packet_dir: Option<String>,
    worktree_command: Option<String>,
    opencode_port_base: Option<u16>,
    opencode_port_span: Option<u16>,
    opencode_shutdown_owned_servers: Option<bool>,
    escape_key: Option<String>,
    merge_method: Option<String>,
    ui: Option<RawUiConfig>,
    layout: Option<RawLayoutConfig>,
    notifications: Option<RawNotificationConfig>,
    worktrees: Option<RawWorktrees>,
    tools: Option<BTreeMap<String, String>>,
    remote_hosts: Option<BTreeMap<String, RawRemoteHostConfig>>,
    agents: Option<BTreeMap<String, RawAgentConfig>>,
    prompt_templates: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteHostConfig {
    pub(crate) provider: ProviderKind,
    pub(crate) web_url: Option<String>,
    pub(crate) api_url: Option<String>,
    pub(crate) credential_env: Option<String>,
    pub(crate) allow_http: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRemoteHostConfig {
    provider: String,
    web_url: Option<String>,
    api_url: Option<String>,
    credential_env: Option<String>,
    allow_http: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawUiConfig {
    icon_style: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawLayoutConfig {
    sidebar_width: Option<u16>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawNotificationConfig {
    enabled: Option<bool>,
    needs_input: Option<bool>,
    completed: Option<bool>,
    failed: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawWorktrees {
    columns: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RawAgentConfig {
    command: Option<String>,
    prompt_mode: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
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

#[derive(Debug)]
enum ConfigDocumentError {
    Utf8(std::string::FromUtf8Error),
    Toml(toml::de::Error),
    Semantic(String),
}

impl fmt::Display for ConfigDocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Utf8(error) => write!(formatter, "config is unreadable text: {error}"),
            Self::Toml(error) => write!(formatter, "config has invalid TOML: {error}"),
            Self::Semantic(error) => write!(formatter, "config is semantically invalid: {error}"),
        }
    }
}

impl Error for ConfigDocumentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Utf8(error) => Some(error),
            Self::Toml(error) => Some(error),
            Self::Semantic(_) => None,
        }
    }
}

fn parse_and_validate_config(
    text: &str,
    is_user_config: bool,
) -> Result<RawConfig, ConfigDocumentError> {
    if text.trim().is_empty() {
        return Ok(RawConfig::default());
    }
    let value = toml::from_str::<toml::Value>(text).map_err(ConfigDocumentError::Toml)?;
    let raw = value
        .try_into::<RawConfig>()
        .map_err(|error| ConfigDocumentError::Semantic(error.to_string()))?;
    validate_config_values(&raw, is_user_config).map_err(ConfigDocumentError::Semantic)?;
    Ok(raw)
}

fn validate_config_values(raw: &RawConfig, is_user_config: bool) -> Result<(), String> {
    if raw.opencode_port_span == Some(0) {
        return Err("opencode_port_span must be greater than zero".to_string());
    }
    if let Some(value) = raw.merge_method.as_deref()
        && MergeMethod::parse(value).is_none()
    {
        return Err(format!("merge_method has unsupported value '{value}'"));
    }
    if let Some(value) = raw.escape_key.as_deref()
        && EscapeKey::parse(value).is_none()
    {
        return Err(format!("escape_key has unsupported value '{value}'"));
    }
    if let Some(value) = raw.ui.as_ref().and_then(|ui| ui.icon_style.as_deref())
        && IconStyle::parse(value).is_none()
    {
        return Err(format!("ui.icon_style has unsupported value '{value}'"));
    }
    if let Some(harnesses) = &raw.harnesses {
        for (id, harness) in harnesses {
            harness_config_from_raw(id, harness.clone())?;
        }
    }
    if let Some(hosts) = &raw.remote_hosts {
        for (hostname, host) in hosts {
            remote_host_from_raw(hostname, host.clone())?;
        }
    }
    if !is_user_config && (raw.default_harness.is_some() || raw.harnesses.is_some()) {
        return Err(
            "repository config cannot contain default_harness or [harnesses.*]".to_string(),
        );
    }
    Ok(())
}

fn validate_config_for_mutation(raw: &RawConfig, is_user_config: bool) -> Result<(), String> {
    if raw.default_agent.is_some() || raw.agents.is_some() {
        return Err(
            "obsolete default_agent/[agents.*] settings must be replaced before Prism can update this file"
                .to_string(),
        );
    }
    if raw
        .tools
        .as_ref()
        .is_some_and(|tools| tools.contains_key("opencode"))
    {
        return Err(
            "obsolete [tools].opencode must be replaced with [harnesses.opencode].program before Prism can update this file"
                .to_string(),
        );
    }
    if is_user_config
        && let Some(default_harness) = raw.default_harness.as_deref()
        && builtin_adapter(default_harness).is_none()
        && !raw
            .harnesses
            .as_ref()
            .is_some_and(|harnesses| harnesses.contains_key(default_harness))
    {
        return Err(format!(
            "default_harness '{default_harness}' has no matching harness configuration"
        ));
    }
    Ok(())
}

pub(crate) fn update_config_file(
    path: &Path,
    is_user_config: bool,
    transform: impl FnOnce(&str, bool) -> Result<String, String>,
) -> Result<(), String> {
    file_persistence::update(path, UpdateOptions::important_toml(), |contents| {
        let missing = matches!(contents, FileContents::Missing);
        let text = match contents {
            FileContents::Missing => String::new(),
            FileContents::Present(bytes) => String::from_utf8(bytes)
                .map_err(|error| Box::new(ConfigDocumentError::Utf8(error)) as BoxError)?,
        };
        if !text.trim().is_empty() {
            let raw = parse_and_validate_config(&text, is_user_config)
                .map_err(|error| Box::new(error) as BoxError)?;
            validate_config_for_mutation(&raw, is_user_config)
                .map_err(|error| Box::new(ConfigDocumentError::Semantic(error)) as BoxError)?;
        }
        let updated = transform(&text, missing)
            .map_err(|error| Box::new(ConfigDocumentError::Semantic(error)) as BoxError)?;
        if !updated.trim().is_empty() {
            let raw = parse_and_validate_config(&updated, is_user_config)
                .map_err(|error| Box::new(error) as BoxError)?;
            validate_config_for_mutation(&raw, is_user_config)
                .map_err(|error| Box::new(ConfigDocumentError::Semantic(error)) as BoxError)?;
        }
        let replacement = (updated != text).then(|| updated.into_bytes());
        Ok(((), replacement))
    })
    .map_err(|error| error.to_string())
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
            ("glab", "glab"),
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
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            opencode_port_base: 41_000,
            opencode_port_span: 1_000,
            opencode_shutdown_owned_servers: false,
            escape_key: EscapeKey::EscEsc,
            merge_method: MergeMethod::Squash,
            icon_style: IconStyle::Unicode,
            icon_style_configured: false,
            layout: LayoutConfig::default(),
            notifications: NotificationConfig::default(),
            worktree_columns: Vec::new(),
            tools,
            remote_hosts: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            prompt_templates: BTreeMap::new(),
            user_path,
            repo_config_path,
        }
    }

    fn apply_file(&mut self, path: &Path) {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                self.config_errors
                    .push(format!("read {}: {error}", path.display()));
                return;
            }
        };
        let is_user_config = path == self.user_path;
        let raw = match parse_and_validate_config(&text, is_user_config) {
            Ok(raw) => raw,
            Err(error) => {
                self.config_errors
                    .push(format!("load {}: {error}", path.display()));
                return;
            }
        };
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
        if let Some(layout) = raw.layout
            && let Some(width) = layout.sidebar_width
        {
            self.layout.sidebar_width = Some(width.clamp(20, 120));
        }
        if let Some(notifications) = raw.notifications {
            if let Some(enabled) = notifications.enabled {
                self.notifications.enabled = enabled;
            }
            if let Some(enabled) = notifications.needs_input {
                self.notifications.needs_input = enabled;
            }
            if let Some(enabled) = notifications.completed {
                self.notifications.completed = enabled;
            }
            if let Some(enabled) = notifications.failed {
                self.notifications.failed = enabled;
            }
        }
        if let Some(worktrees) = raw.worktrees
            && let Some(values) = worktrees.columns
        {
            self.worktree_columns = values;
        }
        if let Some(tools) = raw.tools {
            self.tools.extend(tools);
        }
        if let Some(hosts) = raw.remote_hosts {
            for (hostname, host) in hosts {
                match remote_host_from_raw(&hostname, host) {
                    Ok(host) => {
                        self.remote_hosts.insert(hostname, host);
                    }
                    Err(error) => self.config_errors.push(error),
                }
            }
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

    pub(crate) fn needs_initial_harness_setup(&self) -> bool {
        match fs::read_to_string(&self.user_path) {
            Ok(text) => toml::from_str::<RawConfig>(&text)
                .is_ok_and(|config| config.default_harness.is_none()),
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        }
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

    pub(crate) fn remote_discovery(&self) -> Result<RemoteDiscovery, String> {
        let profiles = self
            .remote_hosts
            .iter()
            .map(|(hostname, host)| host.profile(hostname))
            .collect::<Result<Vec<_>, _>>()?;
        RemoteDiscovery::new(profiles).map_err(|error| error.to_string())
    }

    pub(crate) fn remote_api_override(&self, host: &HostIdentity) -> Option<String> {
        self.remote_hosts
            .get(host.hostname())
            .and_then(|config| config.api_url.clone())
    }
}

fn remote_host_from_raw(
    hostname: &str,
    raw: RawRemoteHostConfig,
) -> Result<RemoteHostConfig, String> {
    HostIdentity::new(hostname, None)
        .map_err(|error| format!("remote host '{hostname}': {error}"))?;
    let provider = ProviderKind::parse(&raw.provider).ok_or_else(|| {
        format!(
            "remote host '{hostname}' has unsupported provider '{}'; expected github, gitlab, or forgejo",
            raw.provider
        )
    })?;
    let config = RemoteHostConfig {
        provider,
        web_url: raw.web_url,
        api_url: raw.api_url,
        credential_env: raw.credential_env,
        allow_http: raw.allow_http.unwrap_or(false),
    };
    if config.credential_env.is_some() && provider != ProviderKind::Forgejo {
        return Err(format!(
            "remote host '{hostname}' credential_env is supported only by the Forgejo adapter"
        ));
    }
    let profile = config.profile(hostname)?;
    if provider != ProviderKind::Forgejo {
        if config.web_url.is_some()
            && (profile.web_base.host() != &profile.host
                || !profile.web_base.path_prefix().is_empty()
                || profile.web_base.scheme() != crate::remote::WebScheme::Https)
        {
            return Err(format!(
                "remote host '{hostname}' web_url cannot be routed safely by the {} CLI; omit it or use https://{hostname}",
                provider.config_label()
            ));
        }
        if provider == ProviderKind::GitHub
            && config.api_url.is_some()
            && !matches!(profile.api_base.path_prefix(), "" | "/api/v3")
        {
            return Err(format!(
                "remote host '{hostname}' GitHub api_url must end at the API root or /api/v3 so Prism can derive the GraphQL endpoint"
            ));
        }
    }
    Ok(config)
}

impl RemoteHostConfig {
    fn profile(&self, hostname: &str) -> Result<HostProfile, String> {
        let host = HostIdentity::new(hostname, None).map_err(|error| error.to_string())?;
        let mut profile = HostProfile::new(host, self.provider)
            .map_err(|error| error.to_string())?
            .with_http_allowed(self.allow_http);
        let web_base = self
            .web_url
            .as_deref()
            .map(|url| RemoteBase::parse(url, self.allow_http))
            .transpose()
            .map_err(|error| format!("remote host '{hostname}' web_url: {error}"))?
            .unwrap_or_else(|| profile.web_base.clone());
        let api_base = self
            .api_url
            .as_deref()
            .map(|url| RemoteBase::parse(url, self.allow_http))
            .transpose()
            .map_err(|error| format!("remote host '{hostname}' api_url: {error}"))?
            .unwrap_or_else(|| profile.api_base.clone());
        profile = profile.with_bases(web_base, api_base);
        if let Some(name) = &self.credential_env {
            profile = profile
                .with_credential_environment(name)
                .map_err(|error| format!("remote host '{hostname}' credential_env: {error}"))?;
        }
        Ok(profile)
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
    update_config_file(path, true, |text, _| {
        update_user_harness_config_text(text, default_harness, generic)
    })
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
    update_config_file(path, true, |text, _| {
        let mut text = text.to_string();
        if text.contains("icon_style") {
            return Ok(text);
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
        Ok(text)
    })
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
    println!("review_packet_dir = {}", config.review_packet_dir);
    println!("worktree_command = {}", config.worktree_command);
    println!("opencode_port_base = {}", config.opencode_port_base);
    println!("opencode_port_span = {}", config.opencode_port_span);
    println!(
        "opencode_shutdown_owned_servers = {}",
        config.opencode_shutdown_owned_servers
    );
    println!("escape_key = {}", config.escape_key.label());
    println!("merge_method = {}", config.merge_method.label());
    println!("ui.icon_style = {}", config.icon_style.label());
    println!("notifications.enabled = {}", config.notifications.enabled);
    println!(
        "notifications.needs_input = {}",
        config.notifications.needs_input
    );
    println!(
        "notifications.completed = {}",
        config.notifications.completed
    );
    println!("notifications.failed = {}", config.notifications.failed);
    println!(
        "layout.sidebar_width = {}",
        config
            .layout
            .sidebar_width
            .map(|width| width.to_string())
            .unwrap_or_default()
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
    println!("[remote_hosts]");
    for (hostname, host) in &config.remote_hosts {
        println!("{hostname}.provider = {}", host.provider.config_label());
        println!("{hostname}.allow_http = {}", host.allow_http);
        if let Some(web_url) = &host.web_url {
            println!("{hostname}.web_url = {web_url}");
        }
        if let Some(api_url) = &host.api_url {
            println!("{hostname}.api_url = {api_url}");
        }
        if let Some(environment) = &host.credential_env {
            println!("{hostname}.credential_env = {environment}");
        }
    }
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
                "managed workflows",
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
    print_tool_status("gh", &config.tool("gh"), false);
    print_tool_status("glab", &config.tool("glab"), false);
    print_tool_status("tmux", &config.tool("tmux"), true);
    print_worktrunk_status(repo, config);
    print_tool_status("fzf", &config.tool("fzf"), false);
    println!();

    println!();
    print_remote_doctor(repo, config);

    println!();
    print_workflow_doctor(repo);

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

fn print_workflow_doctor(repo: &Repository) {
    let global = crate::util::prism_config_dir();
    let repository = repo.root.join(".prism");
    let trusted =
        crate::repository_resources_are_trusted(&global, &repo.root, &repository).unwrap_or(false);
    match crate::PromptWorkflowCatalog::discover(&global, Some(&repository), trusted) {
        Ok(catalog) => {
            let workflows = catalog.list();
            println!("workflow definitions: {}", workflows.len());
            for workflow in workflows {
                println!(
                    "  {}  valid  {:?}  {}",
                    workflow.name,
                    workflow.scope,
                    workflow.path.display()
                );
            }
        }
        Err(diagnostics) => {
            println!("workflow catalog errors: {}", diagnostics.len());
            for diagnostic in diagnostics {
                println!(
                    "  {}  invalid: {}",
                    diagnostic.path.display(),
                    crate::util::single_line(&diagnostic.message)
                );
            }
        }
    }
}
fn modified_after(path: &Path, threshold: std::time::SystemTime) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() {
        return true;
    }
    if metadata.is_file() {
        return metadata
            .modified()
            .is_ok_and(|modified| modified > threshold);
    }
    std::fs::read_dir(path).is_ok_and(|entries| {
        entries
            .filter_map(Result::ok)
            .any(|entry| modified_after(&entry.path(), threshold))
    })
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

pub fn ensure_required_tools(
    repo: &Repository,
    config: &Config,
) -> Result<crate::worktrunk::WorktrunkVersion, String> {
    let mut required = vec![
        ("git", config.tool("git")),
        ("tmux", config.tool("tmux")),
        (
            config.worktree_command.as_str(),
            config.tool(&config.worktree_command),
        ),
    ];
    if let Ok(remote) = crate::remote::discover_git_remote(
        &repo.root,
        config,
        "origin",
        crate::remote::RemoteUrlKind::Fetch,
    ) {
        match remote.repository.id.provider() {
            ProviderKind::GitHub => required.push(("gh", config.tool("gh"))),
            ProviderKind::GitLab => required.push(("glab", config.tool("glab"))),
            ProviderKind::Forgejo => {}
        }
    }
    let missing = required
        .into_iter()
        .filter(|(_, command)| !command_exists(command))
        .map(|(label, command)| format!("{label} ({command})"))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "missing required tool(s): {}. Install them or configure [tools] in {} or {}",
            missing.join(", "),
            config.user_path.display(),
            config.repo_config_path.display()
        ));
    }
    crate::worktrunk::ensure_supported_version(config)
}

fn print_worktrunk_status(repo: &Repository, config: &Config) {
    let command = config.tool(&config.worktree_command);
    if !command_exists(&command) {
        println!(
            "missing {:12} {:18} required -",
            config.worktree_command, command
        );
        return;
    }
    match crate::worktrunk::detect_version(config) {
        Ok(version) => {
            let status = if version.supported() {
                "ok"
            } else {
                "unsupported"
            };
            println!(
                "{status:7} {:12} {command:18} required {} (supported >= {}; tested current {})",
                config.worktree_command,
                version.raw,
                crate::worktrunk::MINIMUM_VERSION,
                crate::worktrunk::TESTED_CURRENT_VERSION,
            );
            if version.supported() {
                match crate::worktrunk::observe_repository(repo, config) {
                    Ok(snapshot) => println!(
                        "worktrunk observation: fresh schema={} worktrees={}",
                        match snapshot.schema {
                            crate::worktrunk::WorktrunkSchema::V1 => 1,
                            crate::worktrunk::WorktrunkSchema::V2 => 2,
                        },
                        snapshot.by_path.len()
                    ),
                    Err(error) => println!(
                        "worktrunk observation: unavailable (never loaded) {}",
                        error.safe_summary()
                    ),
                }
            }
        }
        Err(error) => println!(
            "error   {:12} {command:18} required {error}",
            config.worktree_command
        ),
    }
}

fn print_remote_doctor(repo: &Repository, config: &Config) {
    for line in remote_doctor_lines(repo, config) {
        println!("{line}");
    }
}

fn remote_doctor_lines(repo: &Repository, config: &Config) -> Vec<String> {
    let mut lines = Vec::new();
    let remote = crate::remote::discover_git_remote(
        &repo.root,
        config,
        "origin",
        crate::remote::RemoteUrlKind::Fetch,
    );
    let Ok(remote) = remote else {
        lines.push(format!("remote: unavailable: {}", remote.unwrap_err()));
        return lines;
    };
    let repository = &remote.repository;
    let provider = repository.id.provider();
    lines.push(format!("remote provider: {provider}"));
    lines.push(format!("remote host: {}", repository.id.host()));
    lines.push(format!("remote project: {}", repository.id.project_path()));
    lines.push(format!(
        "remote transport: {}",
        match repository.api_base.scheme() {
            crate::remote::WebScheme::Http => "http",
            crate::remote::WebScheme::Https => "https",
        }
    ));
    let diagnostics = crate::remote::dispatcher::runtime_diagnostics(&repo.root, config);
    let capabilities = diagnostics
        .as_ref()
        .map(|diagnostics| diagnostics.capabilities.clone())
        .unwrap_or_default();
    lines.push(format!(
        "remote capabilities: list={} details={} policy={} create={} resolve={} merge={} ci_logs={} queue={}",
        capabilities.list_change_requests.label(),
        capabilities.change_request_details.label(),
        capabilities.repository_policy.label(),
        capabilities.create_change_request.label(),
        capabilities.resolve_review_thread.label(),
        capabilities.guarded_merge.label(),
        capabilities.ci_logs.label(),
        capabilities.merge_queue.label(),
    ));
    if let Some(reason) = &capabilities.guarded_merge_reason {
        lines.push(format!("remote merge unavailable: {reason}"));
    }
    if let Err(error) = &diagnostics {
        lines.push(format!("remote capabilities unavailable: {error}"));
    }
    lines.push(format!(
        "remote authentication: {}",
        crate::remote::dispatcher::authentication_status(&repo.root, config)
            .unwrap_or_else(|error| error)
    ));
    lines.push(format!(
        "remote server version: {}",
        diagnostics
            .map(|diagnostics| {
                diagnostics
                    .server_version
                    .unwrap_or_else(|| "not applicable".to_string())
            })
            .unwrap_or_else(|error| format!("unavailable: {error}"))
    ));
    lines
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
    update_user_harness_config(&config.user_path, selected, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_unix_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64
    }

    #[cfg(unix)]
    fn doctor_git_script(directory: &Path, remote: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = directory.join("git-doctor-test");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{}'\n",
                remote.replace('\'', "'\\''")
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[cfg(unix)]
    fn forgejo_doctor_server() -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let length = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..length]);
                let body = if request.starts_with("GET /api/v1/version ") {
                    r#"{"version":"11.0.1"}"#
                } else if request.starts_with("GET /api/v1/settings/api ") {
                    r#"{"max_response_items":50,"default_paging_num":30}"#
                } else if request.starts_with("GET /api/v1/repos/acme/widget ") {
                    r#"{"id":1,"full_name":"acme/widget","has_actions":false}"#
                } else {
                    panic!("unexpected doctor request: {request}");
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (format!("http://{address}/api/v1"), worker)
    }

    #[cfg(unix)]
    #[test]
    fn doctor_reports_configured_transport_and_runtime_forgejo_capabilities() {
        let directory = std::env::temp_dir().join(format!(
            "prism-doctor-forgejo-{}-{}",
            std::process::id(),
            test_unix_ms()
        ));
        fs::create_dir_all(&directory).unwrap();
        let git = doctor_git_script(&directory, "http://forge.test/acme/widget.git");
        let (api_url, worker) = forgejo_doctor_server();
        let repo =
            Repository::with_config_dir_for_test(directory.clone(), directory.join("config"));
        let mut config = Config::defaults(directory.join("user.toml"), directory.join("repo.toml"));
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        config.remote_hosts.insert(
            "forge.test".to_string(),
            RemoteHostConfig {
                provider: ProviderKind::Forgejo,
                web_url: Some("http://forge.test".to_string()),
                api_url: Some(api_url),
                credential_env: None,
                allow_http: true,
            },
        );

        let report = remote_doctor_lines(&repo, &config).join("\n");
        worker.join().unwrap();

        assert!(report.contains("remote transport: http"));
        assert!(report.contains("create=supported"));
        assert!(report.contains("merge=supported"));
        assert!(report.contains("ci_logs=unsupported"));
        assert!(report.contains("remote server version: 11.0.1"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn doctor_reports_unknown_runtime_capabilities_with_safe_failure_reason() {
        let directory = std::env::temp_dir().join(format!(
            "prism-doctor-forgejo-unavailable-{}-{}",
            std::process::id(),
            test_unix_ms()
        ));
        fs::create_dir_all(&directory).unwrap();
        let git = doctor_git_script(&directory, "ssh://git@forge.test/acme/widget.git");
        let repo =
            Repository::with_config_dir_for_test(directory.clone(), directory.join("config"));
        let mut config = Config::defaults(directory.join("user.toml"), directory.join("repo.toml"));
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        config.remote_hosts.insert(
            "forge.test".to_string(),
            RemoteHostConfig {
                provider: ProviderKind::Forgejo,
                web_url: None,
                api_url: Some("https://127.0.0.1:9/api/v1".to_string()),
                credential_env: Some("FORGEJO_DOCTOR_TOKEN".to_string()),
                allow_http: false,
            },
        );

        let report = remote_doctor_lines(&repo, &config).join("\n");

        assert!(report.contains("remote transport: https"));
        assert!(report.contains("create=unknown"));
        assert!(report.contains("remote capabilities unavailable:"));
        assert!(report.contains("remote server version: unavailable:"));
        assert!(!report.contains("super-secret-token"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn doctor_reports_gitlab_rebase_merge_as_unavailable() {
        let directory = std::env::temp_dir().join(format!(
            "prism-doctor-gitlab-rebase-{}-{}",
            std::process::id(),
            test_unix_ms()
        ));
        fs::create_dir_all(&directory).unwrap();
        let git = doctor_git_script(&directory, "git@gitlab.com:acme/widget.git");
        let repo =
            Repository::with_config_dir_for_test(directory.clone(), directory.join("config"));
        let mut config = Config::defaults(directory.join("user.toml"), directory.join("repo.toml"));
        config.merge_method = MergeMethod::Rebase;
        config
            .tools
            .insert("git".to_string(), git.display().to_string());

        let report = remote_doctor_lines(&repo, &config).join("\n");

        assert!(report.contains("merge=unsupported"));
        assert!(
            report.contains(
                "remote merge unavailable: GitLab adapter does not support rebase merges"
            )
        );
        fs::remove_dir_all(directory).unwrap();
    }

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
        assert_eq!(
            config.notifications,
            NotificationConfig {
                enabled: true,
                needs_input: true,
                completed: false,
                failed: true,
            }
        );
        assert!(config.worktree_columns.is_empty());
        assert_eq!(config.opencode_port_base, 41_000);
        assert_eq!(config.opencode_port_span, 1_000);
        assert!(!config.opencode_shutdown_owned_servers);
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
    fn notification_config_merges_field_by_field() {
        let mut config = Config::defaults(
            PathBuf::from("/tmp/user.toml"),
            PathBuf::from("/tmp/repo.toml"),
        );
        config.apply_raw_config(
            RawConfig {
                notifications: Some(RawNotificationConfig {
                    enabled: Some(true),
                    completed: Some(false),
                    ..RawNotificationConfig::default()
                }),
                ..RawConfig::default()
            },
            true,
        );
        config.apply_raw_config(
            RawConfig {
                notifications: Some(RawNotificationConfig {
                    completed: Some(true),
                    failed: Some(false),
                    ..RawNotificationConfig::default()
                }),
                ..RawConfig::default()
            },
            false,
        );

        assert_eq!(
            config.notifications,
            NotificationConfig {
                enabled: true,
                needs_input: true,
                completed: true,
                failed: false,
            }
        );
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
            "opencode_port_base = 42000\nopencode_port_span = 50\nopencode_shutdown_owned_servers = true\n",
        )
        .unwrap();
        let mut config = Config::defaults(PathBuf::from("/tmp/user.toml"), path.clone());

        config.apply_file(&path);

        assert_eq!(config.opencode_port_base, 42_000);
        assert_eq!(config.opencode_port_span, 50);
        assert!(config.opencode_shutdown_owned_servers);

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
    fn generated_mutation_does_not_overwrite_invalid_user_config() {
        let directory = std::env::temp_dir().join(format!(
            "prism-config-invalid-mutation-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.toml");
        let invalid = "default_harness = [\n";
        fs::write(&path, invalid).unwrap();

        let error = save_user_icon_style(&path, IconStyle::NerdFont).unwrap_err();

        assert!(error.contains(&path.display().to_string()));
        assert!(error.contains("invalid TOML"));
        assert_eq!(fs::read_to_string(&path).unwrap(), invalid);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn generated_mutation_does_not_overwrite_semantically_invalid_repo_config() {
        let directory = std::env::temp_dir().join(format!(
            "prism-repo-config-invalid-mutation-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.toml");
        let invalid = "merge_method = 'explode'\n";
        fs::write(&path, invalid).unwrap();

        let error = update_config_file(&path, false, |text, _| {
            Ok(format!("{text}[worktrees]\ncolumns = []\n"))
        })
        .unwrap_err();

        assert!(error.contains("semantically invalid"));
        assert_eq!(fs::read_to_string(&path).unwrap(), invalid);
        fs::remove_dir_all(directory).unwrap();
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

    #[test]
    fn remote_host_configuration_is_typed_and_inherited_by_hostname() {
        let mut config = Config::defaults(
            PathBuf::from("/tmp/user.toml"),
            PathBuf::from("/tmp/repo.toml"),
        );
        let user = parse_and_validate_config(
            r#"
[remote_hosts."git.example.com"]
provider = "forgejo"
credential_env = "FORGEJO_TOKEN"
"#,
            true,
        )
        .unwrap();
        config.apply_raw_config(user, true);

        let repository = config
            .remote_discovery()
            .unwrap()
            .discover("git@git.example.com:Team/Project.git")
            .unwrap()
            .repository;

        assert_eq!(repository.id.provider(), ProviderKind::Forgejo);
        assert_eq!(repository.id.project_path(), "Team/Project");
        assert_eq!(
            config.remote_hosts["git.example.com"]
                .credential_env
                .as_deref(),
            Some("FORGEJO_TOKEN")
        );
    }

    #[test]
    fn remote_host_configuration_rejects_secrets_and_insecure_bases_by_shape() {
        let token_error = parse_and_validate_config(
            r#"
[remote_hosts."git.example.com"]
provider = "forgejo"
credential_env = "actual token value"
"#,
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(token_error.contains("credential_env"), "{token_error}");

        let http_error = parse_and_validate_config(
            r#"
[remote_hosts."git.example.com"]
provider = "gitlab"
web_url = "http://git.example.com"
"#,
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(http_error.contains("allow_http"), "{http_error}");
    }

    #[test]
    fn github_and_gitlab_api_overrides_validate_supported_cli_shapes() {
        parse_and_validate_config(
            r#"
[remote_hosts."github.example.com"]
provider = "github"
api_url = "https://api.example.com/api/v3"

[remote_hosts."gitlab.example.com"]
provider = "gitlab"
api_url = "https://api.example.com/gitlab/api/v4"
"#,
            true,
        )
        .unwrap();

        let github_web = parse_and_validate_config(
            r#"
[remote_hosts."github.example.com"]
provider = "github"
web_url = "https://proxy.example.com/github"
"#,
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(
            github_web.contains("cannot be routed safely"),
            "{github_web}"
        );

        let github_api = parse_and_validate_config(
            r#"
[remote_hosts."github.example.com"]
provider = "github"
api_url = "https://api.example.com/custom/rest"
"#,
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(github_api.contains("GraphQL endpoint"), "{github_api}");
    }
}
