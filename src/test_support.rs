use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::{AutoConfig, Checks, Config, EscapeKey, IconStyle, LayoutConfig, MergeMethod};

static SHIM_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const TOOL_NAMES: [&str; 7] = ["wt", "gh", "git", "tmux", "lazygit", "fzf", "opencode"];

pub(crate) fn test_config() -> Config {
    let tools = TOOL_NAMES
        .into_iter()
        .map(|name| (name.to_string(), unconfigured_tool_path(name)))
        .collect();
    Config {
        default_harness: "opencode".to_string(),
        harnesses: BTreeMap::from([(
            "opencode".to_string(),
            crate::harness::HarnessConfig::opencode("opencode"),
        )]),
        config_errors: Vec::new(),
        default_agent: "ask".to_string(),
        default_base: None,
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
        agent_commands: BTreeMap::from([("ask".to_string(), unconfigured_tool_path("ask"))]),
        agent_prompt_modes: BTreeMap::new(),
        prompt_templates: BTreeMap::new(),
        user_path: PathBuf::from("/tmp/prism-test-user-config.toml"),
        repo_config_path: PathBuf::from("/tmp/prism-test-repo-config.toml"),
    }
}

fn unconfigured_tool_path(name: &str) -> String {
    format!("/__prism_test_unconfigured_tool__/{name}")
}

pub(crate) fn install_tool(
    config: &mut Config,
    directory: &Path,
    name: &str,
    contents: &str,
) -> PathBuf {
    let path = directory.join(name);
    write_executable(&path, contents);
    config
        .tools
        .insert(name.to_string(), path.display().to_string());
    if name == "opencode" {
        config.harnesses.insert(
            "opencode".to_string(),
            crate::harness::HarnessConfig::opencode(path.display().to_string()),
        );
    }
    path
}

pub(crate) fn use_real_tool(config: &mut Config, name: &str) {
    config.tools.insert(name.to_string(), name.to_string());
    if name == "opencode" {
        config.harnesses.insert(
            "opencode".to_string(),
            crate::harness::HarnessConfig::opencode(name),
        );
    }
}

pub(crate) fn write_executable(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let sequence = SHIM_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let staging = path.with_extension(format!("staging-{}-{sequence}", std::process::id()));
    fs::write(&staging, contents).unwrap();
    let mut permissions = fs::metadata(&staging).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&staging, permissions).unwrap();
    fs::rename(staging, path).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_fails_closed_for_every_configured_tool() {
        let config = test_config();

        for name in TOOL_NAMES {
            assert_eq!(config.tool(name), unconfigured_tool_path(name));
        }
        assert_eq!(config.agent_command("ask"), unconfigured_tool_path("ask"));
    }

    #[test]
    fn installing_a_tool_makes_the_expected_shim_explicit() {
        let directory = std::env::temp_dir().join(format!(
            "prism-test-support-{}-{}",
            std::process::id(),
            SHIM_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut config = test_config();

        let path = install_tool(&mut config, &directory, "git", "#!/bin/sh\nexit 0\n");

        assert_eq!(config.tool("git"), path.display().to_string());
        assert!(path.is_file());
        let _ = fs::remove_dir_all(directory);
    }
}
