use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::process::{run_capture, run_status, split_command_words};
use crate::repo::Repository;
use crate::session::Session;
use crate::util::{safe_branch_filename, stable_hash};

const EXISTING_SESSION_READY_WAIT: Duration = Duration::from_millis(250);
const CREATED_SESSION_READY_WAIT: Duration = Duration::from_millis(1_200);
const SESSION_READY_POLL_INTERVAL: Duration = Duration::from_millis(50);
const AGENT_INPUT_READY_WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TmuxWindow {
    Agent,
    LazyGit,
    Terminal,
}

impl TmuxWindow {
    fn index(self) -> u8 {
        match self {
            TmuxWindow::Agent => 1,
            TmuxWindow::LazyGit => 2,
            TmuxWindow::Terminal => 3,
        }
    }

    fn name(self, config: &Config) -> String {
        match self {
            TmuxWindow::Agent => config.default_agent.clone(),
            TmuxWindow::LazyGit => "lazygit".to_string(),
            TmuxWindow::Terminal => "terminal".to_string(),
        }
    }
}

pub fn attach_or_create_agent(
    repo: &Repository,
    config: &Config,
    session: &Session,
    generation: u64,
) -> Result<(), String> {
    let name = agent_session_name(repo, &session.branch, generation);
    ensure_agent_session(repo, config, session, generation)?;
    match attach(config, &name, TmuxWindow::Agent) {
        Ok(()) => Ok(()),
        Err(_) if matches!(session_exists(config, &name), Ok(false)) => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn attach_or_create_window(
    repo: &Repository,
    config: &Config,
    session: &Session,
    generation: u64,
    window: TmuxWindow,
) -> Result<(), String> {
    let name = agent_session_name(repo, &session.branch, generation);
    ensure_agent_session(repo, config, session, generation)?;
    match attach(config, &name, window) {
        Ok(()) => Ok(()),
        Err(_) if matches!(session_exists(config, &name), Ok(false)) => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn ensure_agent_session(
    repo: &Repository,
    config: &Config,
    session: &Session,
    generation: u64,
) -> Result<bool, String> {
    let name = agent_session_name(repo, &session.branch, generation);
    if session_exists(config, &name)? {
        if !configure_agent_session(config, &name)? {
            create_detached_agent_session(config, session, &name)?;
            configure_agent_session(config, &name)?;
            ensure_companion_windows(config, session, &name)?;
            return Ok(wait_for_agent_session_running(
                repo,
                config,
                session,
                generation,
                CREATED_SESSION_READY_WAIT,
            ));
        }
        if wait_for_agent_session_running(
            repo,
            config,
            session,
            generation,
            EXISTING_SESSION_READY_WAIT,
        ) {
            ensure_companion_windows(config, session, &name)?;
            return Ok(true);
        }
        kill_session(config, &name)?;
    }
    create_detached_agent_session(config, session, &name)?;
    configure_agent_session(config, &name)?;
    ensure_companion_windows(config, session, &name)?;
    Ok(wait_for_agent_session_running(
        repo,
        config,
        session,
        generation,
        CREATED_SESSION_READY_WAIT,
    ))
}

pub fn paste_agent_prompt(
    repo: &Repository,
    config: &Config,
    session: &Session,
    generation: u64,
    prompt: &str,
) -> Result<(), String> {
    let name = agent_session_name(repo, &session.branch, generation);
    if !ensure_agent_session(repo, config, session, generation)? {
        return Err("agent session did not become ready".to_string());
    }
    if !wait_for_agent_input_ready(config, &name, AGENT_INPUT_READY_WAIT) {
        return Err("agent prompt did not become ready".to_string());
    }
    let buffer_name = format!("{name}-prompt");
    run_tmux_status_with_stdin(
        Command::new(config.tool("tmux")).env_remove("TMUX").args([
            "load-buffer",
            "-b",
            &buffer_name,
            "-",
        ]),
        prompt,
    )?;
    run_tmux_status(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "paste-buffer",
        "-d",
        "-b",
        &buffer_name,
        "-t",
        &window_target(&name, TmuxWindow::Agent),
    ]))
}

pub fn agent_session_running(
    repo: &Repository,
    config: &Config,
    session: &Session,
    generation: u64,
) -> bool {
    let name = agent_session_name(repo, &session.branch, generation);
    if !matches!(session_exists(config, &name), Ok(true)) {
        return false;
    }
    pane_current_command(config, &window_target(&name, TmuxWindow::Agent))
        .map(|command| pane_command_matches_agent(config, &command))
        .unwrap_or(false)
}

pub fn kill_agent_session(
    repo: &Repository,
    config: &Config,
    branch: &str,
    generation: u64,
) -> Result<(), String> {
    let name = agent_session_name(repo, branch, generation);
    kill_session(config, &name)
}

pub fn agent_session_name(repo: &Repository, branch: &str, generation: u64) -> String {
    format!("{}{}", agent_session_prefix(repo, branch), generation)
}

pub fn latest_agent_session_generation(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Option<u64> {
    let prefix = agent_session_prefix(repo, branch);
    let output = run_capture(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "list-sessions",
        "-F",
        "#{session_name}",
    ]))
    .ok()?;
    output
        .lines()
        .filter_map(|name| name.strip_prefix(&prefix)?.parse::<u64>().ok())
        .max()
}

fn agent_session_prefix(repo: &Repository, branch: &str) -> String {
    let hash = stable_hash(repo.root.as_path());
    let branch = safe_tmux_name(&safe_branch_filename(branch));
    format!("prism-{hash:016x}-{branch}-")
}

fn attach(config: &Config, name: &str, window: TmuxWindow) -> Result<(), String> {
    run_status(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "attach-session",
        "-t",
        &window_target(name, window),
    ]))
}

