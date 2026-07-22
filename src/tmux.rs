use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::opencode::{OpencodeRuntime, ensure_opencode_session, load_runtime, submit_prompt};
use crate::process::{
    run_capture, run_output, run_output_allow_failure, run_status_inherited, run_status_with_stdin,
    split_command_words,
};
use crate::repo::Repository;
use crate::session::Session;
use crate::util::{safe_branch_filename, stable_hash};

const EXISTING_SESSION_READY_WAIT: Duration = Duration::from_millis(250);
const CREATED_SESSION_READY_WAIT: Duration = Duration::from_secs(2);
const SESSION_READY_POLL_INTERVAL: Duration = Duration::from_millis(50);
const AGENT_INPUT_READY_WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TmuxAgentSession {
    name: String,
}

impl TmuxAgentSession {
    pub fn for_worktree_session(repo: &Repository, branch: &str, generation: u64) -> Self {
        Self {
            name: format!("{}{}", agent_session_prefix(repo, branch), generation),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn target(&self, window: TmuxWindow) -> String {
        window_target(&self.name, window)
    }

    fn prompt_buffer_name(&self) -> String {
        format!("{}-prompt", self.name)
    }
}

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
    let runtime = TmuxAgentSession::for_worktree_session(repo, &session.branch, generation);
    ensure_tmux_agent_session_for_attach(repo, config, session, &runtime)?;
    match attach_session(config, runtime.name()) {
        Ok(()) => Ok(()),
        Err(_) if matches!(session_exists(config, runtime.name()), Ok(false)) => Ok(()),
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
    let runtime = TmuxAgentSession::for_worktree_session(repo, &session.branch, generation);
    ensure_tmux_agent_session_for_attach(repo, config, session, &runtime)?;
    match attach(config, &runtime, window) {
        Ok(()) => Ok(()),
        Err(_) if matches!(session_exists(config, runtime.name()), Ok(false)) => Ok(()),
        Err(error) => Err(error),
    }
}

#[allow(dead_code)]
pub fn attach_or_create_plan_mode(
    config: &Config,
    name: &str,
    cwd: &Path,
    command: &str,
) -> Result<(), String> {
    if !session_exists(config, name)? {
        create_detached_plan_mode_session(config, name, cwd, command)?;
        configure_detach_on_destroy(config, name)?;
    }
    match attach_session(config, name) {
        Ok(()) => Ok(()),
        Err(_) if matches!(session_exists(config, name), Ok(false)) => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn ensure_agent_session(
    repo: &Repository,
    config: &Config,
    session: &Session,
    generation: u64,
) -> Result<bool, String> {
    let runtime = TmuxAgentSession::for_worktree_session(repo, &session.branch, generation);
    ensure_tmux_agent_session(repo, config, session, &runtime)
}

fn ensure_tmux_agent_session_for_attach(
    repo: &Repository,
    config: &Config,
    session: &Session,
    runtime: &TmuxAgentSession,
) -> Result<(), String> {
    if tmux_agent_session_running(config, runtime)
        && configure_agent_session(config, runtime.name())?
    {
        ensure_companion_windows(config, session, runtime)?;
        return Ok(());
    }
    ensure_tmux_agent_session(repo, config, session, runtime).map(|_| ())
}

fn ensure_tmux_agent_session(
    repo: &Repository,
    config: &Config,
    session: &Session,
    runtime_session: &TmuxAgentSession,
) -> Result<bool, String> {
    let runtime = opencode_runtime_for_session(repo, config, session)?;
    if session_exists(config, runtime_session.name())? {
        if !configure_agent_session(config, runtime_session.name())? {
            create_detached_agent_session(
                repo,
                config,
                session,
                runtime_session,
                runtime.as_ref(),
            )?;
            configure_agent_session(config, runtime_session.name())?;
            ensure_companion_windows(config, session, runtime_session)?;
            return Ok(wait_for_agent_session_running(
                config,
                runtime_session,
                CREATED_SESSION_READY_WAIT,
            ));
        }
        if wait_for_agent_session_running(config, runtime_session, EXISTING_SESSION_READY_WAIT) {
            ensure_companion_windows(config, session, runtime_session)?;
            return Ok(true);
        }
        kill_session(config, runtime_session.name())?;
    }
    create_detached_agent_session(repo, config, session, runtime_session, runtime.as_ref())?;
    configure_agent_session(config, runtime_session.name())?;
    ensure_companion_windows(config, session, runtime_session)?;
    Ok(wait_for_agent_session_running(
        config,
        runtime_session,
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
    let runtime_session = TmuxAgentSession::for_worktree_session(repo, &session.branch, generation);
    if config.default_agent == "opencode" && !config.is_default_branch(&session.branch) {
        let runtime = ensure_opencode_session(repo, config, &session.branch, &session.path)
            .map_err(|error| format!("prepare opencode runtime for prompt: {error}"))?;
        let session_id = runtime
            .opencode_session_id
            .as_deref()
            .ok_or_else(|| "OpenCode session ID is not available".to_string())?;
        let agent_ready = ensure_agent_session(repo, config, session, generation)?;
        match submit_prompt(&runtime.server_url, session_id, prompt) {
            Ok(()) => return Ok(()),
            Err(api_error) => {
                if !agent_ready {
                    return Err(format!(
                        "submit opencode prompt through API failed: {api_error}; agent session did not become ready"
                    ));
                }
                paste_prompt_into_tmux(config, &runtime_session, prompt).map_err(|paste_error| {
                    format!(
                        "submit opencode prompt through API failed: {api_error}; paste fallback failed: {paste_error}"
                    )
                })?;
                return Ok(());
            }
        }
    }
    if !ensure_agent_session(repo, config, session, generation)? {
        return Err("agent session did not become ready".to_string());
    }
    paste_prompt_into_tmux(config, &runtime_session, prompt)
}

fn paste_prompt_into_tmux(
    config: &Config,
    runtime_session: &TmuxAgentSession,
    prompt: &str,
) -> Result<(), String> {
    if !wait_for_agent_input_ready(
        config,
        &runtime_session.target(TmuxWindow::Agent),
        AGENT_INPUT_READY_WAIT,
    ) {
        return Err("agent prompt did not become ready".to_string());
    }
    let buffer_name = runtime_session.prompt_buffer_name();
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
        &runtime_session.target(TmuxWindow::Agent),
    ]))
}

pub fn agent_session_running(
    repo: &Repository,
    config: &Config,
    session: &Session,
    generation: u64,
) -> bool {
    let runtime = TmuxAgentSession::for_worktree_session(repo, &session.branch, generation);
    tmux_agent_session_running(config, &runtime)
}

fn tmux_agent_session_running(config: &Config, runtime: &TmuxAgentSession) -> bool {
    if !matches!(session_exists(config, runtime.name()), Ok(true)) {
        return false;
    }
    let target = runtime.target(TmuxWindow::Agent);
    let Some(current_command) = pane_current_command(config, &target) else {
        return false;
    };
    pane_command_matches_agent(config, &current_command)
        || pane_start_command(config, &target)
            .is_some_and(|command| pane_start_command_matches_agent(config, &command))
}

pub fn kill_agent_session(
    repo: &Repository,
    config: &Config,
    branch: &str,
    generation: u64,
) -> Result<(), String> {
    let runtime = TmuxAgentSession::for_worktree_session(repo, branch, generation);
    kill_session(config, runtime.name())
}

pub fn kill_agent_sessions_for_branch(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<(), String> {
    let prefix = agent_session_prefix(repo, branch);
    for name in agent_session_names_with_prefix(config, &prefix)? {
        kill_session(config, &name)?;
    }
    Ok(())
}

pub fn latest_agent_session_generation(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Option<u64> {
    let prefix = agent_session_prefix(repo, branch);
    agent_session_names_with_prefix(config, &prefix)
        .ok()?
        .into_iter()
        .filter_map(|name| name.strip_prefix(&prefix)?.parse::<u64>().ok())
        .max()
}

fn agent_session_names_with_prefix(config: &Config, prefix: &str) -> Result<Vec<String>, String> {
    let output =
        run_output_allow_failure(Command::new(config.tool("tmux")).env_remove("TMUX").args([
            "list-sessions",
            "-F",
            "#{session_name}",
        ]))?;
    if !output.status.success() {
        let stderr = output.stderr.trim();
        if tmux_missing_session_error(stderr) {
            return Ok(Vec::new());
        }
        return Err(if stderr.is_empty() {
            format!("tmux exited with {}", output.status)
        } else {
            stderr.to_string()
        });
    }
    Ok(output
        .stdout
        .lines()
        .filter(|name| name.starts_with(prefix))
        .map(str::to_string)
        .collect())
}

fn agent_session_prefix(repo: &Repository, branch: &str) -> String {
    let hash = stable_hash(repo.root.as_path());
    let branch = safe_tmux_name(&safe_branch_filename(branch));
    format!("prism-{hash:016x}-{branch}-")
}

fn attach(config: &Config, runtime: &TmuxAgentSession, window: TmuxWindow) -> Result<(), String> {
    run_status_inherited(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "attach-session",
        "-t",
        &runtime.target(window),
    ]))
}

fn attach_session(config: &Config, name: &str) -> Result<(), String> {
    run_status_inherited(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "attach-session",
        "-t",
        name,
    ]))
}

fn create_detached_agent_session(
    repo: &Repository,
    config: &Config,
    session: &Session,
    runtime_session: &TmuxAgentSession,
    runtime: Option<&OpencodeRuntime>,
) -> Result<(), String> {
    let command = agent_shell_command(repo, config, session, runtime)?;
    run_tmux_status(
        Command::new(config.tool("tmux"))
            .env_remove("TMUX")
            .args(["new-session", "-d", "-s"])
            .arg(runtime_session.name())
            .args(["-n", &TmuxWindow::Agent.name(config)])
            .arg("-c")
            .arg(&session.path)
            .arg(command),
    )
}

fn configure_agent_session(config: &Config, name: &str) -> Result<bool, String> {
    match configure_detach_on_destroy(config, name) {
        Ok(()) => Ok(true),
        Err(error) if tmux_missing_session_error(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn configure_detach_on_destroy(config: &Config, name: &str) -> Result<(), String> {
    run_tmux_status(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "set-option",
        "-t",
        name,
        "detach-on-destroy",
        "on",
    ]))
}

#[allow(dead_code)]
fn create_detached_plan_mode_session(
    config: &Config,
    name: &str,
    cwd: &Path,
    command: &str,
) -> Result<(), String> {
    run_tmux_status(
        Command::new(config.tool("tmux"))
            .env_remove("TMUX")
            .args(["new-session", "-d", "-s"])
            .arg(name)
            .args(["-n", "plan"])
            .arg("-c")
            .arg(cwd)
            .arg(command),
    )
}

fn ensure_companion_windows(
    config: &Config,
    session: &Session,
    runtime: &TmuxAgentSession,
) -> Result<(), String> {
    configure_window_indexing(config, runtime.name())?;
    move_initial_window_to_one(config, runtime)?;
    rename_window(config, runtime, TmuxWindow::Agent)?;
    ensure_window(config, session, runtime, TmuxWindow::LazyGit)?;
    ensure_window(config, session, runtime, TmuxWindow::Terminal)?;
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

fn move_initial_window_to_one(config: &Config, runtime: &TmuxAgentSession) -> Result<(), String> {
    match run_tmux_status(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "move-window",
        "-s",
        &format!("{}:0", runtime.name()),
        "-t",
        &runtime.target(TmuxWindow::Agent),
    ])) {
        Ok(()) => Ok(()),
        Err(error) if tmux_missing_session_error(&error) || error.contains("same index") => Ok(()),
        Err(error) => Err(error),
    }
}

fn rename_window(
    config: &Config,
    runtime: &TmuxAgentSession,
    window: TmuxWindow,
) -> Result<(), String> {
    match run_tmux_status(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "rename-window",
        "-t",
        &runtime.target(window),
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
    runtime: &TmuxAgentSession,
    window: TmuxWindow,
) -> Result<(), String> {
    if window_exists(config, runtime.name(), window)? {
        rename_window(config, runtime, window)?;
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
            .args(["new-window", "-d", "-t", &runtime.target(window)])
            .args(["-n", &window.name(config)])
            .arg("-c")
            .arg(&session.path)
            .arg(command),
    )
}

fn window_exists(config: &Config, name: &str, window: TmuxWindow) -> Result<bool, String> {
    run_output_allow_failure(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "list-windows",
        "-t",
        name,
        "-F",
        "#{window_index}",
    ]))
    .map(|output| {
        output.status.success()
            && output
                .stdout
                .lines()
                .any(|line| line == window.index().to_string())
    })
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
    run_output_allow_failure(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "has-session",
        "-t",
        name,
    ]))
    .map(|output| output.status.success())
}

