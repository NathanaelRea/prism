use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::auto_flow::{
    AutoOutputLine, AutoRunStatus, PersistedAutoRun, load_auto_run_snapshot,
    load_output_lines as load_auto_output_lines, load_recent_active_run_snapshots_for_repo,
    load_terminal_repair_run_snapshots_for_repo,
};
use crate::plan_run::{
    PersistedPlanRun, PlanOutputLine, PlanRunStatus, PlanStepStatus, load_output_lines,
    load_plan_run, load_recent_plan_runs_for_repo,
};
use crate::repo::Repository;
use crate::session::WorktreeRepositoryKey;
use crate::view;
use crate::workspace_state::{
    InspectRequest, WorkflowLifecycle, WorkflowSnapshot, WorkspaceContext, WorkspaceState,
};

use super::{
    DashboardOutputKey, DashboardOutputLines, DashboardOutputResult, PanelFocus,
    TUI_ACTION_JOB_TIMEOUT, Tui, TuiJobKey, TuiJobKind, TuiJobPayload, WorkflowPollResult,
    WorkflowPollSnapshot,
};

fn preferred_plan_step(run: &PersistedPlanRun) -> usize {
    run.steps
        .iter()
        .filter(|step| {
            matches!(
                step.status,
                PlanStepStatus::Starting | PlanStepStatus::Running
            )
        })
        .max_by_key(|step| (step.started_unix_ms.unwrap_or(0), step.step))
        .or_else(|| {
            run.steps
                .iter()
                .filter(|step| {
                    !matches!(step.status, PlanStepStatus::Done | PlanStepStatus::Skipped)
                })
                .filter(|step| step.started_unix_ms.is_some() || step.finished_unix_ms.is_some())
                .max_by_key(|step| {
                    (
                        step.started_unix_ms.or(step.finished_unix_ms).unwrap_or(0),
                        step.step,
                    )
                })
        })
        .or_else(|| {
            run.steps
                .iter()
                .filter(|step| {
                    matches!(
                        step.status,
                        PlanStepStatus::Done
                            | PlanStepStatus::Failed
                            | PlanStepStatus::Aborted
                            | PlanStepStatus::Skipped
                    )
                })
                .max_by_key(|step| (step.finished_unix_ms.unwrap_or(0), step.step))
        })
        .or_else(|| {
            run.steps
                .iter()
                .find(|step| step.step == run.run.selected_step)
        })
        .or_else(|| run.steps.iter().max_by_key(|step| step.step))
        .map(|step| step.step)
        .unwrap_or(run.run.selected_step)
}

fn plan_run_status_sort_key(status: PlanRunStatus) -> u8 {
    match status {
        PlanRunStatus::Running => 0,
        PlanRunStatus::Queued => 1,
        PlanRunStatus::Paused => 2,
        PlanRunStatus::Failed => 3,
        PlanRunStatus::Aborted => 4,
        PlanRunStatus::Draft => 5,
        PlanRunStatus::Done => 6,
    }
}

pub(super) fn auto_status(status: WorkflowLifecycle) -> Option<AutoRunStatus> {
    match status {
        WorkflowLifecycle::Queued => Some(AutoRunStatus::Queued),
        WorkflowLifecycle::Running => Some(AutoRunStatus::Running),
        WorkflowLifecycle::Paused => Some(AutoRunStatus::Paused),
        WorkflowLifecycle::Failed => Some(AutoRunStatus::Failed),
        WorkflowLifecycle::Done => Some(AutoRunStatus::Done),
        WorkflowLifecycle::Aborted => Some(AutoRunStatus::Aborted),
        WorkflowLifecycle::Draft => None,
    }
}

fn auto_run_priority(status: AutoRunStatus) -> u8 {
    match status {
        AutoRunStatus::Running => 0,
        AutoRunStatus::Queued => 1,
        AutoRunStatus::Paused => 2,
        AutoRunStatus::Failed => 3,
        AutoRunStatus::Aborted => 4,
        AutoRunStatus::Done => 5,
    }
}