fn create_detached_agent_session(
    config: &Config,
    session: &Session,
    name: &str,
) -> Result<(), String> {
    let command = agent_shell_command(config)?;
    run_tmux_status(
        Command::new(config.tool("tmux"))
            .env_remove("TMUX")
            .args(["new-session", "-d", "-s"])
            .arg(name)
            .args(["-n", &TmuxWindow::Agent.name(config)])
            .arg("-c")
            .arg(&session.path)
            .arg(command),
    )
}

fn configure_agent_session(config: &Config, name: &str) -> Result<bool, String> {
    match run_tmux_status(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "set-option",
        "-t",
        name,
        "detach-on-destroy",
        "on",
    ])) {
        Ok(()) => Ok(true),
        Err(error) if tmux_missing_session_error(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn ensure_companion_windows(config: &Config, session: &Session, name: &str) -> Result<(), String> {
    configure_window_indexing(config, name)?;
    move_initial_window_to_one(config, name)?;
    rename_window(config, name, TmuxWindow::Agent)?;
    ensure_window(config, session, name, TmuxWindow::LazyGit)?;
    ensure_window(config, session, name, TmuxWindow::Terminal)?;
    Ok(())
}

fn configure_window_indexing(config: &Config, name: &str) -> Result<(), String> {
    run_tmux_status(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "set-option",
        "-t",
        name,
        "base-index",
        "1",
    ]))?;
    run_tmux_status(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "set-option",
        "-t",
        name,
        "renumber-windows",
        "off",
    ]))
}

fn move_initial_window_to_one(config: &Config, name: &str) -> Result<(), String> {
    match run_tmux_status(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "move-window",
        "-s",
        &format!("{name}:0"),
        "-t",
        &window_target(name, TmuxWindow::Agent),
    ])) {
        Ok(()) => Ok(()),
        Err(error) if tmux_missing_session_error(&error) || error.contains("same index") => Ok(()),
        Err(error) => Err(error),
    }
}

