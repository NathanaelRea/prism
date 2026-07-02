use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use crate::agent::AgentState;
use crate::config::Config;
use crate::repo::Repository;
use crate::session::{Session, save_agent_state};
use crate::tmux;
use crate::tui::ManagedRepo;

const DELAYED_REWARM_AFTER_ATTACH: Duration = Duration::from_millis(250);

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

pub(crate) struct AgentSessionUse {
    pub slot: AgentSessionSlot,
    pub generation: u64,
    pub warmup_key: AgentSessionWarmupKey,
}

pub(crate) struct AgentSessionWarmupJob {
    pub key: AgentSessionWarmupKey,
    pub repo: Repository,
    pub config: Config,
    pub session: Session,
    pub delay: Duration,
}

pub(crate) struct AgentSessionDelayedWarmup {
    pub key: AgentSessionWarmupKey,
    pub delay: Duration,
}

pub(crate) struct AgentSessionLifecycleOutcome {
    pub delayed_warmup: Option<AgentSessionDelayedWarmup>,
}

pub(crate) struct AgentSessionAttachCompletion<'a> {
    pub repo: &'a Repository,
    pub config: &'a Config,
    pub session_use: AgentSessionUse,
    pub branch: &'a str,
    pub running: bool,
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

pub(crate) fn session_use(
    repos: &[ManagedRepo],
    generations: &mut BTreeMap<AgentSessionSlot, u64>,
    session: &Session,
) -> AgentSessionUse {
    let slot = AgentSessionSlot::for_session(session);
    let generation = generation_for_slot(repos, generations, &slot);
    let warmup_key = AgentSessionWarmupKey::new(slot.clone(), generation);
    AgentSessionUse {
        slot,
        generation,
        warmup_key,
    }
}

pub(crate) fn warmup_jobs_for_sessions(
    repos: &[ManagedRepo],
    sessions: &[Session],
    generations: &mut BTreeMap<AgentSessionSlot, u64>,
    in_flight: &BTreeSet<AgentSessionWarmupKey>,
) -> Vec<AgentSessionWarmupJob> {
    sessions
        .iter()
        .map(Session::background_job_snapshot)
        .filter_map(|session| {
            let use_ = session_use(repos, generations, &session);
            (!in_flight.contains(&use_.warmup_key))
                .then(|| {
                    repos
                        .get(session.repo_index)
                        .map(|repo| AgentSessionWarmupJob {
                            key: use_.warmup_key,
                            repo: repo.repo.clone(),
                            config: repo.config.clone(),
                            session,
                            delay: Duration::ZERO,
                        })
                })
                .flatten()
        })
        .collect()
}