pub(super) fn plan_status(status: WorkflowLifecycle) -> Option<PlanRunStatus> {
    match status {
        WorkflowLifecycle::Draft => Some(PlanRunStatus::Draft),
        WorkflowLifecycle::Queued => Some(PlanRunStatus::Queued),
        WorkflowLifecycle::Running => Some(PlanRunStatus::Running),
        WorkflowLifecycle::Paused => Some(PlanRunStatus::Paused),
        WorkflowLifecycle::Failed => Some(PlanRunStatus::Failed),
        WorkflowLifecycle::Done => Some(PlanRunStatus::Done),
        WorkflowLifecycle::Aborted => Some(PlanRunStatus::Aborted),
    }
}

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
            self.worker_health = Some(snapshot.worker_health.clone());
            if let Ok(runs) = snapshot.plan_runs {
                for run in runs {
                    changed |= self.remember_plan_run_snapshot(run);
                }
            }
            if let Ok(runs) = snapshot.auto_runs {
                for run in runs {
                    changed |= self.remember_auto_run_snapshot(run);
                }
            }
            if let Ok(runs) = snapshot.linked_plan_runs {
                for run in runs {
                    let run_id = run.run.id.clone();
                    changed |= self.linked_plan_runs.get(&run_id) != Some(&run);
                    self.linked_plan_runs.insert(run_id, run);
                }
            }
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
                    let snapshot = crate::observability::with_nonblocking_read_db_named(
                        &repo,
                        "tui.workflow.refresh",
                        |conn| {
                            let plan_runs = load_recent_plan_runs_for_repo(conn, &repo.root, 8);
                            let auto_runs = (|| {
                                let mut runs =
                                    load_recent_active_run_snapshots_for_repo(conn, &repo.root, 8)?;
                                let active_ids = runs
                                    .iter()
                                    .map(|run| run.run.id.clone())
                                    .collect::<BTreeSet<_>>();
                                runs.extend(
                                    load_terminal_repair_run_snapshots_for_repo(conn, &repo.root)?
                                        .into_iter()
                                        .filter(|run| !active_ids.contains(&run.run.id)),
                                );
                                Ok(runs)
                            })();
                            let linked_plan_runs = match &auto_runs {
                                Ok(runs) => {
                                    let plan_ids = runs
                                        .iter()
                                        .flat_map(|run| &run.steps)
                                        .filter_map(|step| step.plan_run_id.as_ref())
                                        .collect::<BTreeSet<_>>();
                                    plan_ids
                                        .into_iter()
                                        .filter_map(|run_id| {
                                            load_plan_run(conn, run_id).transpose()
                                        })
                                        .collect::<Result<Vec<_>, _>>()
                                }
                                Err(_) => Ok(Vec::new()),
                            };
                            Ok(WorkflowPollSnapshot {
                                repository: repository_snapshot,
                                plan_runs,
                                auto_runs,
                                linked_plan_runs,
                                worker_health,
                            })
                        },
                    );
                    Ok(Some(TuiJobPayload::WorkflowPoll(WorkflowPollResult {
                        repository: job_repository,
                        revision,
                        snapshot,
                    })))
                },
            );
        }
    }

    pub(crate) fn load_plan_run_snapshot(&mut self, repo_root: &Path, run_id: &str) {
        let repo = Repository {
            root: repo_root.to_path_buf(),
        };
        if let Ok(Some(run)) = crate::observability::with_nonblocking_read_db_named(
            &repo,
            "tui.plan_run.snapshot",
            |conn| load_plan_run(conn, run_id),
        ) {
            self.remember_plan_run(run);
        }
    }

    pub(crate) fn remember_plan_run(&mut self, run: PersistedPlanRun) -> bool {
        let changed = self.remember_plan_run_snapshot(run);
        if changed {
            self.workflow_revision = self.workflow_revision.saturating_add(1);
        }
        changed
    }

    pub(crate) fn invalidate_workflow_snapshots(&mut self) {
        self.workflow_revision = self.workflow_revision.saturating_add(1);
    }

    pub(super) fn remember_plan_run_snapshot(&mut self, run: PersistedPlanRun) -> bool {
        let run_id = run.run.id.clone();
        let scope_path = run.run.scope_path.clone();
        let selected_step = self.resolved_plan_step_selection(&run);
        self.selected_plan_step_by_run
            .insert(run_id.clone(), selected_step);
        let selected_run_is_known = self
            .active_plan_runs
            .get(&scope_path)
            .is_some_and(|selected| selected == &run_id || self.plan_runs.contains_key(selected));
        if !selected_run_is_known {
            self.active_plan_runs.insert(scope_path, run_id.clone());
        }
        let changed = self.plan_runs.get(&run_id) != Some(&run);
        self.plan_runs.insert(run_id, run);
        changed
    }

    pub(crate) fn current_plan_dashboard(&self) -> Option<view::PlanDashboard> {
        if self.focused_panel != PanelFocus::Worktrees {
            return None;
        }
        let (repo, run_id) = self.selected_plan_run_id()?;
        let mut run = self.plan_runs.get(&run_id)?.clone();
        let run_scope_path = run.run.scope_path.clone();
        run.run.selected_step = self.resolved_plan_step_selection(&run);
        let output_lines = self.plan_output_snapshot(&repo, &run.run.id, run.run.selected_step);
        let mut output_state = self
            .plan_output_state_by_run
            .get(&run.run.id)
            .cloned()
            .unwrap_or_else(|| view::PlanOutputViewerState {
                cursor: output_lines.len().saturating_sub(1),
                follow: true,
                expanded_blocks: BTreeSet::new(),
            });
        if output_state.follow {
            output_state.cursor = output_lines.len().saturating_sub(1);
        } else if !output_lines.is_empty() {
            output_state.cursor = output_state
                .cursor
                .min(output_lines.len().saturating_sub(1));
        }
        Some(view::PlanDashboard {
            run,
            runs: self.plan_run_summaries_for_scope(&repo.root, &run_scope_path, Some(&run_id)),
            output_lines,
            output_state,
        })
    }

    pub(super) fn selected_plan_run_id(&self) -> Option<(Repository, String)> {
        let (repo, scope_path) = self.selected_plan_scope()?;
        let run_ids = self.plan_run_ids_for_scope(&repo.root, &scope_path);
        let selected = self
            .active_plan_runs
            .get(&scope_path)
            .filter(|run_id| run_ids.iter().any(|candidate| candidate == *run_id))
            .cloned()
            .or_else(|| run_ids.first().cloned())?;
        Some((repo, selected))
    }

    pub(super) fn plan_run_ids_for_scope(
        &self,
        repo_root: &Path,
        scope_path: &Path,
    ) -> Vec<String> {
        let repo_root = repo_root.display().to_string();
        let mut runs = self
            .plan_runs
            .values()
            .filter(|run| {
                run.run.repo_root == repo_root
                    && run.run.scope_path == scope_path
                    && run.run.archived_unix_ms.is_none()
            })
            .collect::<Vec<_>>();
        runs.sort_by_key(|run| {
            (
                plan_run_status_sort_key(run.run.status),
                std::cmp::Reverse(run.run.updated_unix_ms),
            )
        });
        runs.into_iter().map(|run| run.run.id.clone()).collect()
    }

    pub(super) fn plan_run_summaries_for_scope(
        &self,
        repo_root: &Path,
        scope_path: &Path,
        selected_run_id: Option<&str>,
    ) -> Vec<view::PlanRunSummary> {
        let selected = self.active_plan_runs.get(scope_path);
        self.plan_run_ids_for_scope(repo_root, scope_path)
            .into_iter()
            .filter_map(|run_id| {
                let run = self.plan_runs.get(&run_id)?;
                Some(view::PlanRunSummary {
                    id: run.run.id.clone(),
                    plan_display: run.run.plan_display.clone(),
                    scope_path: run.run.scope_path.display().to_string(),
                    status: run.run.status,
                    updated_unix_ms: run.run.updated_unix_ms,
                    selected: selected_run_id
                        .map(|selected| selected == run_id.as_str())
                        .unwrap_or(selected == Some(&run_id)),
                })
            })
            .collect()
    }

    pub(crate) fn move_plan_run_selection(&mut self, direction: isize) -> bool {
        let Some((repo, selected_run_id)) = self.selected_plan_run_id() else {
            return false;
        };
        let Some(selected_run) = self.plan_runs.get(&selected_run_id) else {
            return false;
        };
        let scope_path = selected_run.run.scope_path.clone();
        let run_ids = self.plan_run_ids_for_scope(&repo.root, &scope_path);
        if run_ids.len() < 2 {
            return false;
        }
        let current = run_ids
            .iter()
            .position(|run_id| run_id == &selected_run_id)
            .unwrap_or(0);
        let next = if direction < 0 {
            if current == 0 {
                run_ids.len() - 1
            } else {
                current.saturating_sub(direction.unsigned_abs())
            }
        } else {
            (current + direction as usize) % run_ids.len()
        };
        self.active_plan_runs
            .insert(scope_path, run_ids[next].clone());
        true
    }

    pub(crate) fn load_auto_run_snapshot(&mut self, repo_root: &Path, run_id: &str) {
        let repo = Repository {
            root: repo_root.to_path_buf(),
        };
        if let Ok(Some(run)) = crate::observability::with_nonblocking_read_db_named(
            &repo,
            "tui.auto_run.snapshot",
            |conn| load_auto_run_snapshot(conn, run_id),
        ) {
            self.remember_auto_run(run);
        }
    }

    pub(crate) fn remember_auto_run(&mut self, run: PersistedAutoRun) -> bool {
        let changed = self.remember_auto_run_snapshot(run);
        if changed {
            self.workflow_revision = self.workflow_revision.saturating_add(1);
        }
        changed
    }

    pub(super) fn remember_auto_run_snapshot(&mut self, run: PersistedAutoRun) -> bool {
        let run_id = run.run.id.clone();
        let selected_step = self
            .selected_auto_step_by_run
            .get(&run_id)
            .copied()
            .or(run.run.selected_step_run_id)
            .or_else(|| run.steps.first().and_then(|step| step.id));
        if let Some(selected_step) = selected_step {
            self.selected_auto_step_by_run
                .insert(run_id.clone(), selected_step);
        }
        let is_active = matches!(
            run.run.status,
            AutoRunStatus::Queued | AutoRunStatus::Running | AutoRunStatus::Paused
        );
        if is_active {
            let replace_active = self
                .active_auto_runs
                .get(&run.run.worktree_path)
                .and_then(|active| self.auto_runs.get(active))
                .is_none_or(|active| {
                    !matches!(
                        active.run.status,
                        AutoRunStatus::Queued | AutoRunStatus::Running | AutoRunStatus::Paused
                    ) || auto_run_priority(run.run.status) < auto_run_priority(active.run.status)
                });
            if replace_active {
                self.active_auto_runs
                    .insert(run.run.worktree_path.clone(), run_id.clone());
            }
        } else if self.active_auto_runs.get(&run.run.worktree_path) == Some(&run_id) {
            self.active_auto_runs.remove(&run.run.worktree_path);
        }
        if self.selected_auto_run.is_none() {
            self.selected_auto_run = Some(run_id.clone());
        }
        let changed = self.auto_runs.get(&run_id) != Some(&run);
        self.auto_runs.insert(run_id, run);
        changed
    }

    pub(super) fn poll_dashboard_outputs(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.dashboard_output_rx.try_recv() {
            if result.revision != self.workflow_revision {
                continue;
            }
            let Ok(lines) = result.lines else {
                continue;
            };
            match (&result.key, lines) {
                (
                    DashboardOutputKey::Plan { run_id, step, .. },
                    DashboardOutputLines::Plan(lines),
                ) => {
                    let key = (run_id.clone(), *step);
                    changed |= self.plan_output_cache.borrow().get(&key) != Some(&lines);
                    self.plan_output_cache.borrow_mut().insert(key, lines);
                }
                (
                    DashboardOutputKey::Auto {
                        repository,
                        step_run_id,
                    },
                    DashboardOutputLines::Auto(lines),
                ) => {
                    let key = (repository.clone(), *step_run_id);
                    changed |= self.auto_output_cache.borrow().get(&key) != Some(&lines);
                    self.auto_output_cache.borrow_mut().insert(key, lines);
                }
                _ => {}
            }
        }

        let revision = self.workflow_revision;
        let requests = self.dashboard_output_requests();
        for (key, repo) in requests {
            if self.dashboard_outputs_in_flight.contains(&key)
                || self
                    .dashboard_output_last_polled
                    .get(&key)
                    .is_some_and(|last| last.elapsed() < Duration::from_secs(1))
            {
                continue;
            }
            self.dashboard_outputs_in_flight.insert(key.clone());
            self.dashboard_output_last_polled
                .insert(key.clone(), Instant::now());
            let job_key = key.clone();
            self.spawn_tui_job(
                TuiJobKind::DashboardOutput,
                TuiJobKey::DashboardOutput(key),
                revision,
                Some(TUI_ACTION_JOB_TIMEOUT),
                "prism-dashboard-output".to_string(),
                move |_| {
                    let lines = crate::observability::with_nonblocking_read_db_named(
                        &repo,
                        "tui.dashboard_output.refresh",
                        |conn| match &job_key {
                            DashboardOutputKey::Plan { run_id, step, .. } => {
                                load_output_lines(conn, run_id, *step)
                                    .map(DashboardOutputLines::Plan)
                            }
                            DashboardOutputKey::Auto { step_run_id, .. } => {
                                load_auto_output_lines(conn, *step_run_id)
                                    .map(DashboardOutputLines::Auto)
                            }
                        },
                    );
                    Ok(Some(TuiJobPayload::DashboardOutput(
                        DashboardOutputResult {
                            key: job_key,
                            revision,
                            lines,
                        },
                    )))
                },
            );
        }
        changed
    }

    pub(super) fn dashboard_output_requests(&self) -> BTreeMap<DashboardOutputKey, Repository> {
        let mut requests = BTreeMap::new();
        if let Some((repo, run_id)) = self.selected_plan_run_id()
            && let Some(run) = self.plan_runs.get(&run_id)
            && let Some(repository) = self.managed_repository_identity(&repo)
        {
            requests.insert(
                DashboardOutputKey::Plan {
                    repository: repository.clone(),
                    run_id,
                    step: self.resolved_plan_step_selection(run),
                },
                repo,
            );
        }
        if let Some((repo, worktree_path)) = self.selected_auto_scope()
            && let Some(run_id) = self.auto_run_id_for_worktree(&worktree_path)
            && let Some(run) = self.auto_runs.get(run_id)
        {
            let selected_step_run_id = self
                .selected_auto_step_by_run
                .get(run_id)
                .copied()
                .or(run.run.selected_step_run_id)
                .or_else(|| run.steps.first().and_then(|step| step.id));
            if let Some(step_run_id) = selected_step_run_id {
                let Some(repository) = self.managed_repository_identity(&repo).cloned() else {
                    return requests;
                };
                requests.insert(
                    DashboardOutputKey::Auto {
                        repository: repository.clone(),
                        step_run_id,
                    },
                    repo.clone(),
                );
                if let Some(plan_run_id) = run
                    .steps
                    .iter()
                    .find(|step| step.id == Some(step_run_id))
                    .and_then(|step| step.plan_run_id.as_ref())
                    && let Some(plan_run) = self
                        .plan_runs
                        .get(plan_run_id)
                        .or_else(|| self.linked_plan_runs.get(plan_run_id))
                {
                    requests.insert(
                        DashboardOutputKey::Plan {
                            repository,
                            run_id: plan_run_id.clone(),
                            step: self.resolved_plan_step_selection(plan_run),
                        },
                        repo,
                    );
                }
            }
        }
        requests
    }

    pub(crate) fn current_auto_dashboard(&self) -> Option<view::AutoDashboard> {
        let (repo, worktree_path) = self.selected_auto_scope()?;
        let run_id = self.auto_run_id_for_worktree(&worktree_path)?;
        let mut run = self.auto_runs.get(run_id)?.clone();
        if let Some(selected_step) = self.selected_auto_step_by_run.get(run_id).copied() {
            run.run.selected_step_run_id = Some(selected_step);
        }
        let selected_step_run_id = run
            .run
            .selected_step_run_id
            .or_else(|| run.steps.first().and_then(|step| step.id));
        let output_lines = selected_step_run_id
            .map(|step_run_id| self.auto_output_snapshot(&repo, step_run_id))
            .unwrap_or_default();
        let mut output_state = self
            .auto_output_state_by_run
            .get(&run.run.id)
            .cloned()
            .unwrap_or_else(|| view::AutoOutputViewerState {
                cursor: output_lines.len().saturating_sub(1),
                follow: true,
            });
        if output_state.follow {
            output_state.cursor = output_lines.len().saturating_sub(1);
        } else if !output_lines.is_empty() {
            output_state.cursor = output_state
                .cursor
                .min(output_lines.len().saturating_sub(1));
        }
        let linked_plan_dashboard = run
            .steps
            .iter()
            .find(|step| step.id == selected_step_run_id)
            .and_then(|step| step.plan_run_id.as_deref())
            .and_then(|plan_run_id| self.linked_plan_dashboard(&repo, plan_run_id));
        Some(view::AutoDashboard {
            run,
            linked_plan_dashboard,
            output_lines,
            output_state,
            worker_status: match &self.worker_health {
                Some(Ok(())) => "healthy".to_string(),
                Some(Err(_)) => "unavailable".to_string(),
                None => "checking".to_string(),
            },
        })
    }

    pub(super) fn auto_run_id_for_worktree(&self, worktree_path: &Path) -> Option<&String> {
        if let Some(run_id) = self.active_auto_runs.get(worktree_path) {
            return Some(run_id);
        }
        if self.plan_runs.values().any(|run| {
            run.run.scope_path == worktree_path
                && run.run.archived_unix_ms.is_none()
                && matches!(
                    run.run.status,
                    PlanRunStatus::Queued | PlanRunStatus::Running | PlanRunStatus::Paused
                )
        }) {
            return None;
        }
        self.auto_runs
            .iter()
            .filter(|(_, run)| {
                run.run.worktree_path == worktree_path
                    && run.run.archived_unix_ms.is_none()
                    && run.run.variant == "repair"
            })
            .max_by_key(|(_, run)| run.run.updated_unix_ms)
            .map(|(run_id, _)| run_id)
    }

    pub(super) fn linked_plan_dashboard(
        &self,
        repo: &Repository,
        plan_run_id: &str,
    ) -> Option<view::PlanDashboard> {
        let mut run = self
            .plan_runs
            .get(plan_run_id)
            .or_else(|| self.linked_plan_runs.get(plan_run_id))?
            .clone();
        let run_scope_path = run.run.scope_path.clone();
        run.run.selected_step = self.resolved_plan_step_selection(&run);
        let output_lines = self.plan_output_snapshot(repo, &run.run.id, run.run.selected_step);
        let mut output_state = self
            .plan_output_state_by_run
            .get(&run.run.id)
            .cloned()
            .unwrap_or_else(|| view::PlanOutputViewerState {
                cursor: output_lines.len().saturating_sub(1),
                follow: true,
                expanded_blocks: BTreeSet::new(),
            });
        if output_state.follow {
            output_state.cursor = output_lines.len().saturating_sub(1);
        } else if !output_lines.is_empty() {
            output_state.cursor = output_state
                .cursor
                .min(output_lines.len().saturating_sub(1));
        }
        Some(view::PlanDashboard {
            run,
            runs: self.plan_run_summaries_for_scope(&repo.root, &run_scope_path, Some(plan_run_id)),
            output_lines,
            output_state,
        })
    }

    pub(super) fn plan_output_snapshot(
        &self,
        _repo: &Repository,
        run_id: &str,
        step: usize,
    ) -> Vec<PlanOutputLine> {
        let key = (run_id.to_string(), step);
        self.plan_output_cache
            .borrow()
            .get(&key)
            .cloned()
            .unwrap_or_default()
    }

    pub(super) fn auto_output_snapshot(
        &self,
        repo: &Repository,
        step_run_id: i64,
    ) -> Vec<AutoOutputLine> {
        let Some(repository) = self.managed_repository_identity(repo) else {
            return Vec::new();
        };
        let key = (repository.clone(), step_run_id);
        self.auto_output_cache
            .borrow()
            .get(&key)
            .cloned()
            .unwrap_or_default()
    }

    fn managed_repository_identity(
        &self,
        repository: &Repository,
    ) -> Option<&WorktreeRepositoryKey> {
        self.repos
            .iter()
            .find(|managed| managed.repo.root == repository.root)
            .map(|managed| &managed.identity)
    }

    pub(super) fn resolved_plan_step_selection(&self, run: &PersistedPlanRun) -> usize {
        if self.manual_plan_step_selection_by_run.contains(&run.run.id) {
            return self
                .selected_plan_step_by_run
                .get(&run.run.id)
                .copied()
                .filter(|selected| run.steps.iter().any(|step| step.step == *selected))
                .unwrap_or_else(|| preferred_plan_step(run));
        }
        preferred_plan_step(run)
    }

    pub(super) fn selected_auto_scope(&self) -> Option<(Repository, PathBuf)> {
        match self.focused_panel {
            PanelFocus::Worktrees => {
                let context = self.selected_worktree_context()?;
                Some((
                    context.repo,
                    self.sessions.get(context.session_index)?.path.clone(),
                ))
            }
            PanelFocus::Status => {
                let run_id = self.selected_status_auto_run_id()?;
                let run = self.auto_runs.get(run_id)?;
                Some((
                    Repository {
                        root: PathBuf::from(&run.run.repo_root),
                    },
                    run.run.worktree_path.clone(),
                ))
            }
            PanelFocus::Repos => None,
        }
    }

    pub(super) fn selected_status_auto_run_id(&self) -> Option<&str> {
        if let Some(run_id) = self.selected_auto_run.as_deref()
            && self.auto_runs.contains_key(run_id)
            && self
                .active_auto_runs
                .values()
                .any(|active| active == run_id)
        {
            return Some(run_id);
        }

        self.active_auto_runs
            .values()
            .filter_map(|run_id| {
                self.auto_runs
                    .get(run_id)
                    .map(|run| (run_id.as_str(), run.run.updated_unix_ms))
            })
            .max_by_key(|(_, updated_unix_ms)| *updated_unix_ms)
            .map(|(run_id, _)| run_id)
    }

    pub(super) fn selected_plan_scope(&self) -> Option<(Repository, PathBuf)> {
        match self.focused_panel {
            PanelFocus::Worktrees => {
                let context = self.selected_worktree_context()?;
                Some((
                    context.repo,
                    self.sessions.get(context.session_index)?.path.clone(),
                ))
            }
            PanelFocus::Status | PanelFocus::Repos => None,
        }
    }

    pub(super) fn move_plan_step_selection(&mut self, direction: isize) -> bool {
        let Some(dashboard) = self.current_plan_dashboard() else {
            return false;
        };
        let run_id = dashboard.run.run.id.clone();
        let steps = dashboard
            .run
            .steps
            .iter()
            .map(|step| step.step)
            .collect::<Vec<_>>();
        let current_step = self
            .selected_plan_step_by_run
            .get(&run_id)
            .copied()
            .unwrap_or(dashboard.run.run.selected_step);
        let current = steps
            .iter()
            .position(|step| *step == current_step)
            .unwrap_or(0);
        self.manual_plan_step_selection_by_run
            .insert(run_id.clone());
        let next = current as isize + direction;
        if next < 0 {
            return true;
        }
        if let Some(step) = steps.get(next as usize).copied() {
            self.selected_plan_step_by_run.insert(run_id, step);
        }
        true
    }

    pub(super) fn move_auto_step_selection(&mut self, direction: isize) -> bool {
        let Some(dashboard) = self.current_auto_dashboard() else {
            return false;
        };
        if dashboard.run.run.variant != "repair" {
            return false;
        }
        let run_id = dashboard.run.run.id.clone();
        let steps = dashboard
            .run
            .steps
            .iter()
            .filter_map(|step| step.id)
            .collect::<Vec<_>>();
        if steps.is_empty() {
            return false;
        }
        let current_step = self
            .selected_auto_step_by_run
            .get(&run_id)
            .copied()
            .or(dashboard.run.run.selected_step_run_id)
            .unwrap_or(steps[0]);
        let current = steps
            .iter()
            .position(|step| *step == current_step)
            .unwrap_or(0);
        let next = current as isize + direction;
        let Some(step) = usize::try_from(next)
            .ok()
            .and_then(|next| steps.get(next))
            .copied()
        else {
            return false;
        };
        self.selected_auto_step_by_run.insert(run_id, step);
        true
    }

    pub(super) fn workflow_snapshot(
        &self,
        repository: &Path,
        kind: crate::execution::WorkflowKind,
        run_id: &str,
    ) -> Option<&WorkflowSnapshot> {
        self.workspace_repositories
            .iter()
            .find(|(identity, _)| identity.root == repository)?
            .1
            .workflows
            .iter()
            .find(|workflow| workflow.identity.kind == kind && workflow.identity.run_id == run_id)
    }

    pub(super) fn worktree_workflow_snapshot(
        &self,
        repository: &Path,
        path: &Path,
        kind: crate::execution::WorkflowKind,
    ) -> Option<&WorkflowSnapshot> {
        self.workspace_repositories
            .iter()
            .find(|(identity, _)| identity.root == repository)?
            .1
            .workflows
            .iter()
            .filter(|workflow| workflow.identity.kind == kind && workflow.worktree.path == path)
            .min_by_key(|workflow| {
                let historical = workflow.lifecycle.terminal()
                    && workflow.dispatch.state
                        != Some(crate::execution::DispatchState::RecoveryPending);
                (historical, std::cmp::Reverse(workflow.updated_unix_ms))
            })
    }

    pub(crate) fn workflow_controls(
        &self,
        repository: &Path,
        kind: crate::execution::WorkflowKind,
        run_id: &str,
    ) -> Option<&crate::workspace_state::AvailableControls> {
        self.workflow_snapshot(repository, kind, run_id)
            .map(|workflow| &workflow.available_controls)
    }
}