fn rename_window(config: &Config, name: &str, window: TmuxWindow) -> Result<(), String> {
    match run_tmux_status(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "rename-window",
        "-t",
        &window_target(name, window),
        &window.name(config),
    ])) {
        Ok(()) => Ok(()),
        Err(error) if tmux_missing_session_error(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn ensure_window(
    config: &Config,
    session: &Session,
    name: &str,
    window: TmuxWindow,
) -> Result<(), String> {
    if window_exists(config, name, window)? {
        rename_window(config, name, window)?;
        return Ok(());
    }
    let command = match window {
        TmuxWindow::Agent => return Ok(()),
        TmuxWindow::LazyGit => config.tool("lazygit"),
        TmuxWindow::Terminal => std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string()),
    };
    run_tmux_status(
        Command::new(config.tool("tmux"))
            .env_remove("TMUX")
            .args(["new-window", "-d", "-t", &window_target(name, window)])
            .args(["-n", &window.name(config)])
            .arg("-c")
            .arg(&session.path)
            .arg(command),
    )
}

fn window_exists(config: &Config, name: &str, window: TmuxWindow) -> Result<bool, String> {
    Command::new(config.tool("tmux"))
        .env_remove("TMUX")
        .args(["list-windows", "-t", name, "-F", "#{window_index}"])
        .stderr(Stdio::piped())
        .output()
        .map(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line == window.index().to_string())
        })
        .map_err(|error| format!("tmux: {error}"))
}

fn window_target(name: &str, window: TmuxWindow) -> String {
    format!("{name}:{}", window.index())
}