pub(crate) fn warmup_job_for_key(
    repos: &[ManagedRepo],
    sessions: &[Session],
    generations: &BTreeMap<AgentSessionSlot, u64>,
    in_flight: &BTreeSet<AgentSessionWarmupKey>,
    key: AgentSessionWarmupKey,
    delay: Duration,
) -> Option<AgentSessionWarmupJob> {
    if in_flight.contains(&key) || !key_is_current(generations, &key) {
        return None;
    }
    let session = sessions
        .iter()
        .find(|session| AgentSessionSlot::for_session(session) == key.slot)?;
    let repo = repos.get(session.repo_index)?;
    Some(AgentSessionWarmupJob {
        key,
        repo: repo.repo.clone(),
        config: repo.config.clone(),
        session: session.background_job_snapshot(),
        delay,
    })
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

pub(crate) fn apply_attach_result(
    repos: &[ManagedRepo],
    sessions: &mut [Session],
    generations: &mut BTreeMap<AgentSessionSlot, u64>,
    completion: AgentSessionAttachCompletion<'_>,
) -> AgentSessionLifecycleOutcome {
    let slot = completion.session_use.slot;
    update_observed_state(repos, sessions, &slot, completion.running);
    if completion.running {
        return AgentSessionLifecycleOutcome {
            delayed_warmup: None,
        };
    }

    retire_generation(
        completion.repo,
        completion.config,
        completion.branch,
        completion.session_use.generation,
    );
    let next_generation = rotate_generation(repos, generations, slot.clone());
    AgentSessionLifecycleOutcome {
        delayed_warmup: Some(AgentSessionDelayedWarmup {
            key: AgentSessionWarmupKey::new(slot, next_generation),
            delay: DELAYED_REWARM_AFTER_ATTACH,
        }),
    }
}

pub(crate) fn apply_running_result(
    repos: &[ManagedRepo],
    sessions: &mut [Session],
    slot: &AgentSessionSlot,
    running: bool,
) -> bool {
    update_observed_state(repos, sessions, slot, running)
}

fn update_observed_state(
    repos: &[ManagedRepo],
    sessions: &mut [Session],
    slot: &AgentSessionSlot,
    running: bool,
) -> bool {
    let Some(session) = sessions
        .iter_mut()
        .find(|session| AgentSessionSlot::for_session(session) == *slot)
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
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    use super::{
        AgentSessionAttachCompletion, AgentSessionSlot, AgentSessionWarmupKey, apply_attach_result,
        observed_agent_state, session_use, warmup_job_for_key, warmup_jobs_for_sessions,
    };
    use crate::agent::AgentState;
    use crate::config::{Checks, Config, EscapeKey, MergeMethod};
    use crate::github::PrCache;
    use crate::repo::Repository;
    use crate::session::Session;
    use crate::tui::ManagedRepo;

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

    #[test]
    fn attach_result_rotates_generation_and_schedules_delayed_rewarm_when_agent_exits() {
        let repo = Repository {
            root: PathBuf::from("/tmp/prism-agent-session-test"),
        };
        let config = test_config();
        let repos = vec![ManagedRepo::new(repo.clone(), config.clone(), None)];
        let mut sessions = vec![test_session("feature")];
        sessions[0].agent_state = AgentState::Running;
        let slot = AgentSessionSlot::for_session(&sessions[0]);
        let mut generations = BTreeMap::from([(slot.clone(), 2)]);
        let completion_use = session_use(&repos, &mut generations, &sessions[0]);

        let outcome = apply_attach_result(
            &repos,
            &mut sessions,
            &mut generations,
            AgentSessionAttachCompletion {
                repo: &repo,
                config: &config,
                session_use: completion_use,
                branch: "feature",
                running: false,
            },
        );

        assert_eq!(sessions[0].agent_state, AgentState::ExitedOk);
        assert_eq!(generations.get(&slot), Some(&3));
        let delayed = outcome.delayed_warmup.expect("delayed warmup");
        assert_eq!(delayed.key, AgentSessionWarmupKey::new(slot, 3));
        assert!(!delayed.delay.is_zero());
    }

    #[test]
    fn warmup_job_for_key_rejects_stale_or_in_flight_generations() {
        let repo = Repository {
            root: PathBuf::from("/tmp/prism-agent-session-test"),
        };
        let config = test_config();
        let repos = vec![ManagedRepo::new(repo, config, None)];
        let sessions = vec![test_session("feature")];
        let slot = AgentSessionSlot::for_session(&sessions[0]);
        let generations = BTreeMap::from([(slot.clone(), 1)]);

        assert!(
            warmup_job_for_key(
                &repos,
                &sessions,
                &generations,
                &BTreeSet::new(),
                AgentSessionWarmupKey::new(slot.clone(), 0),
                std::time::Duration::ZERO,
            )
            .is_none()
        );

        let current = AgentSessionWarmupKey::new(slot, 1);
        let in_flight = BTreeSet::from([current.clone()]);
        assert!(
            warmup_job_for_key(
                &repos,
                &sessions,
                &generations,
                &in_flight,
                current,
                std::time::Duration::ZERO,
            )
            .is_none()
        );
    }

    #[test]
    fn warmup_jobs_for_sessions_uses_current_generation_interface() {
        let repo = Repository {
            root: PathBuf::from("/tmp/prism-agent-session-test"),
        };
        let config = test_config();
        let repos = vec![ManagedRepo::new(repo, config, None)];
        let sessions = vec![test_session("feature")];
        let slot = AgentSessionSlot::for_session(&sessions[0]);
        let mut generations = BTreeMap::from([(slot.clone(), 4)]);

        let jobs = warmup_jobs_for_sessions(&repos, &sessions, &mut generations, &BTreeSet::new());

        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].key, AgentSessionWarmupKey::new(slot, 4));
        assert_eq!(jobs[0].session.branch, "feature");
    }

    fn test_session(branch: &str) -> Session {
        Session {
            repo_index: 0,
            repo_label: "repo".to_string(),
            repo_key: None,
            path: PathBuf::from("/tmp/prism-agent-session-test/worktree"),
            path_display: "worktree".to_string(),
            branch: branch.to_string(),
            prompt_summary: String::new(),
            classification: crate::session::SessionClassification::Work,
            adopted: true,
            hidden: false,
            status_label: String::new(),
            agent_state: AgentState::Idle,
            opencode_status: None,
            pr: PrCache::default(),
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        }
    }

    fn test_config() -> Config {
        Config {
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
            auto: crate::config::AutoConfig::default(),
            layout: crate::config::LayoutConfig::default(),
            checks: Checks::default(),
            worktree_columns: Vec::new(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            prompt_templates: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/prism-user.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-repo.toml"),
        }
    }
}