fn wait_for_agent_session_running(
    config: &Config,
    runtime: &TmuxAgentSession,
    timeout: Duration,
) -> bool {
    let started = Instant::now();
    loop {
        if tmux_agent_session_running(config, runtime) {
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
    let output = run_output(command)?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = output.stderr.trim().to_string();
    if stderr.is_empty() {
        Err(format!("tmux exited with {}", output.status))
    } else {
        Err(stderr)
    }
}

fn run_tmux_status_with_stdin(command: &mut Command, stdin: &str) -> Result<(), String> {
    run_status_with_stdin(command, stdin)
}

fn tmux_missing_session_error(error: &str) -> bool {
    error.contains("can't find session")
        || error.contains("can't find window")
        || error.contains("can't find pane")
        || error.contains("no server running")
}

fn agent_shell_command(
    repo: &Repository,
    config: &Config,
    session: &Session,
    runtime: Option<&OpencodeRuntime>,
) -> Result<String, String> {
    let argv = interactive_agent_argv(repo, config, session, runtime);
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

fn interactive_agent_argv(
    repo: &Repository,
    config: &Config,
    session: &Session,
    runtime: Option<&OpencodeRuntime>,
) -> Vec<String> {
    if config.default_agent == "opencode" {
        if let Some(runtime) = runtime
            .cloned()
            .or_else(|| usable_opencode_runtime(repo, session))
            && let Some(session_id) = runtime.opencode_session_id
        {
            return vec![
                config.tool("opencode"),
                "attach".to_string(),
                runtime.server_url,
                "--dir".to_string(),
                session.path.display().to_string(),
                "--session".to_string(),
                session_id,
            ];
        }
        vec![config.tool("opencode")]
    } else {
        split_command_words(&config.agent_command(&config.default_agent))
    }
}

fn opencode_runtime_for_session(
    repo: &Repository,
    config: &Config,
    session: &Session,
) -> Result<Option<OpencodeRuntime>, String> {
    if config.default_agent != "opencode" || config.is_default_branch(&session.branch) {
        return Ok(None);
    }
    ensure_opencode_session(repo, config, &session.branch, &session.path)
        .map(Some)
        .map_err(|error| format!("prepare opencode runtime: {error}"))
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

fn pane_start_command(config: &Config, name: &str) -> Option<String> {
    run_capture(
        Command::new(config.tool("tmux"))
            .env_remove("TMUX")
            .args(["display-message", "-p", "-t"])
            .arg(name)
            .arg("#{pane_start_command}"),
    )
    .ok()
    .map(|output| output.trim().to_string())
    .filter(|output| !output.is_empty())
}

fn pane_command_matches_agent(config: &Config, pane_command: &str) -> bool {
    let expected = if config.default_agent == "opencode" {
        config.tool("opencode")
    } else {
        let Some(expected) = split_command_words(&config.agent_command(&config.default_agent))
            .first()
            .cloned()
        else {
            return false;
        };
        expected
    };
    let expected = Path::new(&expected)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&expected);
    pane_command == expected
        || (config.default_agent == "opencode" && pane_command == format!("{expected}.exe"))
}

fn pane_start_command_matches_agent(config: &Config, pane_start_command: &str) -> bool {
    let command = pane_start_command
        .strip_prefix('"')
        .and_then(|command| command.strip_suffix('"'))
        .unwrap_or(pane_start_command);
    let Some(executable) = split_command_words(command).into_iter().next() else {
        return false;
    };
    let executable = Path::new(&executable)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&executable);
    pane_command_matches_agent(config, executable)
}

fn usable_opencode_runtime(repo: &Repository, session: &Session) -> Option<OpencodeRuntime> {
    load_runtime(repo, &session.branch, &session.path)
        .ok()
        .flatten()
        .filter(|runtime| {
            !runtime.server_url.is_empty()
                && runtime.worktree_path == session.path.display().to_string()
        })
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
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::io::{Error, ErrorKind};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::agent::AgentState;
    use crate::config::Config;
    use crate::github::PrCache;
    use crate::opencode::{OpencodeRuntime, save_runtime, server_url};
    use crate::repo::Repository;
    use crate::session::Session;

    use super::{
        TmuxAgentSession, TmuxWindow, attach_or_create_agent, attach_or_create_plan_mode,
        attach_or_create_window, ensure_agent_session, latest_agent_session_generation,
        pane_command_matches_agent, pane_start_command_matches_agent, paste_agent_prompt,
        shell_quote,
    };

    #[test]
    fn tmux_session_names_are_stable_and_safe() {
        let repo = Repository {
            root: PathBuf::from("/repo/my project"),
        };

        let runtime = TmuxAgentSession::for_worktree_session(&repo, "feature/foo:bar", 3);
        let name = runtime.name();

        assert!(name.starts_with("prism-"));
        assert!(name.ends_with("-feature_foo_bar-3"));
        assert!(!name.contains('/'));
        assert!(!name.contains(':'));
    }

    #[test]
    fn tmux_agent_session_exposes_runtime_targets() {
        let repo = Repository {
            root: PathBuf::from("/repo/my project"),
        };

        let runtime = TmuxAgentSession::for_worktree_session(&repo, "feature/foo:bar", 3);

        assert_eq!(
            runtime.name(),
            TmuxAgentSession::for_worktree_session(&repo, "feature/foo:bar", 3).name()
        );
        assert_eq!(
            runtime.target(TmuxWindow::Agent),
            format!("{}:1", runtime.name())
        );
        assert_eq!(
            runtime.target(TmuxWindow::LazyGit),
            format!("{}:2", runtime.name())
        );
        assert_eq!(
            runtime.target(TmuxWindow::Terminal),
            format!("{}:3", runtime.name())
        );
        assert_eq!(
            runtime.prompt_buffer_name(),
            format!("{}-prompt", runtime.name())
        );
    }

    #[test]
    fn shell_quote_preserves_argument_boundaries() {
        assert_eq!(shell_quote("opencode"), "opencode");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("that's"), "'that'\"'\"'s'");
    }

    #[test]
    fn plan_mode_runs_in_detachable_tmux_session() {
        let temp = unique_temp_dir("prism-tmux-plan-mode-test");
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
  new-session|set-option|attach-session)
    exit 0
    ;;
esac
exit 1
"#,
                log.display()
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

        attach_or_create_plan_mode(
            &config,
            "prism-plan-test",
            &temp,
            "prism --repo /repo plan; status=$?",
        )
        .unwrap();

        let commands = fs::read_to_string(&log).unwrap_or_default();
        assert!(commands.contains("new-session -d -s prism-plan-test"));
        assert!(commands.contains("-n plan"));
        assert!(commands.contains("prism --repo /repo plan; status=$?"));
        assert!(commands.contains("set-option -t prism-plan-test"));
        assert!(commands.contains("detach-on-destroy on"));
        assert!(commands.contains("attach-session -t prism-plan-test"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn rejects_prompt_placeholder_for_interactive_tmux_command() {
        let mut config = crate::test_support::test_config();
        config.default_agent = "custom".to_string();
        config.agent_commands.insert(
            "custom".to_string(),
            "custom-agent --prompt {prompt}".to_string(),
        );

        let repo = Repository {
            root: PathBuf::from("/repo"),
        };
        let session = test_session(unique_temp_dir("prism-tmux-placeholder-test"), "feature");

        let error = super::agent_shell_command(&repo, &config, &session, None).unwrap_err();

        assert!(error.contains("prompt placeholder"));
    }

    #[test]
    fn opencode_runtime_uses_attach_command_for_agent_window() {
        let temp = unique_temp_dir("prism-tmux-opencode-attach-test");
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
            .insert("opencode".to_string(), "/usr/bin/opencode".to_string());
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(temp.join("worktree"), "feature");
        save_runtime(
            &repo,
            &OpencodeRuntime {
                repo_root: temp.display().to_string(),
                branch: "feature".to_string(),
                worktree_path: session.path.display().to_string(),
                server_port: 41_234,
                server_url: server_url(41_234),
                server_pid: Some(123),
                opencode_session_id: Some("ses_123".to_string()),
                generation: 1,
                updated_unix_ms: 42,
            },
        )
        .unwrap();

        let result = ensure_agent_session(&repo, &config, &session, 0);

        assert_eq!(result, Ok(false));
        let commands = fs::read_to_string(&log).unwrap_or_default();
        assert!(commands.contains("/usr/bin/opencode attach http://127.0.0.1:41234"));
        assert!(commands.contains("--dir"));
        assert!(commands.contains(&session.path.display().to_string()));
        assert!(commands.contains("--session ses_123"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn ensure_agent_session_resolves_opencode_session_before_tmux_attach() {
        let temp = unique_temp_dir("prism-tmux-opencode-resolve-test");
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
        config.default_base = Some("main".to_string());
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "/usr/bin/opencode".to_string());
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(temp.join("worktree"), "feature");
        let port = match start_fake_opencode_server(session.path.clone(), 200, None, 4) {
            Ok(port) => port,
            Err(error) if error.kind() == ErrorKind::PermissionDenied => return,
            Err(error) => panic!("start fake OpenCode server: {error}"),
        };
        save_runtime(
            &repo,
            &OpencodeRuntime {
                repo_root: temp.display().to_string(),
                branch: "feature".to_string(),
                worktree_path: session.path.display().to_string(),
                server_port: port,
                server_url: server_url(port),
                server_pid: Some(123),
                opencode_session_id: None,
                generation: 1,
                updated_unix_ms: 42,
            },
        )
        .unwrap();

        let result = ensure_agent_session(&repo, &config, &session, 0);

        assert_eq!(result, Ok(false));
        let runtime = crate::opencode::load_runtime(&repo, "feature", &session.path)
            .unwrap()
            .unwrap();
        assert_eq!(runtime.opencode_session_id.as_deref(), Some("ses_123"));
        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.contains(&format!("/usr/bin/opencode attach http://127.0.0.1:{port}")));
        assert!(commands.contains("--session ses_123"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn pane_command_only_counts_the_configured_agent_as_running() {
        let mut config = crate::test_support::test_config();
        config.default_agent = "opencode".to_string();
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());

        assert!(pane_command_matches_agent(&config, "opencode"));
        assert!(pane_command_matches_agent(&config, "opencode.exe"));
        assert!(!pane_command_matches_agent(&config, "bash"));
        assert!(!pane_command_matches_agent(&config, "zsh"));
        assert!(pane_start_command_matches_agent(
            &config,
            r#""/usr/local/bin/opencode attach http://127.0.0.1:41000""#
        ));
        assert!(!pane_start_command_matches_agent(&config, r#""/bin/bash""#));
    }

    #[test]
    fn latest_agent_session_generation_reads_highest_existing_generation() {
        let temp = unique_temp_dir("prism-tmux-latest-generation-test");
        fs::create_dir_all(&temp).unwrap();
        let tmux = temp.join("tmux");
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
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
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(temp.join("worktree"), "feature");

        let prompt =
            "  fix review comments\nquote: \"that's fine\"\n$PATH && rm -rf nope\n--leading-dash";

        paste_agent_prompt(&repo, &config, &session, 0, prompt).unwrap();

        assert_eq!(fs::read_to_string(&prompt_file).unwrap(), prompt);
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
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
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
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(temp.join("worktree"), "feature");

        paste_agent_prompt(&repo, &config, &session, 0, "hello").unwrap();

        assert_eq!(fs::read_to_string(&prompt_file).unwrap(), "hello");
        assert_eq!(fs::read_to_string(&capture_count).unwrap().trim(), "3");
        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.find("capture-pane").unwrap() < commands.find("load-buffer").unwrap());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn paste_agent_prompt_persists_prompt_in_target_opencode_session() {
        let temp = unique_temp_dir("prism-tmux-api-paste-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let prompt_file = temp.join("prompt.txt");
        let api_log = temp.join("api.log");
        let session_marker = temp.join("tmux-session");
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  has-session)
    test -f '{}'
    exit $?
    ;;
  new-session)
    touch '{}'
    exit 0
    ;;
  set-option|move-window|rename-window|new-window)
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
    echo 'Starting OpenCode...'
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
                session_marker.display(),
                session_marker.display(),
                prompt_file.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        let mut config = test_config();
        config.default_base = Some("main".to_string());
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(temp.join("worktree"), "feature");
        let port =
            match start_fake_opencode_server(session.path.clone(), 204, Some(api_log.clone()), 8) {
                Ok(port) => port,
                Err(error) if error.kind() == ErrorKind::PermissionDenied => return,
                Err(error) => panic!("start fake OpenCode server: {error}"),
            };
        save_runtime(
            &repo,
            &OpencodeRuntime {
                repo_root: temp.display().to_string(),
                branch: "feature".to_string(),
                worktree_path: session.path.display().to_string(),
                server_port: port,
                server_url: server_url(port),
                server_pid: Some(123),
                opencode_session_id: Some("ses_123".to_string()),
                generation: 1,
                updated_unix_ms: 42,
            },
        )
        .unwrap();

        let prompt =
            "  fix review comments\nquote: \"that's fine\"\n$PATH && rm -rf nope\n--leading-dash";

        paste_agent_prompt(&repo, &config, &session, 0, prompt).unwrap();

        assert!(!prompt_file.exists());
        let api_requests = fs::read_to_string(&api_log).unwrap();
        assert!(api_requests.contains("POST /session/ses_123/prompt_async"));
        assert!(api_requests.contains(
            r#"{"parts":[{"type":"text","text":"  fix review comments\nquote: \"that's fine\"\n$PATH && rm -rf nope\n--leading-dash"}]}"#
        ));
        assert!(!api_requests.contains("POST /tui/"));
        let commands = fs::read_to_string(&log).unwrap_or_default();
        assert!(commands.contains("new-session -d -s"));
        assert!(commands.contains("opencode attach"));
        assert!(!commands.contains("load-buffer"));
        assert!(!commands.contains("paste-buffer"));

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
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
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
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
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
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
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
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(temp.join("worktree"), "feature");

        let result = attach_or_create_agent(&repo, &config, &session, 0);

        assert_eq!(result, Ok(()));
        let commands = fs::read_to_string(&log).unwrap();
        assert_eq!(commands.matches("new-session -d -s").count(), 1);
        assert_eq!(commands.matches("attach-session -t").count(), 1);
        let attach = commands
            .lines()
            .find(|line| line.starts_with("attach-session -t "))
            .unwrap();
        assert!(!attach.contains(":1"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn attach_existing_agent_does_not_require_opencode_server() {
        let temp = unique_temp_dir("prism-tmux-existing-agent-attach-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  has-session|set-option|move-window|attach-session)
    exit 0
    ;;
  display-message)
    echo opencode
    exit 0
    ;;
  list-windows)
    printf '1\n2\n3\n'
    exit 0
    ;;
  rename-window)
    exit 0
    ;;
esac
exit 1
"#,
                log.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();

        let mut config = test_config();
        config.default_base = Some("main".to_string());
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config.tools.insert(
            "opencode".to_string(),
            temp.join("opencode").display().to_string(),
        );
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(temp.join("worktree"), "feature");

        attach_or_create_agent(&repo, &config, &session, 0).unwrap();

        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.contains("attach-session -t"));
        assert!(!commands.contains("new-session -d -s"));

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
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(temp.join("worktree"), "feature");

        attach_or_create_window(&repo, &config, &session, 0, TmuxWindow::LazyGit).unwrap();

        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.contains("new-window -d -t"));
        assert!(commands.contains("-n lazygit"));
        assert!(commands.contains("-n terminal"));
        let attach = commands
            .lines()
            .find(|line| line.starts_with("attach-session -t "))
            .unwrap();
        assert!(attach.contains(":2"));

        let _ = fs::remove_dir_all(temp);
    }

    fn test_session(path: PathBuf, branch: &str) -> Session {
        fs::create_dir_all(&path).unwrap();
        Session {
            repo_index: 0,
            repo_label: "repo".to_string(),
            repo_key: None,
            path: path.clone(),
            incarnation: String::new(),
            path_display: path.display().to_string(),
            branch: branch.to_string(),
            prompt_summary: String::new(),
            classification: crate::session::SessionClassification::Work,
            visibility: 0,
            adopted: false,
            hidden: false,
            status_label: "clean".to_string(),
            agent_state: AgentState::Idle,
            opencode_status: None,
            pr: PrCache::default(),
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        }
    }

    fn test_config() -> Config {
        let mut config = crate::test_support::test_config();
        config.default_agent = "opencode".to_string();
        config.default_base = Some("feature".to_string());
        config
    }

    fn start_fake_opencode_server(
        worktree: PathBuf,
        prompt_status: u16,
        request_log: Option<PathBuf>,
        request_limit: usize,
    ) -> Result<u16, Error> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        thread::spawn(move || {
            for stream in listener.incoming().take(request_limit).flatten() {
                handle_fake_opencode_request(
                    stream,
                    &worktree,
                    prompt_status,
                    request_log.as_ref(),
                );
            }
        });
        Ok(port)
    }

    fn handle_fake_opencode_request(
        mut stream: TcpStream,
        worktree: &Path,
        prompt_status: u16,
        request_log: Option<&PathBuf>,
    ) {
        let mut reader = BufReader::new(&mut stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() || request_line.trim().is_empty() {
            return;
        }
        let mut content_length = 0_usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line == "\r\n" || line == "\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or_default();
            }
        }
        let mut request_body = Vec::new();
        if content_length > 0 {
            let mut body = vec![0; content_length];
            if reader.read_exact(&mut body).is_err() {
                return;
            }
            request_body = body;
        }
        drop(reader);

        if let Some(path) = request_log {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            let _ = writeln!(file, "{}", request_line.trim_end());
            if !request_body.is_empty() {
                let _ = writeln!(file, "{}", String::from_utf8_lossy(&request_body));
            }
        }

        let session = format!(
            r#"{{"id":"ses_123","directory":"{}","title":"feature"}}"#,
            worktree.display()
        );
        let (status, body) = if request_line.starts_with("GET /global/health ") {
            (200, "{}".to_string())
        } else if request_line.starts_with("GET /session/ses_123 ") {
            (200, session)
        } else if request_line.starts_with("GET /session ")
            || request_line.starts_with("GET /session?")
        {
            (200, format!(r#"{{"data":[{session}]}}"#))
        } else if request_line.starts_with("POST /session/ses_123/prompt_async ") {
            (prompt_status, String::new())
        } else {
            (404, "{}".to_string())
        };
        let reason = if status == 200 { "OK" } else { "ERROR" };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