fn kill_session(config: &Config, name: &str) -> Result<(), String> {
    match run_tmux_status(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "kill-session",
        "-t",
        name,
    ])) {
        Ok(()) => Ok(()),
        Err(error) if tmux_missing_session_error(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn session_exists(config: &Config, name: &str) -> Result<bool, String> {
    Command::new(config.tool("tmux"))
        .env_remove("TMUX")
        .args(["has-session", "-t", name])
        .stderr(Stdio::piped())
        .output()
        .map(|output| output.status.success())
        .map_err(|error| format!("tmux: {error}"))
}

fn wait_for_agent_session_running(
    repo: &Repository,
    config: &Config,
    session: &Session,
    generation: u64,
    timeout: Duration,
) -> bool {
    let started = Instant::now();
    loop {
        if agent_session_running(repo, config, session, generation) {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(SESSION_READY_POLL_INTERVAL);
    }
}

fn wait_for_agent_input_ready(config: &Config, name: &str, timeout: Duration) -> bool {
    if config.default_agent != "opencode" {
        return true;
    }
    let started = Instant::now();
    loop {
        if pane_capture(config, name)
            .map(|output| opencode_input_ready(&output))
            .unwrap_or(false)
        {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(SESSION_READY_POLL_INTERVAL);
    }
}

fn opencode_input_ready(output: &str) -> bool {
    output.contains("Ask anything") || output.contains("ctrl+p commands")
}

fn pane_capture(config: &Config, name: &str) -> Option<String> {
    run_capture(
        Command::new(config.tool("tmux"))
            .env_remove("TMUX")
            .args(["capture-pane", "-p", "-t"])
            .arg(name),
    )
    .ok()
}

fn run_tmux_status(command: &mut Command) -> Result<(), String> {
    let output = command
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("tmux: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        Err(format!("tmux exited with {}", output.status))
    } else {
        Err(stderr)
    }
}

fn run_tmux_status_with_stdin(command: &mut Command, stdin: &str) -> Result<(), String> {
    let mut child = command
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("tmux: {error}"))?;
    if let Some(mut child_stdin) = child.stdin.take() {
        child_stdin
            .write_all(stdin.as_bytes())
            .map_err(|error| format!("tmux: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("tmux: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        Err(format!("tmux exited with {}", output.status))
    } else {
        Err(stderr)
    }
}

fn tmux_missing_session_error(error: &str) -> bool {
    error.contains("can't find session")
        || error.contains("can't find window")
        || error.contains("can't find pane")
}

fn agent_shell_command(config: &Config) -> Result<String, String> {
    let argv = interactive_agent_argv(config);
    if argv.is_empty() {
        return Err(format!(
            "agent '{}' has an empty command",
            config.default_agent
        ));
    }
    if argv.iter().any(|arg| arg.contains("{prompt")) {
        return Err(format!(
            "agent '{}' command contains a prompt placeholder; configure an interactive command for tmux attach",
            config.default_agent
        ));
    }
    Ok(argv
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" "))
}

fn interactive_agent_argv(config: &Config) -> Vec<String> {
    if config.default_agent == "opencode" {
        vec![config.tool("opencode")]
    } else {
        split_command_words(&config.agent_command(&config.default_agent))
    }
}

fn pane_current_command(config: &Config, name: &str) -> Option<String> {
    run_capture(
        Command::new(config.tool("tmux"))
            .env_remove("TMUX")
            .args(["display-message", "-p", "-t"])
            .arg(name)
            .arg("#{pane_current_command}"),
    )
    .ok()
    .map(|output| output.trim().to_string())
    .filter(|output| !output.is_empty())
}

fn pane_command_matches_agent(config: &Config, pane_command: &str) -> bool {
    let Some(expected) = interactive_agent_argv(config).first().cloned() else {
        return false;
    };
    let expected = Path::new(&expected)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&expected);
    pane_command == expected
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':' | '='))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn safe_tmux_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::agent::AgentState;
    use crate::config::{Checks, Config, EscapeKey};
    use crate::github::PrCache;
    use crate::repo::Repository;
    use crate::session::Session;

    use super::{
        TmuxWindow, agent_session_name, attach_or_create_agent, attach_or_create_window,
        ensure_agent_session, latest_agent_session_generation, pane_command_matches_agent,
        paste_agent_prompt, shell_quote,
    };

    #[test]
    fn tmux_session_names_are_stable_and_safe() {
        let repo = Repository {
            root: PathBuf::from("/repo/my project"),
        };

        let name = agent_session_name(&repo, "feature/foo:bar", 3);

        assert!(name.starts_with("prism-"));
        assert!(name.ends_with("-feature_foo_bar-3"));
        assert!(!name.contains('/'));
        assert!(!name.contains(':'));
    }

    #[test]
    fn shell_quote_preserves_argument_boundaries() {
        assert_eq!(shell_quote("opencode"), "opencode");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("that's"), "'that'\"'\"'s'");
    }

    #[test]
    fn rejects_prompt_placeholder_for_interactive_tmux_command() {
        let config = Config {
            default_agent: "custom".to_string(),
            default_base: None,
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            escape_key: EscapeKey::EscEsc,
            checks: Checks::default(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::from([(
                "custom".to_string(),
                "custom-agent --prompt {prompt}".to_string(),
            )]),
            agent_prompt_modes: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/user.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-repo-config.toml"),
        };

        let error = super::agent_shell_command(&config).unwrap_err();

        assert!(error.contains("prompt placeholder"));
    }

    #[test]
    fn pane_command_only_counts_the_configured_agent_as_running() {
        let config = Config {
            default_agent: "opencode".to_string(),
            default_base: None,
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            escape_key: EscapeKey::EscEsc,
            checks: Checks::default(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/user.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-repo-config.toml"),
        };

        assert!(pane_command_matches_agent(&config, "opencode"));
        assert!(!pane_command_matches_agent(&config, "bash"));
        assert!(!pane_command_matches_agent(&config, "zsh"));
    }

    #[test]
    fn latest_agent_session_generation_reads_highest_existing_generation() {
        let temp = unique_temp_dir("prism-tmux-latest-generation-test");
        fs::create_dir_all(&temp).unwrap();
        let tmux = temp.join("tmux");
        let repo = Repository { root: temp.clone() };
        let expected_prefix = super::agent_session_prefix(&repo, "feature");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
case "$1" in
  list-sessions)
    echo '{}0'
    echo '{}7'
    echo '{}not-a-number'
    echo other-session
    exit 0
    ;;
esac
exit 1
"#,
                expected_prefix, expected_prefix, expected_prefix
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());

        let generation = latest_agent_session_generation(&repo, &config, "feature");

        assert_eq!(generation, Some(7));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn paste_agent_prompt_loads_and_pastes_tmux_buffer() {
        let temp = unique_temp_dir("prism-tmux-paste-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let prompt_file = temp.join("prompt.txt");
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  has-session|set-option|move-window|rename-window|new-window)
    exit 0
    ;;
  list-windows)
    exit 0
    ;;
  display-message)
    echo opencode
    exit 0
    ;;
  capture-pane)
    echo 'Ask anything'
    exit 0
    ;;
  load-buffer)
    cat > '{}'
    exit 0
    ;;
  paste-buffer)
    exit 0
    ;;
