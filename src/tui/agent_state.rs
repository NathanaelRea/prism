use crate::agent::AgentState;
use crate::desktop_notification::AgentObservation;
use crate::repo::Repository;
use crate::session::WorktreeSessionKey;
use std::collections::BTreeSet;

use super::{TUI_ACTION_JOB_TIMEOUT, Tui, TuiJobKey, TuiJobKind};

#[derive(Clone, Copy)]
pub(super) enum AgentObservationMode {
    Baseline,
    Transition,
    AttachedLiveness,
}

pub(super) struct AgentStatePersistenceRequest {
    generation: u64,
    repo: Repository,
    worktree_session_id: String,
    branch: String,
    state: Option<AgentState>,
}

impl Tui {
    pub(crate) fn apply_agent_state(
        &mut self,
        session_index: usize,
        state: AgentState,
        notify: bool,
    ) -> bool {
        let Some(session) = self.sessions.get_mut(session_index) else {
            return false;
        };
        if session.agent_state == state {
            return false;
        }
        session.agent_state = state;
        self.queue_agent_state_persistence(session_index);
        let mode = if notify {
            AgentObservationMode::Transition
        } else {
            AgentObservationMode::Baseline
        };
        self.submit_agent_observation(session_index, mode);
        true
    }

    pub(crate) fn accept_external_agent_state_change(
        &mut self,
        session_index: usize,
        previous: AgentState,
        suppress_attached_liveness: bool,
    ) -> bool {
        let Some(current) = self
            .sessions
            .get(session_index)
            .map(|session| session.agent_state)
        else {
            return false;
        };
        if current == previous {
            return false;
        }
        self.queue_agent_state_persistence(session_index);
        let replayed_liveness = suppress_attached_liveness
            && current == AgentState::Attached
            && matches!(
                previous,
                AgentState::NeedsInput
                    | AgentState::ExitedOk
                    | AgentState::ExitedError
                    | AgentState::NeedsRestart
            );
        let mode = if replayed_liveness {
            AgentObservationMode::AttachedLiveness
        } else {
            AgentObservationMode::Transition
        };
        self.submit_agent_observation(session_index, mode);
        true
    }

    pub(crate) fn observe_current_agent_state(&mut self, session_index: usize) {
        self.submit_agent_observation(session_index, AgentObservationMode::Transition);
    }

    pub(super) fn submit_agent_observation(
        &mut self,
        session_index: usize,
        mode: AgentObservationMode,
    ) {
        let Some(session) = self.sessions.get(session_index) else {
            return;
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return;
        };
        let key = session.identity_key(&managed.identity);
        let state = session.agent_state;
        let config = managed.config.notifications;
        let observation = AgentObservation {
            session: &key,
            repo_label: &session.repo_label,
            branch: &session.branch,
            state,
            config,
        };
        match mode {
            AgentObservationMode::Baseline => self.desktop_notifier.baseline(observation),
            AgentObservationMode::Transition => self.desktop_notifier.observe(observation),
            AgentObservationMode::AttachedLiveness => {
                self.desktop_notifier.observe_attached_liveness(observation)
            }
        }
    }

    pub(crate) fn reseed_desktop_notifications(&mut self) {
        let observations = self
            .sessions
            .iter()
            .filter_map(|session| {
                let managed = self.repos.get(session.repo_index)?;
                Some((
                    session.identity_key(&managed.identity),
                    session.repo_label.clone(),
                    session.branch.clone(),
                    session.agent_state,
                    managed.config.notifications,
                ))
            })
            .collect::<Vec<_>>();
        self.desktop_notifier.seed(observations.iter().map(
            |(session, repo_label, branch, state, config)| AgentObservation {
                session,
                repo_label,
                branch,
                state: *state,
                config: *config,
            },
        ));
    }

    pub(crate) fn queue_agent_state_persistence(&mut self, session_index: usize) {
        let Some(session) = self.sessions.get(session_index) else {
            return;
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return;
        };
        let worktree = session.identity_key(&managed.identity);
        let generation = self
            .worktree_generations
            .get(&worktree)
            .copied()
            .unwrap_or_default();
        self.agent_state_persistence_pending.insert(
            worktree.clone(),
            AgentStatePersistenceRequest {
                generation,
                repo: managed.repo.clone(),
                worktree_session_id: session.worktree_session_id.clone(),
                branch: session.branch.clone(),
                state: Some(session.agent_state),
            },
        );
        self.start_agent_state_persistence_jobs();
    }

    pub(crate) fn queue_agent_state_removal(&mut self, session_index: usize) {
        let Some(session) = self.sessions.get(session_index) else {
            return;
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return;
        };
        let worktree = session.identity_key(&managed.identity);
        let generation = self
            .worktree_generations
            .get(&worktree)
            .copied()
            .unwrap_or_default();
        self.agent_state_persistence_pending.insert(
            worktree,
            AgentStatePersistenceRequest {
                generation,
                repo: managed.repo.clone(),
                worktree_session_id: session.worktree_session_id.clone(),
                branch: session.branch.clone(),
                state: None,
            },
        );
        self.start_agent_state_persistence_jobs();
    }

    pub(crate) fn retain_agent_state_persistence_for(
        &mut self,
        live: &BTreeSet<WorktreeSessionKey>,
    ) {
        self.agent_state_persistence_pending
            .retain(|key, request| live.contains(key) || request.state.is_none());
    }

    pub(super) fn start_agent_state_persistence_jobs(&mut self) {
        let keys = self
            .agent_state_persistence_pending
            .keys()
            .filter(|key| !self.agent_state_persistence_in_flight.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            let Some(request) = self.agent_state_persistence_pending.remove(&key) else {
                continue;
            };
            self.agent_state_persistence_in_flight.insert(key.clone());
            self.spawn_tui_job(
                TuiJobKind::AgentStatePersistence,
                TuiJobKey::AgentStatePersistence(key),
                request.generation,
                Some(TUI_ACTION_JOB_TIMEOUT),
                "prism-agent-state-persistence".to_string(),
                move |_| {
                    match request.state {
                        Some(state) => crate::session::save_agent_state(
                            &request.repo,
                            &request.worktree_session_id,
                            &request.branch,
                            state,
                        )?,
                        None => crate::session::remove_agent_state(
                            &request.repo,
                            &request.worktree_session_id,
                            &request.branch,
                        )?,
                    }
                    Ok(None)
                },
            );
        }
    }
}
