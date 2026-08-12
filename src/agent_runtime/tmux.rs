#![allow(
    dead_code,
    reason = "harness adapters expose optional resume capabilities"
)]

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::opencode::{OpencodeRuntime, load_runtime};
use crate::process::{
    ProcessDescriptor, ProcessPolicy, run_capture_named, run_output_allow_failure_named,
    run_output_named, run_status_inherited_named, run_status_with_stdin_named, split_command_words,
};
use crate::repo::Repository;
use crate::session::Session;
use crate::util::{safe_branch_filename, stable_hash};

const EXISTING_SESSION_READY_WAIT: Duration = Duration::from_millis(250);
const CREATED_SESSION_READY_WAIT: Duration = Duration::from_secs(2);
const SESSION_READY_POLL_INTERVAL: Duration = Duration::from_millis(50);
const AGENT_INPUT_READY_WAIT: Duration = Duration::from_secs(5);
const OPENCODE_RUNTIME_OPTION: &str = "@prism-opencode-runtime";

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
    pub(crate) const fn label(self) -> &'static str {
        match self {
            TmuxWindow::Agent => "agent",
            TmuxWindow::LazyGit => "lazygit",
            TmuxWindow::Terminal => "terminal",
        }
    }

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
    match attach_agent_session(config, &runtime) {
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
    let opencode_runtime = usable_opencode_runtime(repo, config, session);
    if tmux_agent_session_running(config, runtime)
        && agent_session_runtime_matches(config, runtime.name(), session, opencode_runtime.as_ref())
        && configure_agent_session(config, runtime.name(), opencode_runtime.as_ref())?
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
    if session_exists(config, runtime_session.name())? {
        let stored_runtime = usable_opencode_runtime(repo, config, session);
        if !agent_session_runtime_matches(
            config,
            runtime_session.name(),
            session,
            stored_runtime.as_ref(),
        ) {
            kill_session(config, runtime_session.name())?;
        } else if !configure_agent_session(config, runtime_session.name(), stored_runtime.as_ref())?
        {
            let runtime = opencode_runtime_for_session(repo, config, session)?;
            create_detached_agent_session(
                repo,
                config,
                session,
                runtime_session,
                runtime.as_ref(),
                InteractiveAgentLaunch::default(),
            )?;
            configure_agent_session(config, runtime_session.name(), runtime.as_ref())?;
            ensure_companion_windows(config, session, runtime_session)?;
            return Ok(wait_for_agent_session_running(
                config,
                runtime_session,
                CREATED_SESSION_READY_WAIT,
            ));
        } else if wait_for_agent_session_running(
            config,
            runtime_session,
            EXISTING_SESSION_READY_WAIT,
        ) {
            ensure_companion_windows(config, session, runtime_session)?;
            return Ok(true);
        } else {
            kill_session(config, runtime_session.name())?;
        }
    }
    let runtime = opencode_runtime_for_session(repo, config, session)?;
    create_detached_agent_session(
        repo,
        config,
        session,
        runtime_session,
        runtime.as_ref(),
        InteractiveAgentLaunch::default(),
    )?;
    configure_agent_session(config, runtime_session.name(), runtime.as_ref())?;
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
    paste_agent_prompt_with_selection(
        repo,
        config,
        session,
        generation,
        prompt,
        crate::harness::AgentSelection::default(),
    )
}