esac
exit 1
"#,
                log.display(),
                prompt_file.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository { root: temp.clone() };
        let session = test_session(temp.join("worktree"), "feature");

        paste_agent_prompt(&repo, &config, &session, 0, "hello\nworld").unwrap();

        assert_eq!(fs::read_to_string(&prompt_file).unwrap(), "hello\nworld");
        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.contains("load-buffer -b"));
        assert!(commands.contains("paste-buffer -d -b"));
        assert!(!commands.contains("attach-session"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn paste_agent_prompt_does_not_require_window_zero() {
        let temp = unique_temp_dir("prism-tmux-base-index-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let prompt_file = temp.join("prompt.txt");
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
for arg in "$@"; do
  case "$arg" in
    *:0.0*)
      echo "can't find window 0" >&2
      exit 1
      ;;
  esac
done
case "$1" in
  has-session|set-option|move-window|rename-window|new-window)
    exit 0
    ;;
  list-windows)
    exit 0
    ;;
  display-message)
    echo opencode
    exit 0
    ;;
  capture-pane)
    echo 'Ask anything'
    exit 0
    ;;
  load-buffer)
    cat > '{}'
    exit 0
    ;;
  paste-buffer)
    exit 0
    ;;
esac
exit 1
"#,
                log.display(),
                prompt_file.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository { root: temp.clone() };
        let session = test_session(temp.join("worktree"), "feature");

        paste_agent_prompt(&repo, &config, &session, 0, "hello").unwrap();

        assert_eq!(fs::read_to_string(&prompt_file).unwrap(), "hello");
        let commands = fs::read_to_string(&log).unwrap();
        assert!(!commands.contains(":0.0"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn paste_agent_prompt_waits_for_opencode_input() {
        let temp = unique_temp_dir("prism-tmux-input-ready-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let prompt_file = temp.join("prompt.txt");
        let capture_count = temp.join("capture-count");
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  has-session|set-option|move-window|rename-window|new-window)
    exit 0
    ;;
  list-windows)
    exit 0
    ;;
  display-message)
    echo opencode
    exit 0
    ;;
  capture-pane)
    count="$(cat '{}' 2>/dev/null || echo 0)"
    count="$((count + 1))"
    echo "$count" > '{}'
    if [ "$count" -lt 3 ]; then
      echo 'Starting OpenCode...'
    else
      echo 'Ask anything'
    fi
    exit 0
    ;;
  load-buffer)
    cat > '{}'
    exit 0
    ;;
  paste-buffer)
    exit 0
    ;;
esac
exit 1
"#,
                log.display(),
                capture_count.display(),
                capture_count.display(),
                prompt_file.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository { root: temp.clone() };
        let session = test_session(temp.join("worktree"), "feature");

        paste_agent_prompt(&repo, &config, &session, 0, "hello").unwrap();

        assert_eq!(fs::read_to_string(&prompt_file).unwrap(), "hello");
        assert_eq!(fs::read_to_string(&capture_count).unwrap().trim(), "3");
        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.find("capture-pane").unwrap() < commands.find("load-buffer").unwrap());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn ensure_agent_session_sets_detach_on_destroy() {
        let temp = unique_temp_dir("prism-tmux-detach-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  has-session)
    exit 1
    ;;
  new-session|set-option)
    exit 0
    ;;
  display-message)
    echo opencode
    exit 0
    ;;
