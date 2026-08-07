use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crate::workspace_state::{InspectRequest, WorkspaceContext, WorkspaceState};

use super::{
    TUI_ACTION_JOB_TIMEOUT, Tui, TuiJobKey, TuiJobKind, TuiJobPayload, WorkflowPollResult,
    WorkflowPollSnapshot,
};

impl Tui {
    pub(super) fn poll_workflow_runs(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.workflow_poll_rx.try_recv() {
            if result.revision != self.workflow_revision {
                continue;
            }
            let Ok(snapshot) = result.snapshot else {
                continue;
            };
            changed = true;
            self.workspace_repositories
                .insert(result.repository.clone(), snapshot.repository);
            changed |= self.worker_health.as_ref() != Some(&snapshot.worker_health);
            self.worker_health = Some(snapshot.worker_health);
        }
        let repositories = self
            .repos
            .iter()
            .map(|managed| managed.identity.clone())
            .collect::<BTreeSet<_>>();
        let previous = self.workspace_repositories.len();
        self.workspace_repositories
            .retain(|repository, _| repositories.contains(repository));
        changed |= previous != self.workspace_repositories.len();
        self.start_workflow_polls(false);
        changed
    }

    pub(crate) fn start_workflow_polls(&mut self, force: bool) {
        let revision = self.workflow_revision;
        let requests = self
            .repos
            .iter()
            .filter(|managed| {
                !self.workflow_polls_in_flight.contains(&managed.identity)
                    && (force
                        || self
                            .workflow_last_polled
                            .get(&managed.identity)
                            .is_none_or(|last| last.elapsed() >= Duration::from_secs(1)))
            })
            .map(|managed| (managed.repo.clone(), managed.identity.clone()))
            .collect::<Vec<_>>();
        for (repo, repository) in requests {
            self.workflow_polls_in_flight.insert(repository.clone());
            self.workflow_last_polled
                .insert(repository.clone(), Instant::now());
            let job_repository = repository.clone();
            self.spawn_tui_job(
                TuiJobKind::WorkflowPoll,
                TuiJobKey::WorkflowRepository(repository),
                revision,
                Some(TUI_ACTION_JOB_TIMEOUT),
                "prism-workflow-poll".to_string(),
                move |_| {
                    let worker_health =
                        crate::worker::probe_health().and_then(|health| match health.state {
                            crate::worker::DaemonState::Running => Ok(()),
                            crate::worker::DaemonState::Draining => Err(format!(
                                "Prism worker is draining ({} active)",
                                health.active
                            )),
                            crate::worker::DaemonState::Stopped => {
                                Err("Prism worker is stopped".to_string())
                            }
                        });
                    let repository_snapshot = WorkspaceState::open(WorkspaceContext {
                        repo: Some(repo.root.clone()),
                        cwd: repo.root.clone(),
                    })?
                    .inspect(InspectRequest {
                        include_hidden: true,
                        include_terminal: true,
                    })?
                    .repositories
                    .into_iter()
                    .next()
                    .ok_or_else(|| "workspace inspection returned no repository".to_string())?;
                    Ok(Some(TuiJobPayload::WorkflowPoll(WorkflowPollResult {
                        repository: job_repository,
                        revision,
                        snapshot: Ok(WorkflowPollSnapshot {
                            repository: repository_snapshot,
                            worker_health,
                        }),
                    })))
                },
            );
        }
    }
}
