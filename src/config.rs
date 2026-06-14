use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::agent::{PromptMode, agent_command_exists, builtin_prompt_mode, detected_agents};
use crate::process::{command_exists, command_version, run_capture};
use crate::repo::Repository;
use crate::session::discover_sessions;
use crate::util::prism_config_dir;

pub const AGENT_CANDIDATES: [&str; 1] = ["opencode"];

#[derive(Clone, Debug, Default)]
pub struct Checks {
    pub pre_pr: Vec<String>,
    pub pre_push: Vec<String>,
    pub review_fix: Vec<String>,
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

#[derive(Clone, Debug)]
pub struct Config {
    pub default_agent: String,
    pub default_base: Option<String>,
    pub plan_dir: String,
    pub review_packet_dir: String,
    pub worktree_command: String,
    pub escape_key: EscapeKey,
    pub checks: Checks,
    pub worktree_columns: Vec<String>,
    pub tools: BTreeMap<String, String>,
    pub agent_commands: BTreeMap<String, String>,
    pub agent_prompt_modes: BTreeMap<String, PromptMode>,
    pub user_path: PathBuf,
    pub repo_config_path: PathBuf,
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
            ("opencode", "opencode"),
            ("codex_plan", "codex-plan"),
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
            escape_key: EscapeKey::EscEsc,
            checks: Checks::default(),
            worktree_columns: vec!["url".to_string()],
            tools,
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            user_path,
            repo_config_path,
        }
    }

    fn apply_file(&mut self, path: &Path) {
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        let mut section = String::new();
        for raw in text.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = line.trim_matches(&['[', ']'][..]).to_string();
                continue;
            }
            let Some((key, raw_value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let raw_value = raw_value.trim();

            if section == "checks" {
                let Some(values) = parse_toml_string_array(raw_value) else {
                    continue;
                };
                match key {
                    "pre_pr" => self.checks.pre_pr = values,
                    "pre_push" => self.checks.pre_push = values,
                    "review_fix" => self.checks.review_fix = values,
                    _ => {}
                }
                continue;
            }

            if section == "worktrees" {
                if key == "columns"
                    && let Some(values) = parse_toml_string_array(raw_value)
                {
                    self.worktree_columns = values;
                }
                continue;
            }

            let Some(value) = parse_toml_string(raw_value) else {
                continue;
            };
            if section == "tools" {
                self.tools.insert(key.to_string(), value);
                continue;
            }
            if let Some(agent) = section.strip_prefix("agents.") {
                match key {
                    "command" => {
                        self.agent_commands.insert(agent.to_string(), value);
                    }
                    "prompt_mode" => {
                        if let Some(mode) = PromptMode::parse(&value) {
                            self.agent_prompt_modes.insert(agent.to_string(), mode);
                        }
                    }
                    _ => {}
                }
                continue;
            }
            match key {
                "default_agent" => self.default_agent = value,
                "default_base" => self.default_base = Some(value),
                "plan_dir" => self.plan_dir = value,
                "review_packet_dir" => self.review_packet_dir = value,
                "worktree_command" => self.worktree_command = value,
                "escape_key" => {
                    if let Some(key) = EscapeKey::parse(&value) {
                        self.escape_key = key;
                    }
                }
                _ => {}
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

    pub fn is_default_branch(&self, branch: &str) -> bool {
        self.default_base
            .as_deref()
            .map(|base| !base.trim().is_empty() && branch == base)
            .unwrap_or(false)
    }
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
    println!("escape_key = {}", config.escape_key.label());
    println!("worktree_columns = {:?}", config.worktree_columns);
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
    print_tool_status("codex-plan", &config.tool("codex_plan"), false);
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

pub fn parse_toml_string(value: &str) -> Option<String> {
    let value = value.trim();
    if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
        Some(value[1..value.len() - 1].replace("\\\"", "\""))
    } else if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
        Some(value[1..value.len() - 1].to_string())
    } else {
        Some(value.to_string())
    }
}

pub fn parse_toml_string_array(value: &str) -> Option<Vec<String>> {
    let value = value.trim();
    if !value.starts_with('[') || !value.ends_with(']') {
        return None;
    }
    let inner = &value[1..value.len() - 1];
    let mut values = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            ',' => {
                if !current.trim().is_empty() {
                    values.push(current.trim().to_string());
                    current.clear();
                }
            }
            ch => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        values.push(current.trim().to_string());
    }
    Some(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_toml_string_array() {
        let values = parse_toml_string_array(r#"["cargo test", 'cargo fmt --check']"#).unwrap();
        assert_eq!(values, vec!["cargo test", "cargo fmt --check"]);
    }

    #[test]
    fn parses_escape_key() {
        assert_eq!(EscapeKey::parse("ctrl-space"), Some(EscapeKey::CtrlSpace));
        assert_eq!(EscapeKey::parse("esc-esc"), Some(EscapeKey::EscEsc));
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