esac
exit 0
"#,
                log.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();

        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository { root: temp.clone() };
        let session = test_session(temp.join("worktree"), "feature");

        let result = ensure_agent_session(&repo, &config, &session, 0);

        assert_eq!(result, Ok(false));
        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.contains("new-session -d -s"));
        assert!(commands.contains("-n opencode"));
        assert!(commands.contains("set-option -t"));
        assert!(commands.contains("detach-on-destroy on"));
        assert!(commands.contains("base-index 1"));
        assert!(commands.contains("new-window -d -t"));
        assert!(commands.contains("-n lazygit"));
        assert!(commands.contains("-n terminal"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn ensure_agent_session_replaces_existing_session_without_agent() {
        let temp = unique_temp_dir("prism-tmux-stale-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  has-session|set-option|kill-session|new-session)
    exit 0
    ;;
  display-message)
    echo bash
    exit 0
    ;;
esac
exit 0
"#,
                log.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();

        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository { root: temp.clone() };
        let session = test_session(temp.join("worktree"), "feature");

        let result = ensure_agent_session(&repo, &config, &session, 0);

        assert_eq!(result, Ok(false));
        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.contains("display-message -p -t"));
        assert!(commands.contains("kill-session -t"));
        assert!(commands.contains("new-session -d -s"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn ensure_agent_session_recovers_when_session_disappears_before_configure() {
        let temp = unique_temp_dir("prism-tmux-vanished-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let configure_count = temp.join("configure-count");
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  has-session|new-session)
    exit 0
    ;;
  set-option)
    count="$(cat '{}' 2>/dev/null || echo 0)"
    count="$((count + 1))"
    echo "$count" > '{}'
    if [ "$count" -eq 1 ]; then
      echo "can't find session: vanished" >&2
      exit 1
    fi
    exit 0
    ;;
  display-message)
    echo opencode
    exit 0
    ;;
esac
exit 0
"#,
                log.display(),
                configure_count.display(),
                configure_count.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();

        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository { root: temp.clone() };
        let session = test_session(temp.join("worktree"), "feature");

        let result = ensure_agent_session(&repo, &config, &session, 0);

        assert_eq!(result, Ok(true));
        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.contains("new-session -d -s"));
        assert!(commands.contains("set-option -t"));
        assert!(!commands.contains("kill-session -t"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn attach_return_after_destroy_does_not_recreate_immediately() {
        let temp = unique_temp_dir("prism-tmux-attach-destroy-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let state = temp.join("state");
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
state="$(cat '{}' 2>/dev/null || echo missing)"
case "$1" in
  has-session)
    [ "$state" = exists ]
    exit $?
    ;;
  new-session)
    echo exists > '{}'
    exit 0
    ;;
  set-option)
    [ "$state" = exists ] || {{
      echo "can't find session: vanished" >&2
      exit 1
    }}
    exit 0
    ;;
  display-message)
    echo opencode
    exit 0
    ;;
  attach-session)
    echo missing > '{}'
    exit 1
    ;;
esac
exit 0
"#,
                log.display(),
                state.display(),
                state.display(),
                state.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();

        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository { root: temp.clone() };
        let session = test_session(temp.join("worktree"), "feature");

        let result = attach_or_create_agent(&repo, &config, &session, 0);

        assert_eq!(result, Ok(()));
        let commands = fs::read_to_string(&log).unwrap();
        assert_eq!(commands.matches("new-session -d -s").count(), 1);
        assert_eq!(commands.matches("attach-session -t").count(), 1);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn attach_companion_windows_targets_named_indices() {
        let temp = unique_temp_dir("prism-tmux-companion-attach-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  has-session)
    exit 1
    ;;
  new-session|set-option|move-window|rename-window|new-window|attach-session)
    exit 0
    ;;
  list-windows)
    exit 0
    ;;
  display-message)
    echo opencode
    exit 0
    ;;
esac
exit 0
"#,
                log.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();

        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository { root: temp.clone() };
        let session = test_session(temp.join("worktree"), "feature");

        attach_or_create_window(&repo, &config, &session, 0, TmuxWindow::LazyGit).unwrap();

        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.contains("new-window -d -t"));
        assert!(commands.contains("-n lazygit"));
        assert!(commands.contains("-n terminal"));
        assert!(commands.contains("attach-session -t"));
        assert!(commands.contains(":2"));

        let _ = fs::remove_dir_all(temp);
    }

    fn test_session(path: PathBuf, branch: &str) -> Session {
        fs::create_dir_all(&path).unwrap();
        Session {
            path: path.clone(),
            path_display: path.display().to_string(),
            branch: branch.to_string(),
            prompt_summary: String::new(),
            adopted: false,
            hidden: false,
            status_label: "clean".to_string(),
            agent: None,
            agent_output: VecDeque::new(),
            agent_state: AgentState::Idle,
            pr: PrCache::default(),
        }
    }

    fn test_config() -> Config {
        Config {
            default_agent: "opencode".to_string(),
            default_base: None,
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            escape_key: EscapeKey::EscEsc,
            checks: Checks::default(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/user.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-repo-config.toml"),
        }
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