pub fn paste_agent_prompt_with_selection(
    repo: &Repository,
    config: &Config,
    session: &Session,
    generation: u64,
    prompt: &str,
    selection: crate::harness::AgentSelection<'_>,
) -> Result<(), String> {
    let runtime_session = TmuxAgentSession::for_worktree_session(repo, &session.branch, generation);
    if selected_adapter_is(config, "opencode") && !config.is_default_branch(&session.branch) {
        let harness_config = config.harness_config(&config.default_harness)?;
        let runtime = crate::harness::Harness::new(&config.default_harness, &harness_config)
            .prepare_session(repo, config, &session.branch, &session.path)?
            .ok_or_else(|| "selected harness has no native session protocol".to_string())?;
        let session_id = runtime
            .opencode_session_id
            .as_deref()
            .ok_or_else(|| "OpenCode session ID is not available".to_string())?;
        ensure_agent_session(repo, config, session, generation)?;
        return crate::opencode::submit_prompt_for_worktree_with_selection(
            &runtime.server_url,
            session_id,
            prompt,
            &session.path,
            selection,
        )
        .map_err(|error| format!("submit prompt through harness protocol: {error}"));
    }
    if uses_legacy_agent_override(config)
        || (selected_adapter_is(config, "opencode") && config.is_default_branch(&session.branch))
    {
        if selection != crate::harness::AgentSelection::default() {
            return Err(format!(
                "harness '{}' cannot apply model or variant overrides when pasting into an existing interactive session",
                config.default_harness
            ));
        }
        if !ensure_agent_session(repo, config, session, generation)? {
            return Err("agent session did not become ready".to_string());
        }
        return paste_prompt_into_tmux(config, &runtime_session, prompt);
    }
    if tmux_agent_session_running(config, &runtime_session) {
        return Err(format!(
            "harness '{}' does not support submitting a prompt to an existing interactive session",
            config.default_harness
        ));
    }
    let runtime = opencode_runtime_for_session(repo, config, session)?;
    if session_exists(config, runtime_session.name())? {
        kill_session(config, runtime_session.name())?;
    }
    create_detached_agent_session(
        repo,
        config,
        session,
        &runtime_session,
        runtime.as_ref(),
        InteractiveAgentLaunch {
            prompt: Some(prompt),
            resume_session_id: None,
            selection,
        },
    )?;
    configure_agent_session(config, runtime_session.name(), runtime.as_ref())?;
    ensure_companion_windows(config, session, &runtime_session)?;
    if wait_for_agent_session_running(config, &runtime_session, CREATED_SESSION_READY_WAIT) {
        Ok(())
    } else {
        Err("agent session did not become ready".to_string())
    }
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
    agent_session_running_result(repo, config, session, generation).unwrap_or(false)
}

pub fn agent_session_running_result(
    repo: &Repository,
    config: &Config,
    session: &Session,
    generation: u64,
) -> Result<bool, String> {
    let runtime = TmuxAgentSession::for_worktree_session(repo, &session.branch, generation);
    tmux_agent_session_running_result(config, &runtime)
}

fn tmux_agent_session_running(config: &Config, runtime: &TmuxAgentSession) -> bool {
    tmux_agent_session_running_result(config, runtime).unwrap_or(false)
}

fn tmux_agent_session_running_result(
    config: &Config,
    runtime: &TmuxAgentSession,
) -> Result<bool, String> {
    if !session_exists(config, runtime.name())? {
        return Ok(false);
    }
    let target = runtime.target(TmuxWindow::Agent);
    let current_command = pane_current_command_result(config, &target)?;
    let start_command = pane_start_command_result(config, &target)?;
    Ok(current_command
        .as_deref()
        .is_some_and(|command| pane_command_matches_agent(config, command))
        || start_command
            .as_deref()
            .is_some_and(|command| pane_start_command_matches_agent(config, command)))
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
    latest_agent_session_generation_result(repo, config, branch)
        .ok()
        .flatten()
}

