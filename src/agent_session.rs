use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::agent::AgentState;
use crate::repo::Repository;
use crate::session::{Session, save_agent_state};
use crate::tmux;
use crate::tui::ManagedRepo;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AgentSessionSlot {
    pub repo_index: usize,
    pub branch: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AgentSessionWarmupKey {
    pub slot: AgentSessionSlot,
    pub generation: u64,
}

pub(crate) struct AgentSessionWarmupResult {
    pub key: AgentSessionWarmupKey,
    pub running: Option<bool>,
    pub error: Option<String>,
}

impl AgentSessionSlot {
    pub(crate) fn for_session(session: &Session) -> Self {
        Self {
            repo_index: session.repo_index,
            branch: session.branch.clone(),
            path: session.path.clone(),
        }
    }
}

impl AgentSessionWarmupKey {
    pub(crate) fn new(slot: AgentSessionSlot, generation: u64) -> Self {
        Self { slot, generation }
    }
}

pub(crate) fn slot_for_session(session: &Session) -> AgentSessionSlot {
    AgentSessionSlot::for_session(session)
}

pub(crate) fn warmup_key(slot: AgentSessionSlot, generation: u64) -> AgentSessionWarmupKey {
    AgentSessionWarmupKey::new(slot, generation)
}

pub(crate) fn background_snapshot(session: &Session) -> Session {
    session.background_job_snapshot()
}

pub(crate) fn generation_for_slot(
    repos: &[ManagedRepo],
    generations: &mut BTreeMap<AgentSessionSlot, u64>,
    slot: &AgentSessionSlot,
) -> u64 {
    if let Some(generation) = generations.get(slot).copied() {
        return generation;
    }
    let generation = repos
        .get(slot.repo_index)
        .and_then(|repo| {
            tmux::latest_agent_session_generation(&repo.repo, &repo.config, &slot.branch)
        })
        .unwrap_or(0);
    generations.insert(slot.clone(), generation);
    generation
}

pub(crate) fn rotate_generation(
    repos: &[ManagedRepo],
    generations: &mut BTreeMap<AgentSessionSlot, u64>,
    slot: AgentSessionSlot,
) -> u64 {
    let generation = generation_for_slot(repos, generations, &slot).saturating_add(1);
    generations.insert(slot, generation);
    generation
}

pub(crate) fn key_is_current(
    generations: &BTreeMap<AgentSessionSlot, u64>,
    key: &AgentSessionWarmupKey,
) -> bool {
    generations.get(&key.slot).copied().unwrap_or(0) == key.generation
}

pub(crate) fn ensure_session(
    repo: &Repository,
    config: &crate::config::Config,
    session: &Session,
    generation: u64,
) -> Result<bool, String> {
    tmux::ensure_agent_session(repo, config, session, generation)
}

pub(crate) fn attach_session(
    repo: &Repository,
    config: &crate::config::Config,
    session: &Session,
    generation: u64,
) -> Result<bool, String> {
    tmux::attach_or_create_agent(repo, config, session, generation)?;
    Ok(tmux::agent_session_running(
        repo, config, session, generation,
    ))
}

pub(crate) fn attach_window(
    repo: &Repository,
    config: &crate::config::Config,
    session: &Session,
    generation: u64,
    window: tmux::TmuxWindow,
) -> Result<bool, String> {
    tmux::attach_or_create_window(repo, config, session, generation, window)?;
    Ok(tmux::agent_session_running(
        repo, config, session, generation,
    ))
}

pub(crate) fn retire_generation(
    repo: &Repository,
    config: &crate::config::Config,
    branch: &str,
    generation: u64,
) {
    let _ = tmux::kill_agent_session(repo, config, branch, generation);
}

pub(crate) fn submit_prompt(
    repo: &Repository,
    config: &crate::config::Config,
    session: &Session,
    generation: u64,
    prompt: &str,
) -> Result<bool, String> {
    tmux::paste_agent_prompt(repo, config, session, generation, prompt)?;
    Ok(tmux::agent_session_running(
        repo, config, session, generation,
    ))
}

pub(crate) fn update_observed_state(
    repos: &[ManagedRepo],
    sessions: &mut [Session],
    slot: &AgentSessionSlot,
    running: bool,
) -> bool {
    let Some(session) = sessions
        .iter_mut()
        .find(|session| slot_for_session(session) == *slot)
    else {
        return false;
    };
    persist_observed_state(repos, session, running)
}

pub(crate) fn apply_warmup_result(
    repos: &[ManagedRepo],
    fallback_repo: &Repository,
    sessions: &mut [Session],
    generations: &BTreeMap<AgentSessionSlot, u64>,
    result: AgentSessionWarmupResult,
) -> bool {
    if !key_is_current(generations, &result.key) {
        return false;
    }
    if let Some(error) = result.error {
        let repo = repos
            .get(result.key.slot.repo_index)
            .map(|repo| repo.repo.clone())
            .unwrap_or_else(|| fallback_repo.clone());
        let _ = crate::session::append_runtime_log(
            &repo,
            &format!(
                "tmux warm-up failed for {}#{}: {error}",
                result.key.slot.branch, result.key.generation
            ),
        );
        return false;
    }
    let Some(running) = result.running else {
        return false;
    };
    update_observed_state(repos, sessions, &result.key.slot, running)
}

fn persist_observed_state(repos: &[ManagedRepo], session: &mut Session, running: bool) -> bool {
    let Some(state) = observed_agent_state(session.agent_state, running) else {
        return false;
    };
    if session.agent_state == state {
        return false;
    }
    session.agent_state = state;
    if let Some(repo) = repos.get(session.repo_index) {
        let _ = save_agent_state(&repo.repo, &session.branch, state);
    }
    true
}

fn observed_agent_state(current: AgentState, tmux_agent_running: bool) -> Option<AgentState> {
    if tmux_agent_running {
        return Some(AgentState::NeedsInput);
    }
    if matches!(current, AgentState::Running | AgentState::NeedsRestart) {
        return Some(AgentState::ExitedOk);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::observed_agent_state;
    use crate::agent::AgentState;

    #[test]
    fn idle_tmux_opencode_session_does_not_count_as_running_agent() {
        let state = observed_agent_state(AgentState::Idle, true);

        assert_eq!(state, Some(AgentState::NeedsInput));
    }

    #[test]
    fn stale_running_state_without_process_is_cleared() {
        let state = observed_agent_state(AgentState::Running, false);

        assert_eq!(state, Some(AgentState::ExitedOk));
    }
}