pub fn latest_agent_session_generation_result(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<Option<u64>, String> {
    let started = std::time::Instant::now();
    let prefix = agent_session_prefix(repo, branch);
    let sessions = agent_session_names_with_prefix(config, &prefix);
    let generation = sessions.as_ref().ok().and_then(|sessions| {
        sessions
            .iter()
            .map(String::as_str)
            .filter_map(|name| name.strip_prefix(&prefix)?.parse::<u64>().ok())
            .max()
    });
    crate::flight_recorder::record(
        "tmux",
        "generation_discovery",
        Some(started.elapsed()),
        vec![
            crate::flight_recorder::text("target", branch),
            crate::flight_recorder::boolean("success", sessions.is_ok()),
            crate::flight_recorder::unsigned("generation", generation.unwrap_or_default()),
        ],
    );
    sessions.map(|_| generation)
}

fn agent_session_names_with_prefix(config: &Config, prefix: &str) -> Result<Vec<String>, String> {
    Ok(tmux_session_names(config)?
        .into_iter()
        .filter(|name| name.starts_with(prefix))
        .collect())
}

fn tmux_session_names(config: &Config) -> Result<Vec<String>, String> {
    let output = run_tmux_output_allow_failure(
        Command::new(config.tool("tmux")).env_remove("TMUX").args([
            "list-sessions",
            "-F",
            "#{session_name}",
        ]),
        ProcessPolicy::TmuxPoll,
    )?;
    if !output.status.success() {
        let stderr = output.stderr.trim();
        if tmux_missing_session_error(stderr) || stderr.contains("error connecting to") {
            return Ok(Vec::new());
        }
        return Err(if stderr.is_empty() {
            format!("tmux exited with {}", output.status)
        } else {
            stderr.to_string()
        });
    }
    Ok(output.stdout.lines().map(str::to_string).collect())
}

fn agent_session_prefix(repo: &Repository, branch: &str) -> String {
    let hash = stable_hash(repo.root.as_path());
    let branch = safe_tmux_name(&safe_branch_filename(branch));
    format!("prism-{branch}-{hash:016x}-")
}

fn legacy_agent_session_repo_prefix(repo: &Repository) -> String {
    let hash = stable_hash(repo.root.as_path());
    format!("prism-{hash:016x}-")
}

pub(crate) fn migrate_legacy_agent_sessions(
    repo: &Repository,
    config: &Config,
) -> Result<(), String> {
    let sessions = tmux_session_names(config)?;
    let hash = stable_hash(repo.root.as_path());
    let legacy_prefix = legacy_agent_session_repo_prefix(repo);
    for legacy_name in &sessions {
        let Some(suffix) = legacy_name.strip_prefix(&legacy_prefix) else {
            continue;
        };
        let Some((branch, generation)) = suffix.rsplit_once('-') else {
            continue;
        };
        if branch.is_empty() || generation.parse::<u64>().is_err() {
            continue;
        }
        if is_repository_hash(branch)
            || branch
                .rsplit_once('-')
                .is_some_and(|(_, possible_hash)| is_repository_hash(possible_hash))
        {
            continue;
        }
        // Legacy workers use this namespace with a fixed-width hash suffix.
        if matches!(branch, "worker-auto" | "worker-plan") && generation.len() == 16 {
            continue;
        }
        let name = format!("prism-{branch}-{hash:016x}-{generation}");
        if legacy_name == &name {
            continue;
        }
        if sessions.iter().any(|session| session == &name) {
            continue;
        }
        run_tmux_status(Command::new(config.tool("tmux")).env_remove("TMUX").args([
            "rename-session",
            "-t",
            legacy_name,
            &name,
        ]))
        .map_err(|error| format!("migrate tmux session '{legacy_name}' to '{name}': {error}"))?;
    }
    Ok(())
}

fn is_repository_hash(value: &str) -> bool {
    value.len() == 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn attach(config: &Config, runtime: &TmuxAgentSession, window: TmuxWindow) -> Result<(), String> {
    let target = runtime.target(window);
    attach_target(config, &target, &target)
}

fn attach_agent_session(config: &Config, runtime: &TmuxAgentSession) -> Result<(), String> {
    attach_target(config, &runtime.target(TmuxWindow::Agent), runtime.name())
}

fn attach_session(config: &Config, name: &str) -> Result<(), String> {
    attach_target(config, name, name)
}

fn attach_target(config: &Config, size_target: &str, attach_target: &str) -> Result<(), String> {
    run_tmux_status(Command::new(config.tool("tmux")).env_remove("TMUX").args([
        "set-option",
        "-w",
        "-t",
        size_target,
        "window-size",
        "latest",
    ]))?;
    let mut command = Command::new(config.tool("tmux"));
    command
        .env_remove("TMUX")
        .args(["attach-session", "-t", attach_target]);
    let descriptor = ProcessDescriptor::for_tmux(&command);
    run_status_inherited_named(&mut command, descriptor)
}

#[derive(Clone, Copy, Debug, Default)]
struct InteractiveAgentLaunch<'a> {
    prompt: Option<&'a str>,
    resume_session_id: Option<&'a str>,
    selection: crate::harness::AgentSelection<'a>,
}

fn create_detached_agent_session(
    repo: &Repository,
    config: &Config,
    session: &Session,
    runtime_session: &TmuxAgentSession,
    runtime: Option<&OpencodeRuntime>,
    launch: InteractiveAgentLaunch<'_>,
) -> Result<(), String> {
    let command = agent_shell_command(
        repo,
        config,
        session,
        runtime,
        launch.prompt,
        launch.resume_session_id,
        launch.selection,
    )?;
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

pub fn attach_resumable_harness_session(
    repo: &Repository,
    config: &Config,
    session: &Session,
    generation: u64,
    session_id: &str,
) -> Result<(), String> {
    let runtime_session = TmuxAgentSession::for_worktree_session(repo, &session.branch, generation);
    if session_exists(config, runtime_session.name())? {
        kill_session(config, runtime_session.name())?;
    }
    create_detached_agent_session(
        repo,
        config,
        session,
        &runtime_session,
        None,
        InteractiveAgentLaunch {
            prompt: None,
            resume_session_id: Some(session_id),
            selection: crate::harness::AgentSelection::default(),
        },
    )?;
    configure_agent_session(config, runtime_session.name(), None)?;
    ensure_companion_windows(config, session, &runtime_session)?;
    attach(config, &runtime_session, TmuxWindow::Agent)
}

fn configure_agent_session(
    config: &Config,
    name: &str,
    runtime: Option<&OpencodeRuntime>,
) -> Result<bool, String> {
    match configure_detach_on_destroy(config, name) {
        Ok(()) => {
            if let Some(runtime) = runtime {
                match configure_opencode_runtime(config, name, runtime) {
                    Ok(()) => {}
                    Err(error) if tmux_missing_session_error(&error) => return Ok(false),
                    Err(error) => return Err(error),
                }
            }
            Ok(true)
        }
        Err(error) if tmux_missing_session_error(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn configure_opencode_runtime(
    config: &Config,
    name: &str,
    runtime: &OpencodeRuntime,
) -> Result<(), String> {
    run_tmux_status(
        Command::new(config.tool("tmux"))
            .env_remove("TMUX")
            .args(["set-option", "-t", name, OPENCODE_RUNTIME_OPTION])
            .arg(opencode_runtime_marker(runtime)),
    )
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
        TmuxWindow::Terminal => crate::terminal::shell_program_from_env(),
    };
    let command = crate::terminal::posix_shell_quote(&command);
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
    run_tmux_output_allow_failure(
        Command::new(config.tool("tmux")).env_remove("TMUX").args([
            "list-windows",
            "-t",
            name,
            "-F",
            "#{window_index}",
        ]),
        ProcessPolicy::TmuxPoll,
    )
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
    let output = run_tmux_output_allow_failure(
        Command::new(config.tool("tmux"))
            .env_remove("TMUX")
            .args(["has-session", "-t", name]),
        ProcessPolicy::TmuxPoll,
    )?;
    if output.status.success() {
        return Ok(true);
    }
    let error = output.stderr.trim();
    if tmux_missing_session_error(error) || (error.is_empty() && output.status.code() == Some(1)) {
        Ok(false)
    } else if error.is_empty() {
        Err(format!("tmux has-session exited with {}", output.status))
    } else {
        Err(error.to_string())
    }
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

pub(crate) fn capture_agent_pane(
    repo: &Repository,
    config: &Config,
    branch: &str,
    generation: u64,
) -> Result<String, String> {
    let runtime = TmuxAgentSession::for_worktree_session(repo, branch, generation);
    capture_pane(config, &runtime.target(TmuxWindow::Agent), true)
}

pub(crate) fn resize_agent_pane(
    repo: &Repository,
    config: &Config,
    branch: &str,
    generation: u64,
    width: u16,
    height: u16,
) -> Result<(), String> {
    let runtime = TmuxAgentSession::for_worktree_session(repo, branch, generation);
    let output = run_tmux_output_allow_failure(
        Command::new(config.tool("tmux"))
            .env_remove("TMUX")
            .args(["resize-window", "-x"])
            .arg(width.to_string())
            .arg("-y")
            .arg(height.to_string())
            .args(["-t", &runtime.target(TmuxWindow::Agent)]),
        ProcessPolicy::TmuxCapture,
    )?;
    match tmux_output_result(output) {
        Ok(_) => Ok(()),
        Err(error) if tmux_missing_session_error(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn pane_capture(config: &Config, name: &str) -> Option<String> {
    capture_pane(config, name, false).ok()
}

fn capture_pane(config: &Config, target: &str, include_styles: bool) -> Result<String, String> {
    let mut command = Command::new(config.tool("tmux"));
    command.env_remove("TMUX").args(["capture-pane", "-p"]);
    if include_styles {
        command.args(["-e", "-N"]);
    }
    command.args(["-t", target]);
    let output = if include_styles {
        run_tmux_output_allow_failure(&mut command, ProcessPolicy::TmuxCapture)?
    } else {
        run_tmux_output_allow_failure(&mut command, ProcessPolicy::TmuxPoll)?
    };
    tmux_output_result(output)
}

fn tmux_output_result(output: crate::process::ProcessOutput) -> Result<String, String> {
    if output.status.success() {
        Ok(output.stdout)
    } else if output.stderr.trim().is_empty() {
        Err(format!("tmux exited with {}", output.status))
    } else {
        Err(output.stderr.trim().to_string())
    }
}

fn run_tmux_status(command: &mut Command) -> Result<(), String> {
    let descriptor = ProcessDescriptor::for_tmux(command);
    let output = run_output_named(command, ProcessPolicy::TmuxPoll, descriptor)?;
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
    let descriptor = ProcessDescriptor::for_tmux(command);
    run_status_with_stdin_named(command, stdin, ProcessPolicy::TmuxPoll, descriptor)
}

fn run_tmux_output_allow_failure(
    command: &mut Command,
    policy: ProcessPolicy,
) -> Result<crate::process::ProcessOutput, String> {
    let descriptor = ProcessDescriptor::for_tmux(command);
    run_output_allow_failure_named(command, policy, descriptor)
}

fn run_tmux_capture(command: &mut Command, policy: ProcessPolicy) -> Result<String, String> {
    let descriptor = ProcessDescriptor::for_tmux(command);
    run_capture_named(command, policy, descriptor)
}

fn tmux_missing_session_error(error: &str) -> bool {
    error.contains("can't find session")
        || error.contains("can't find window")
        || error.contains("can't find pane")
        || error.contains("no server running")
        || error.contains("error connecting to")
}

fn agent_shell_command(
    repo: &Repository,
    config: &Config,
    session: &Session,
    runtime: Option<&OpencodeRuntime>,
    prompt: Option<&str>,
    resume_session_id: Option<&str>,
    selection: crate::harness::AgentSelection<'_>,
) -> Result<String, String> {
    let invocation = interactive_agent_invocation(
        repo,
        config,
        session,
        runtime,
        prompt,
        resume_session_id,
        selection,
    )?;
    let argv = invocation.argv;
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
    let command = argv
        .iter()
        .map(|arg| crate::terminal::posix_shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let command = if invocation.environment.is_empty() {
        command
    } else {
        let assignments = invocation
            .environment
            .iter()
            .map(|(key, value)| format!("{}={}", key, crate::terminal::posix_shell_quote(value)))
            .collect::<Vec<_>>()
            .join(" ");
        format!("env {assignments} {command}")
    };
    if let Some(path) = invocation.prompt_file {
        Ok(format!(
            "{command}; prism_status=$?; rm -f {}; exit $prism_status",
            crate::terminal::posix_shell_quote(&path.display().to_string())
        ))
    } else {
        Ok(command)
    }
}

fn interactive_agent_invocation(
    repo: &Repository,
    config: &Config,
    session: &Session,
    runtime: Option<&OpencodeRuntime>,
    prompt: Option<&str>,
    resume_session_id: Option<&str>,
    selection: crate::harness::AgentSelection<'_>,
) -> Result<crate::harness::Invocation, String> {
    if uses_legacy_agent_override(config) {
        let argv = split_command_words(&config.agent_command(&config.default_agent));
        if argv.iter().any(|arg| arg.contains("{prompt")) {
            return Err(format!(
                "agent '{}' command contains a prompt placeholder; configure an interactive command for tmux attach",
                config.default_agent
            ));
        }
        return Ok(crate::harness::Invocation {
            argv,
            environment: std::collections::BTreeMap::new(),
            stdin: None,
            prompt_file: None,
            structured_events: false,
            attach: false,
        });
    }
    let runtime = runtime.cloned().or_else(|| {
        selected_adapter_is(config, "opencode")
            .then(|| usable_opencode_runtime(repo, config, session))
            .flatten()
    });
    let harness_config = config.harness_config(&config.default_harness)?;
    crate::harness::Harness::new(&config.default_harness, &harness_config)
        .interactive_argv_with_model(
            prompt,
            runtime.as_ref().map(|runtime| runtime.server_url.as_str()),
            resume_session_id.or_else(|| {
                runtime
                    .as_ref()
                    .and_then(|runtime| runtime.opencode_session_id.as_deref())
            }),
            &session.path,
            selection,
        )
}

fn opencode_runtime_for_session(
    repo: &Repository,
    config: &Config,
    session: &Session,
) -> Result<Option<OpencodeRuntime>, String> {
    if !selected_adapter_is(config, "opencode") || config.is_default_branch(&session.branch) {
        return Ok(None);
    }
    let harness_config = config.harness_config(&config.default_harness)?;
    crate::harness::Harness::new(&config.default_harness, &harness_config)
        .prepare_session(repo, config, &session.branch, &session.path)
        .map_err(|error| format!("prepare harness runtime: {error}"))
}

fn pane_current_command_result(config: &Config, name: &str) -> Result<Option<String>, String> {
    let result = run_tmux_capture(
        Command::new(config.tool("tmux"))
            .env_remove("TMUX")
            .args(["display-message", "-p", "-t"])
            .arg(name)
            .arg("#{pane_current_command}"),
        ProcessPolicy::TmuxPoll,
    );
    match result {
        Ok(output) => {
            let output = output.trim().to_string();
            Ok((!output.is_empty()).then_some(output))
        }
        Err(error) if tmux_missing_session_error(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn pane_start_command(config: &Config, name: &str) -> Option<String> {
    pane_start_command_result(config, name).ok().flatten()
}

fn pane_start_command_result(config: &Config, name: &str) -> Result<Option<String>, String> {
    let result = run_tmux_capture(
        Command::new(config.tool("tmux"))
            .env_remove("TMUX")
            .args(["display-message", "-p", "-t"])
            .arg(name)
            .arg("#{pane_start_command}"),
        ProcessPolicy::TmuxPoll,
    );
    match result {
        Ok(output) => {
            let output = output.trim().to_string();
            Ok((!output.is_empty()).then_some(output))
        }
        Err(error) if tmux_missing_session_error(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn pane_command_matches_agent(config: &Config, pane_command: &str) -> bool {
    let Some(expected) = config
        .harnesses
        .get(&config.default_harness)
        .and_then(|harness| harness.interactive_command.first())
        .cloned()
    else {
        return false;
    };
    let expected = Path::new(&expected)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&expected);
    pane_command == expected
}

fn selected_adapter_is(config: &Config, adapter: &str) -> bool {
    if uses_legacy_agent_override(config) {
        return false;
    }
    config
        .harnesses
        .get(&config.default_harness)
        .is_some_and(|harness| harness.adapter == adapter)
}

fn uses_legacy_agent_override(config: &Config) -> bool {
    config.default_agent != config.default_harness
        && config.agent_commands.contains_key(&config.default_agent)
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

fn usable_opencode_runtime(
    repo: &Repository,
    config: &Config,
    session: &Session,
) -> Option<OpencodeRuntime> {
    load_runtime(
        repo,
        &config.default_harness,
        &session.branch,
        &session.path,
    )
    .ok()
    .flatten()
    .filter(|runtime| {
        !runtime.server_url.is_empty()
            && runtime.worktree_path == session.path.display().to_string()
    })
}

fn agent_session_runtime_matches(
    config: &Config,
    name: &str,
    session: &Session,
    runtime: Option<&OpencodeRuntime>,
) -> bool {
    let Some(runtime) = runtime else {
        return true;
    };
    let expected = opencode_runtime_marker(runtime);
    let recorded = run_tmux_capture(
        Command::new(config.tool("tmux")).env_remove("TMUX").args([
            "show-options",
            "-v",
            "-t",
            name,
            OPENCODE_RUNTIME_OPTION,
        ]),
        ProcessPolicy::TmuxPoll,
    )
    .ok()
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty());
    if let Some(recorded) = recorded {
        return recorded == expected;
    }

    let Some(command) = pane_start_command(config, name) else {
        return false;
    };
    let argv = split_command_words(&command);
    if !argv.iter().any(|argument| argument == "attach") {
        return true;
    }
    command.contains(&runtime.server_url)
        && runtime
            .opencode_session_id
            .as_deref()
            .is_none_or(|session_id| command.contains(session_id))
        && command.contains(&session.path.display().to_string())
}

fn opencode_runtime_marker(runtime: &OpencodeRuntime) -> String {
    format!(
        "{}\t{}\t{}",
        runtime.server_url,
        runtime.opencode_session_id.as_deref().unwrap_or_default(),
        runtime.worktree_path
    )
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
    use crate::opencode::{OpencodeRuntime, save_runtime, server_url};
    use crate::remote::PrCache;
    use crate::repo::Repository;
    use crate::session::Session;

    use super::{
        TmuxAgentSession, TmuxWindow, attach_or_create_agent, attach_or_create_window,
        capture_agent_pane, ensure_agent_session, latest_agent_session_generation,
        migrate_legacy_agent_sessions, pane_command_matches_agent,
        pane_start_command_matches_agent, paste_agent_prompt, session_exists,
    };

    #[test]
    fn tmux_session_names_are_stable_and_safe() {
        let repo = Repository {
            root: PathBuf::from("/repo/my project"),
        };

        let runtime = TmuxAgentSession::for_worktree_session(&repo, "feature/foo:bar", 3);
        let name = runtime.name();

        assert_eq!(
            name,
            format!(
                "prism-feature_foo_bar-{:016x}-3",
                crate::util::stable_hash(repo.root.as_path())
            )
        );
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

        let error = super::agent_shell_command(
            &repo,
            &config,
            &session,
            None,
            None,
            None,
            crate::harness::AgentSelection::default(),
        )
        .unwrap_err();

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
        config.harnesses.insert(
            "opencode".to_string(),
            crate::harness::HarnessConfig::opencode("/usr/bin/opencode"),
        );
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(temp.join("worktree"), "feature");
        save_runtime(
            &repo,
            &OpencodeRuntime {
                repo_root: temp.display().to_string(),
                harness_id: "opencode".to_string(),
                branch: "feature".to_string(),
                worktree_path: session.path.display().to_string(),
                server_port: 41_234,
                server_url: server_url(41_234),
                server_pid: Some(123),
                server_process_identity: None,
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
        config.harnesses.insert(
            "opencode".to_string(),
            crate::harness::HarnessConfig::opencode("/usr/bin/opencode"),
        );
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
                harness_id: "opencode".to_string(),
                branch: "feature".to_string(),
                worktree_path: session.path.display().to_string(),
                server_port: port,
                server_url: server_url(port),
                server_pid: None,
                server_process_identity: None,
                opencode_session_id: None,
                generation: 1,
                updated_unix_ms: 42,
            },
        )
        .unwrap();

        let result = ensure_agent_session(&repo, &config, &session, 0);

        assert_eq!(result, Ok(false));
        let runtime = crate::opencode::load_runtime(&repo, "opencode", "feature", &session.path)
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
        assert!(!pane_command_matches_agent(&config, "opencode.exe"));
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
    fn startup_migration_renames_legacy_agent_sessions() {
        let temp = unique_temp_dir("prism-tmux-legacy-generation-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let tmux = temp.join("tmux");
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let prefix = super::agent_session_prefix(&repo, "feature");
        let legacy_repo_prefix = super::legacy_agent_session_repo_prefix(&repo);
        let legacy_prefix = format!("{legacy_repo_prefix}feature-");
        let hash_like_branch = legacy_repo_prefix
            .trim_start_matches("prism-")
            .trim_end_matches('-');
        let current_other_repo = format!("prism-{hash_like_branch}-0123456789abcdef-6");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  list-sessions)
    echo '{}4'
    echo '{}worker-auto-1234567890123456'
    echo '{}'
    exit 0
    ;;
  rename-session)
    exit 0
    ;;
esac
exit 1
"#,
                log.display(),
                legacy_prefix,
                legacy_repo_prefix,
                current_other_repo,
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

        let result = migrate_legacy_agent_sessions(&repo, &config);

        assert_eq!(result, Ok(()));
        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.contains(&format!("rename-session -t {legacy_prefix}4 {prefix}4")));
        assert!(!commands.contains(&format!(
            "rename-session -t {legacy_repo_prefix}worker-auto-1234567890123456"
        )));
        assert!(!commands.contains(&format!("rename-session -t {current_other_repo}")));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn startup_migration_accepts_an_existing_current_session() {
        let temp = unique_temp_dir("prism-tmux-existing-current-session-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let tmux = temp.join("tmux");
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let current_name = format!("{}0", super::agent_session_prefix(&repo, "feature"));
        let legacy_name = format!(
            "{}feature-0",
            super::legacy_agent_session_repo_prefix(&repo)
        );
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  list-sessions)
    echo '{}'
    echo '{}'
    exit 0
    ;;
  rename-session)
    exit 1
    ;;
esac
exit 1
"#,
                log.display(),
                legacy_name,
                current_name,
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

        let result = migrate_legacy_agent_sessions(&repo, &config);

        assert_eq!(result, Ok(()));
        let commands = fs::read_to_string(&log).unwrap();
        assert!(!commands.contains("rename-session"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn startup_migration_propagates_rename_failures() {
        let temp = unique_temp_dir("prism-tmux-legacy-migration-failure-test");
        fs::create_dir_all(&temp).unwrap();
        let tmux = temp.join("tmux");
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let legacy_prefix = format!("{}feature-", super::legacy_agent_session_repo_prefix(&repo));
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
case "$1" in
  list-sessions)
    echo '{}2'
    exit 0
    ;;
  rename-session)
    echo 'rename failed' >&2
    exit 1
    ;;
esac
exit 1
"#,
                legacy_prefix,
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

        let error = migrate_legacy_agent_sessions(&repo, &config).unwrap_err();

        assert!(error.contains("migrate tmux session"));
        assert!(error.contains("rename failed"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn startup_migration_accepts_missing_tmux_socket() {
        let temp = unique_temp_dir("prism-tmux-missing-socket-migration-test");
        fs::create_dir_all(&temp).unwrap();
        let tmux = temp.join("tmux");
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        fs::write(
            &tmux,
            "#!/bin/sh\necho 'error connecting to /tmp/tmux/prism (No such file or directory)' >&2\nexit 1\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();

        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());

        let result = migrate_legacy_agent_sessions(&repo, &config);

        assert_eq!(result, Ok(()));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn session_lookup_accepts_missing_tmux_socket() {
        let temp = unique_temp_dir("prism-tmux-missing-socket-session-test");
        fs::create_dir_all(&temp).unwrap();
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            "#!/bin/sh\necho 'error connecting to /tmp/tmux/prism (No such file or directory)' >&2\nexit 1\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();

        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());

        assert_eq!(session_exists(&config, "prism-test"), Ok(false));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn capture_agent_pane_does_not_resize_agent_window() {
        let temp = unique_temp_dir("prism-tmux-capture-pane-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
echo 'agent output'
"#,
                log.display(),
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
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let runtime = TmuxAgentSession::for_worktree_session(&repo, "feature", 4);

        assert_eq!(
            capture_agent_pane(&repo, &config, "feature", 4),
            Ok("agent output\n".to_string()),
        );
        assert_eq!(
            fs::read_to_string(&log).unwrap().trim(),
            format!("capture-pane -p -e -N -t {0}:1", runtime.name()),
        );

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
        assert!(commands.contains("capture-pane -p -t"));
        assert!(!commands.contains("capture-pane -p -e"));

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
        let port = match start_fake_opencode_server(
            session.path.clone(),
            204,
            Some(api_log.clone()),
            11,
        ) {
            Ok(port) => port,
            Err(error) if error.kind() == ErrorKind::PermissionDenied => return,
            Err(error) => panic!("start fake OpenCode server: {error}"),
        };
        save_runtime(
            &repo,
            &OpencodeRuntime {
                repo_root: temp.display().to_string(),
                harness_id: "opencode".to_string(),
                branch: "feature".to_string(),
                worktree_path: session.path.display().to_string(),
                server_port: port,
                server_url: server_url(port),
                server_pid: None,
                server_process_identity: None,
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
    fn configure_agent_session_reports_session_disappearing_before_runtime_marker() {
        let temp = unique_temp_dir("prism-tmux-vanished-before-runtime-marker-test");
        fs::create_dir_all(&temp).unwrap();
        let configure_count = temp.join("configure-count");
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
case "$1" in
  set-option)
    count="$(cat '{}' 2>/dev/null || echo 0)"
    count="$((count + 1))"
    echo "$count" > '{}'
    if [ "$count" -eq 2 ]; then
      echo "can't find session: vanished" >&2
      exit 1
    fi
    exit 0
    ;;
esac
exit 1
"#,
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
        let runtime = OpencodeRuntime {
            repo_root: temp.display().to_string(),
            harness_id: "opencode".to_string(),
            branch: "feature".to_string(),
            worktree_path: temp.join("worktree").display().to_string(),
            server_port: 41_234,
            server_url: server_url(41_234),
            server_pid: Some(123),
            server_process_identity: None,
            opencode_session_id: Some("ses_123".to_string()),
            generation: 1,
            updated_unix_ms: 42,
        };

        let result = super::configure_agent_session(&config, "prism-test", Some(&runtime));

        assert_eq!(result, Ok(false));

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
        let runtime = TmuxAgentSession::for_worktree_session(&repo, "feature", 0);
        assert!(commands.contains(&format!(
            "set-option -w -t {}:1 window-size latest",
            runtime.name()
        )));
        let attach = commands
            .lines()
            .find(|line| line.contains("attach-session -t "))
            .unwrap();
        assert!(!attach.contains(":1"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn attach_existing_agent_does_not_require_opencode_server() {
        let temp = unique_temp_dir("prism-tmux-existing-agent-attach-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let display_count = temp.join("display-count");
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
    count="$(cat '{}' 2>/dev/null || echo 0)"
    count="$((count + 1))"
    echo "$count" > '{}'
    [ "$count" -gt 1 ] || exit 1
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
                log.display(),
                display_count.display(),
                display_count.display()
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
        assert!(
            commands.find("window-size latest").unwrap()
                < commands.find("attach-session -t").unwrap(),
            "attach should restore client-driven sizing after portal capture"
        );
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
            .find(|line| line.contains("attach-session -t "))
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
        let request_target = request_line.split_whitespace().nth(1).unwrap_or_default();
        let request_path = request_target.split('?').next().unwrap_or(request_target);
        let method = request_line.split_whitespace().next().unwrap_or_default();
        let (status, body) = if method == "GET" && request_path == "/global/health" {
            (200, "{}".to_string())
        } else if method == "GET" && request_path == "/session/ses_123" {
            (200, session)
        } else if method == "GET" && request_path == "/session" {
            (200, format!(r#"{{"data":[{session}]}}"#))
        } else if method == "POST" && request_path == "/session/ses_123/prompt_async" {
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
