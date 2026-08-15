//! Durable repeated-DAG Workflow scheduler.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::agent_phase::{
    AgentCancellation, AgentExecutionError, AgentExecutor, AgentRequest, prompt_with_context,
};
use super::source::{CompiledWorkflow, CompiledWorkflowStep};
use super::step_trigger::{
    AgentOutcome, PostStepResult, PreparedState, StepTrigger, TriggerContext, TriggerDecision,
    TriggerError, TriggerRegistry, TriggerSubject,
};

static SCHEDULER_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const PHASE_LEASE_MS: i64 = 30_000;

async fn wait_for_cancellation(cancellation: &AgentCancellation) {
    while !cancellation.is_cancelled() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}
const MAX_RUN_EVENTS: usize = 4_096;
#[cfg(not(test))]
const PHASE_LEASE_RENEW_MS: u64 = 10_000;
#[cfg(test)]
const PHASE_LEASE_RENEW_MS: u64 = 10;

pub type StoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, WorkflowKernelError>> + Send + 'a>>;

pub trait WorkflowRunStore: Send + Sync + 'static {
    fn retain_workflow<'a>(&'a self, workflow: &'a CompiledWorkflow) -> StoreFuture<'a, ()>;
    fn load_workflow<'a>(&'a self, digest: &'a str) -> StoreFuture<'a, CompiledWorkflow>;
    fn create_run<'a>(&'a self, run: &'a WorkflowRunState) -> StoreFuture<'a, ()>;
    fn load_run<'a>(&'a self, run_id: &'a str) -> StoreFuture<'a, Option<WorkflowRunState>>;
    /// Optimistically persist `run.revision` and increment it on success.
    fn save_run<'a>(&'a self, run: &'a mut WorkflowRunState) -> StoreFuture<'a, ()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Queued,
    Running,
    Waiting,
    NeedsInput,
    Paused,
    Succeeded,
    Failed,
    Cancelled,
    RecoveryRequired,
}

impl WorkflowRunStatus {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepPhase {
    Pending,
    Checking,
    Preparing,
    Prepared,
    RunningAgent,
    AgentSucceeded,
    Finalizing,
    Waiting,
    Satisfied,
    Completed,
    Failed,
    Cancelled,
    RecoveryRequired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Active,
    Succeeded,
    Failed,
    Cancelled,
    RecoveryRequired,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkflowAttemptState {
    pub id: String,
    pub number: u32,
    pub status: AttemptStatus,
    pub phase: StepPhase,
    pub prepared_state: Option<PreparedState>,
    /// Completed authored turns in this attempt, in submission order.
    #[serde(default)]
    pub agent_turns: Vec<AgentOutcome>,
    /// A persisted turn start without a matching outcome is deliberately uncertain.
    #[serde(default)]
    pub agent_turn_in_flight: Option<u32>,
    pub agent_outcome: Option<AgentOutcome>,
    pub error: Option<String>,
    pub started_unix_ms: i64,
    pub finished_unix_ms: Option<i64>,
    pub fencing_token: u64,
    #[serde(default)]
    pub phase_owner: Option<String>,
    #[serde(default)]
    pub lease_expires_unix_ms: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkflowStepState {
    pub key: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub explicit_dependencies: bool,
    pub phase: StepPhase,
    pub summary: Option<String>,
    pub wake_at_unix_ms: Option<i64>,
    pub satisfied_cycle: Option<u64>,
    pub unconditional_completed: bool,
    pub attempts: Vec<WorkflowAttemptState>,
}

impl WorkflowStepState {
    fn active_attempt_index(&self) -> Option<usize> {
        self.attempts
            .iter()
            .rposition(|attempt| attempt.status == AttemptStatus::Active)
    }

    pub fn final_text(&self) -> Option<&str> {
        self.attempts.iter().rev().find_map(|attempt| {
            attempt
                .agent_outcome
                .as_ref()
                .map(|outcome| outcome.final_text.as_str())
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkflowEvent {
    pub sequence: u64,
    pub time_unix_ms: i64,
    pub step_key: Option<String>,
    pub attempt_id: Option<String>,
    pub kind: String,
    pub summary: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRunState {
    pub id: String,
    pub workflow_digest: String,
    pub workflow_name: String,
    pub subject: TriggerSubject,
    pub status: WorkflowRunStatus,
    pub cycle: u64,
    #[serde(default)]
    pub cycle_started_unix_ms: i64,
    pub max_agent_runs: u32,
    pub agent_runs_consumed: u32,
    pub cancellation_requested: bool,
    pub created_unix_ms: i64,
    pub updated_unix_ms: i64,
    pub revision: u64,
    pub steps: Vec<WorkflowStepState>,
    pub events: Vec<WorkflowEvent>,
}

impl WorkflowRunState {
    pub fn step(&self, key: &str) -> Option<&WorkflowStepState> {
        self.steps.iter().find(|step| step.key == key)
    }

    pub fn next_wake_at(&self) -> Option<i64> {
        self.steps
            .iter()
            .filter_map(|step| step.wake_at_unix_ms)
            .min()
    }
}

pub struct StartPromptWorkflow<'a> {
    pub run_id: &'a str,
    pub workflow: &'a CompiledWorkflow,
    pub subject: TriggerSubject,
    pub now_unix_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerProgress {
    Advanced,
    Waiting,
    NeedsInput,
    Paused,
    Succeeded,
    Failed,
    Cancelled,
    RecoveryRequired,
}

#[derive(Clone)]
pub struct WorkflowScheduler {
    store: Arc<dyn WorkflowRunStore>,
    triggers: TriggerRegistry,
    agents: Arc<dyn AgentExecutor>,
    run_locks: Arc<tokio::sync::Mutex<BTreeMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>>,
    worktree_locks:
        Arc<tokio::sync::Mutex<BTreeMap<PathBuf, std::sync::Weak<tokio::sync::Mutex<()>>>>>,
    run_cancellations: Arc<tokio::sync::Mutex<BTreeMap<String, AgentCancellation>>>,
    worker_id: String,
}

impl WorkflowScheduler {
    pub fn new(
        store: Arc<dyn WorkflowRunStore>,
        triggers: TriggerRegistry,
        agents: Arc<dyn AgentExecutor>,
    ) -> Self {
        Self {
            store,
            triggers,
            agents,
            run_locks: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
            worktree_locks: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
            run_cancellations: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
            worker_id: format!(
                "worker-{}-{}",
                std::process::id(),
                SCHEDULER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ),
        }
    }

    pub async fn start(
        &self,
        command: StartPromptWorkflow<'_>,
    ) -> Result<WorkflowRunState, WorkflowKernelError> {
        if command.run_id.trim().is_empty() {
            return Err(WorkflowKernelError::Invalid(
                "run id must not be empty".into(),
            ));
        }
        if command
            .workflow
            .inputs
            .keys()
            .ne(command.workflow.input_values.keys())
        {
            return Err(WorkflowKernelError::Invalid(
                "Workflow launch inputs were not fully bound".into(),
            ));
        }
        self.store.retain_workflow(command.workflow).await?;
        let mut run = WorkflowRunState {
            id: command.run_id.to_string(),
            workflow_digest: command.workflow.digest.clone(),
            workflow_name: command.workflow.name.clone(),
            subject: command.subject,
            status: WorkflowRunStatus::Queued,
            cycle: 1,
            cycle_started_unix_ms: command.now_unix_ms,
            max_agent_runs: command.workflow.max_agent_runs,
            agent_runs_consumed: 0,
            cancellation_requested: false,
            created_unix_ms: command.now_unix_ms,
            updated_unix_ms: command.now_unix_ms,
            revision: 0,
            steps: command
                .workflow
                .steps
                .iter()
                .map(|step| WorkflowStepState {
                    key: step.key.clone(),
                    dependencies: step.dependencies.clone(),
                    explicit_dependencies: step.explicit_dependencies,
                    phase: StepPhase::Pending,
                    summary: None,
                    wake_at_unix_ms: None,
                    satisfied_cycle: None,
                    unconditional_completed: false,
                    attempts: Vec::new(),
                })
                .collect(),
            events: Vec::new(),
        };
        push_event(
            &mut run,
            command.now_unix_ms,
            None,
            None,
            "run_created",
            "queued",
        );
        self.store.create_run(&run).await?;
        Ok(run)
    }

    pub async fn tick(
        &self,
        run_id: &str,
        now_unix_ms: i64,
    ) -> Result<SchedulerProgress, WorkflowKernelError> {
        let cancellation = self.run_cancellation(run_id).await;
        let run_lock = self.named_run_lock(run_id).await;
        if cancellation.is_cancelled() {
            return Err(WorkflowKernelError::PhaseCancelled);
        }
        let _claim = tokio::select! {
            biased;
            _ = wait_for_cancellation(&cancellation) => {
                return Err(WorkflowKernelError::PhaseCancelled);
            }
            claim = run_lock.lock() => claim,
        };
        let mut run = self
            .store
            .load_run(run_id)
            .await?
            .ok_or_else(|| WorkflowKernelError::UnknownRun(run_id.into()))?;
        if run.status.terminal() {
            self.run_cancellations.lock().await.remove(run_id);
            return Ok(progress_for(run.status));
        }
        if run.status == WorkflowRunStatus::Paused {
            return Ok(SchedulerProgress::Paused);
        }
        if run.status == WorkflowRunStatus::NeedsInput {
            self.run_cancellations.lock().await.remove(run_id);
            return Ok(SchedulerProgress::NeedsInput);
        }
        if run.status == WorkflowRunStatus::RecoveryRequired {
            self.run_cancellations.lock().await.remove(run_id);
            return Ok(SchedulerProgress::RecoveryRequired);
        }
        if run.cancellation_requested {
            cancel_run(&mut run, now_unix_ms);
            self.store.save_run(&mut run).await?;
            self.run_cancellations.lock().await.remove(run_id);
            return Ok(SchedulerProgress::Cancelled);
        }
        if run.status == WorkflowRunStatus::Waiting
            && run.next_wake_at().is_some_and(|wake| wake > now_unix_ms)
        {
            return Ok(SchedulerProgress::Waiting);
        }

        let workflow = self.store.load_workflow(&run.workflow_digest).await?;
        run.status = WorkflowRunStatus::Running;
        run.updated_unix_ms = now_unix_ms;
        // A wake always starts from the graph roots; stale Wait/Satisfied observations are
        // re-evaluated while unconditional completions remain durable.
        if run
            .steps
            .iter()
            .any(|step| step.wake_at_unix_ms.is_some_and(|wake| wake <= now_unix_ms))
        {
            invalidate_trigger_observations(&workflow, &mut run);
            push_event(
                &mut run,
                now_unix_ms,
                None,
                None,
                "cycle_woke",
                "re-evaluating from roots",
            );
        }
        self.store.save_run(&mut run).await?;

        // A concurrent wave can leave several durable active Attempts after a crash. Reconcile
        // every such effect boundary before considering any pending or retried Step.
        if let Some((step_index, attempt_index)) =
            run.steps.iter().enumerate().find_map(|(step_index, step)| {
                let ready = step.phase != StepPhase::Waiting
                    || step.wake_at_unix_ms.is_some_and(|wake| wake <= now_unix_ms);
                ready
                    .then(|| step.active_attempt_index())
                    .flatten()
                    .map(|attempt_index| (step_index, attempt_index))
            })
        {
            return self
                .resume_attempt(&workflow, &mut run, step_index, attempt_index, now_unix_ms)
                .await;
        }

        if let Some(progress) = self
            .run_unconditional_wave(&workflow, &mut run, now_unix_ms)
            .await?
        {
            return Ok(progress);
        }

        for key in &workflow.topological_order {
            let step_index = workflow
                .steps
                .iter()
                .position(|step| &step.key == key)
                .ok_or_else(|| {
                    WorkflowKernelError::Invalid(format!("unknown compiled step {key}"))
                })?;
            if !dependencies_satisfied(&workflow, &run, step_index) {
                continue;
            }
            if run.steps[step_index].unconditional_completed
                || run.steps[step_index].satisfied_cycle == Some(run.cycle)
            {
                continue;
            }
            if run.steps[step_index].phase == StepPhase::Waiting
                && run.steps[step_index]
                    .wake_at_unix_ms
                    .is_some_and(|wake| wake > now_unix_ms)
            {
                continue;
            }
            if let Some(attempt_index) = run.steps[step_index].active_attempt_index() {
                let progress = self
                    .resume_attempt(&workflow, &mut run, step_index, attempt_index, now_unix_ms)
                    .await?;
                return Ok(progress);
            }

            let compiled_step = &workflow.steps[step_index];
            if let Some(trigger_revision) = &compiled_step.trigger {
                let trigger =
                    resolve_trigger(&self.triggers, trigger_revision).ok_or_else(|| {
                        WorkflowKernelError::MissingTrigger(trigger_revision.name.clone())
                    })?;
                run.steps[step_index].phase = StepPhase::Checking;
                run.steps[step_index].wake_at_unix_ms = None;
                self.store.save_run(&mut run).await?;
                let context = trigger_context(&run, compiled_step, None);
                let cancellation = self.run_cancellation(&run.id).await;
                let check = tokio::select! {
                    decision = trigger.should_run_step(&context) => decision,
                    _ = wait_for_cancellation(&cancellation) => {
                        return Err(WorkflowKernelError::PhaseCancelled);
                    }
                };
                let decision = match check {
                    Ok(decision) => decision,
                    Err(error) => {
                        fail_step(
                            &mut run,
                            step_index,
                            now_unix_ms,
                            &format!("Trigger check failed: {error}"),
                        );
                        self.store.save_run(&mut run).await?;
                        return Ok(SchedulerProgress::Failed);
                    }
                };
                run.steps[step_index].summary = Some(bounded(decision.summary()));
                match decision {
                    TriggerDecision::Satisfied { .. } => {
                        run.steps[step_index].phase = StepPhase::Satisfied;
                        run.steps[step_index].satisfied_cycle = Some(run.cycle);
                        push_event(
                            &mut run,
                            now_unix_ms,
                            Some(key),
                            None,
                            "trigger_satisfied",
                            "satisfied",
                        );
                        self.store.save_run(&mut run).await?;
                    }
                    TriggerDecision::Wait {
                        wake_at_unix_ms, ..
                    } => {
                        run.steps[step_index].phase = StepPhase::Waiting;
                        run.steps[step_index].wake_at_unix_ms = Some(wake_at_unix_ms);
                        push_event(
                            &mut run,
                            now_unix_ms,
                            Some(key),
                            None,
                            "trigger_waiting",
                            "waiting without an Agent slot",
                        );
                        self.store.save_run(&mut run).await?;
                        // Do not return: independent branches remain eligible.
                    }
                    TriggerDecision::Fail { reason } => {
                        fail_step(&mut run, step_index, now_unix_ms, &reason);
                        self.store.save_run(&mut run).await?;
                        return Ok(SchedulerProgress::Failed);
                    }
                    TriggerDecision::Run { .. } => {
                        if compiled_step.prompt.is_none() {
                            fail_step(
                                &mut run,
                                step_index,
                                now_unix_ms,
                                "check-only Step Trigger returned Run without a prompt",
                            );
                            self.store.save_run(&mut run).await?;
                            return Ok(SchedulerProgress::Failed);
                        }
                        let attempt_index = begin_attempt(&mut run, step_index, now_unix_ms)?;
                        self.store.save_run(&mut run).await?;
                        let progress = self
                            .prepare_attempt(
                                &workflow,
                                &mut run,
                                step_index,
                                attempt_index,
                                trigger,
                                now_unix_ms,
                            )
                            .await?;
                        return Ok(progress);
                    }
                }
            } else {
                let attempt_index = begin_attempt(&mut run, step_index, now_unix_ms)?;
                run.steps[step_index].attempts[attempt_index].phase = StepPhase::Prepared;
                run.steps[step_index].attempts[attempt_index].prepared_state =
                    Some(PreparedState::default());
                run.steps[step_index].phase = StepPhase::Prepared;
                self.store.save_run(&mut run).await?;
                let progress = self
                    .run_agent(&workflow, &mut run, step_index, attempt_index, now_unix_ms)
                    .await?;
                return Ok(progress);
            }
        }

        if graph_satisfied(&workflow, &run) {
            run.status = WorkflowRunStatus::Succeeded;
            run.updated_unix_ms = now_unix_ms;
            push_event(
                &mut run,
                now_unix_ms,
                None,
                None,
                "run_succeeded",
                "one full graph cycle is satisfied",
            );
            self.store.save_run(&mut run).await?;
            self.run_cancellations.lock().await.remove(run_id);
            Ok(SchedulerProgress::Succeeded)
        } else {
            run.status = WorkflowRunStatus::Waiting;
            run.updated_unix_ms = now_unix_ms;
            self.store.save_run(&mut run).await?;
            Ok(SchedulerProgress::Waiting)
        }
    }

    async fn run_unconditional_wave(
        &self,
        workflow: &CompiledWorkflow,
        run: &mut WorkflowRunState,
        now_unix_ms: i64,
    ) -> Result<Option<SchedulerProgress>, WorkflowKernelError> {
        // Recovery remains deliberately single-lifecycle: never start new work beside an
        // Attempt whose last durable phase needs to be resumed or reconciled.
        if run
            .steps
            .iter()
            .any(|step| step.active_attempt_index().is_some())
        {
            return Ok(None);
        }

        let mut step_indices = Vec::new();
        for key in &workflow.topological_order {
            let step_index = workflow
                .steps
                .iter()
                .position(|step| &step.key == key)
                .ok_or_else(|| {
                    WorkflowKernelError::Invalid(format!("unknown compiled step {key}"))
                })?;
            let state = &run.steps[step_index];
            if state.unconditional_completed
                || state.satisfied_cycle == Some(run.cycle)
                || !dependencies_satisfied(workflow, run, step_index)
                || (state.phase == StepPhase::Waiting
                    && state.wake_at_unix_ms.is_some_and(|wake| wake > now_unix_ms))
            {
                continue;
            }
            // Trigger decisions and hooks keep their existing serialized path, but an independent
            // Trigger must not partition unconditional Agents that are eligible for the same wave.
            if workflow.steps[step_index].trigger.is_none() {
                step_indices.push(step_index);
            }
        }

        let remaining_budget = run.max_agent_runs.saturating_sub(run.agent_runs_consumed) as usize;
        step_indices.truncate(remaining_budget);
        if step_indices.len() < 2 {
            return Ok(None);
        }

        // Capture every prompt and selected predecessor message before any branch begins. This
        // makes context for the wave immutable even though Agents intentionally share a worktree.
        let mut branches = Vec::with_capacity(step_indices.len());
        for &step_index in &step_indices {
            let step = &workflow.steps[step_index];
            let authored = step.prompt.as_deref().ok_or_else(|| {
                WorkflowKernelError::Invalid(format!("Step {} has no Agent prompt", step.key))
            })?;
            let contexts = selected_context(workflow, run, step)?;
            let mut prompts = Vec::with_capacity(step.followups.len() + 1);
            prompts.push(prompt_with_context(authored, &contexts));
            prompts.extend(step.followups.iter().cloned());
            branches.push(UnconditionalWaveBranch {
                step_index,
                attempt_index: 0,
                step_key: step.key.clone(),
                prompts,
                harness: step.agent.harness.clone(),
                model: step.agent.model.clone(),
                variant: step.agent.variant.clone(),
                run_id: run.id.clone(),
                repository: run.subject.repository.clone(),
                worktree: run.subject.worktree.clone(),
            });
        }

        // Persist the prepared boundary for the complete wave before claiming any Agent phase.
        for branch in &mut branches {
            let attempt_index = begin_attempt(run, branch.step_index, now_unix_ms)?;
            branch.attempt_index = attempt_index;
            let attempt = &mut run.steps[branch.step_index].attempts[attempt_index];
            attempt.phase = StepPhase::Prepared;
            attempt.prepared_state = Some(PreparedState::default());
            run.steps[branch.step_index].phase = StepPhase::Prepared;
        }
        self.store.save_run(run).await?;

        // Budget units and in-flight markers are one durable write ahead of all effects. A crash
        // after this point therefore requires reconciliation rather than replaying an Agent.
        for branch in &branches {
            run.agent_runs_consumed += 1;
            let attempt = &mut run.steps[branch.step_index].attempts[branch.attempt_index];
            attempt.phase = StepPhase::RunningAgent;
            attempt.agent_turn_in_flight = Some(1);
            run.steps[branch.step_index].phase = StepPhase::RunningAgent;
            claim_attempt_phase(
                run,
                branch.step_index,
                branch.attempt_index,
                &self.worker_id,
                now_unix_ms,
            );
            push_attempt_event(
                run,
                branch.step_index,
                branch.attempt_index,
                now_unix_ms,
                "agent_started",
                "fresh Agent Session in concurrent wave",
            );
            push_attempt_event(
                run,
                branch.step_index,
                branch.attempt_index,
                now_unix_ms,
                "agent_turn_started",
                &format!("turn 1/{}", branch.prompts.len()),
            );
        }
        self.store.save_run(run).await?;

        // Hold one worktree claim for the wave. Branches inside this claim intentionally overlap,
        // while a different Workflow Run cannot mutate the same worktree at the same time.
        let cancellation = self.run_cancellation(&run.id).await;
        let worktree_lock = self.worktree_lock(&run.subject.worktree).await;
        let _worktree_claim = tokio::select! {
            claim = worktree_lock.lock_owned() => claim,
            _ = wait_for_cancellation(&cancellation) => {
                return Err(WorkflowKernelError::PhaseCancelled);
            }
        };
        let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut tasks = tokio::task::JoinSet::new();
        for branch in branches {
            let agents = self.agents.clone();
            let cancellation = cancellation.clone();
            let turn_tx = turn_tx.clone();
            tasks.spawn(async move {
                execute_unconditional_branch(agents, branch, cancellation, turn_tx).await
            });
        }
        drop(turn_tx);

        let mut failed = false;
        let mut cancelled = false;
        let mut wave_completed_unix_ms = now_unix_ms;
        let mut remaining = step_indices.len();
        let started = tokio::time::Instant::now();
        let mut renewal =
            tokio::time::interval(std::time::Duration::from_millis(PHASE_LEASE_RENEW_MS));
        renewal.tick().await;
        while remaining > 0 {
            tokio::select! {
                Some(turn) = turn_rx.recv() => {
                    let completed_unix_ms = phase_time(now_unix_ms, turn.elapsed);
                    let attempt = &mut run.steps[turn.step_index].attempts[turn.attempt_index];
                    attempt.agent_turns.push(turn.outcome);
                    attempt.agent_turn_in_flight = (turn.turn_number < turn.total_turns)
                        .then_some(turn.turn_number + 1);
                    push_attempt_event(
                        run,
                        turn.step_index,
                        turn.attempt_index,
                        completed_unix_ms,
                        "agent_turn_succeeded",
                        &format!("turn {}/{}", turn.turn_number, turn.total_turns),
                    );
                    if turn.turn_number < turn.total_turns {
                        push_attempt_event(
                            run,
                            turn.step_index,
                            turn.attempt_index,
                            completed_unix_ms,
                            "agent_turn_started",
                            &format!("turn {}/{}", turn.turn_number + 1, turn.total_turns),
                        );
                    }
                    self.store.save_run(run).await?;
                    let _ = turn.persisted.send(());
                }
                joined = tasks.join_next() => {
                    let Some(joined) = joined else { break };
                    remaining = remaining.saturating_sub(1);
                    let result = joined.map_err(|error| {
                        WorkflowKernelError::Invalid(format!(
                            "concurrent Agent task failed: {error}"
                        ))
                    })?;
                    let completed_unix_ms = phase_time(now_unix_ms, result.elapsed);
                    wave_completed_unix_ms = wave_completed_unix_ms.max(completed_unix_ms);
                    run.steps[result.step_index].attempts[result.attempt_index]
                        .agent_turn_in_flight = None;
                    match result.error {
                        None => {
                            let outcome = run.steps[result.step_index].attempts[result.attempt_index]
                                .agent_turns
                                .last()
                                .cloned()
                                .ok_or_else(|| {
                                    WorkflowKernelError::Invalid(
                                        "concurrent Agent produced no completed turns".into(),
                                    )
                                })?;
                            run.steps[result.step_index].attempts[result.attempt_index].agent_outcome =
                                Some(outcome);
                            run.steps[result.step_index].attempts[result.attempt_index].phase =
                                StepPhase::AgentSucceeded;
                            finish_attempt(
                                run,
                                result.step_index,
                                result.attempt_index,
                                completed_unix_ms,
                            );
                            run.steps[result.step_index].unconditional_completed = true;
                            run.steps[result.step_index].phase = StepPhase::Completed;
                            run.steps[result.step_index].summary = Some("Agent completed".into());
                        }
                        Some(AgentExecutionError::Cancelled) => {
                            cancelled = true;
                            cancel_attempt(
                                run,
                                result.step_index,
                                result.attempt_index,
                                completed_unix_ms,
                                "concurrent Agent cancelled",
                            );
                        }
                        Some(error) => {
                            failed = true;
                            cancellation.cancel();
                            fail_attempt(
                                run,
                                result.step_index,
                                result.attempt_index,
                                completed_unix_ms,
                                error.to_string(),
                            );
                        }
                    }
                    self.store.save_run(run).await?;
                }
                _ = renewal.tick() => {
                    let renewed_unix_ms = phase_time(now_unix_ms, started.elapsed());
                    for &step_index in &step_indices {
                        let Some(attempt_index) = run.steps[step_index].active_attempt_index() else {
                            continue;
                        };
                        let fencing_token = run.steps[step_index].attempts[attempt_index].fencing_token;
                        renew_attempt_phase(
                            run,
                            step_index,
                            attempt_index,
                            &self.worker_id,
                            fencing_token,
                            renewed_unix_ms,
                        )?;
                    }
                    run.updated_unix_ms = renewed_unix_ms;
                    self.store.save_run(run).await?;
                }
            }
        }

        if failed {
            run.status = WorkflowRunStatus::Failed;
            self.store.save_run(run).await?;
            return Ok(Some(SchedulerProgress::Failed));
        }
        if cancelled {
            cancel_run(run, wave_completed_unix_ms);
            self.store.save_run(run).await?;
            return Ok(Some(SchedulerProgress::Cancelled));
        }

        begin_new_cycle(workflow, run, wave_completed_unix_ms);
        self.store.save_run(run).await?;
        Ok(Some(SchedulerProgress::Advanced))
    }

    async fn prepare_attempt(
        &self,
        workflow: &CompiledWorkflow,
        run: &mut WorkflowRunState,
        step_index: usize,
        attempt_index: usize,
        trigger: Arc<dyn StepTrigger>,
        now_unix_ms: i64,
    ) -> Result<SchedulerProgress, WorkflowKernelError> {
        let step = &workflow.steps[step_index];
        claim_attempt_phase(run, step_index, attempt_index, &self.worker_id, now_unix_ms);
        self.store.save_run(run).await?;
        let context = trigger_context(
            run,
            step,
            Some(&run.steps[step_index].attempts[attempt_index].id),
        );
        let cancellation = self.run_cancellation(&run.id).await;
        let worktree_lock = self.worktree_lock(&run.subject.worktree).await;
        let _worktree_claim = tokio::select! {
            claim = worktree_lock.lock_owned() => claim,
            _ = wait_for_cancellation(&cancellation) => {
                return Err(WorkflowKernelError::PhaseCancelled);
            }
        };
        let preparation = self
            .supervise_phase(
                run,
                step_index,
                attempt_index,
                now_unix_ms,
                &cancellation,
                trigger.pre_step_run(&context),
            )
            .await;
        let (prepared, completed_unix_ms) = match preparation {
            Ok(preparation) => preparation,
            Err(WorkflowKernelError::PhaseCancelled)
                if !trigger.recovery_policy().prepare_repeatable =>
            {
                recovery_required(
                    run,
                    step_index,
                    attempt_index,
                    now_unix_ms,
                    "pre-Step hook was cancelled after its durable mutation boundary; authoritative reconciliation is required",
                );
                self.store.save_run(run).await?;
                return Ok(SchedulerProgress::RecoveryRequired);
            }
            Err(error) => return Err(error),
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                fail_attempt(
                    run,
                    step_index,
                    attempt_index,
                    completed_unix_ms,
                    error.to_string(),
                );
                self.store.save_run(run).await?;
                return Ok(SchedulerProgress::Failed);
            }
        };
        drop(_worktree_claim);
        run.steps[step_index].attempts[attempt_index].prepared_state = Some(prepared);
        release_attempt_phase(run, step_index, attempt_index);
        run.steps[step_index].attempts[attempt_index].phase = StepPhase::Prepared;
        run.steps[step_index].phase = StepPhase::Prepared;
        push_attempt_event(
            run,
            step_index,
            attempt_index,
            completed_unix_ms,
            "step_prepared",
            "prepared state persisted",
        );
        self.store.save_run(run).await?;
        self.run_agent(workflow, run, step_index, attempt_index, completed_unix_ms)
            .await
    }

    async fn run_agent(
        &self,
        workflow: &CompiledWorkflow,
        run: &mut WorkflowRunState,
        step_index: usize,
        attempt_index: usize,
        now_unix_ms: i64,
    ) -> Result<SchedulerProgress, WorkflowKernelError> {
        let step = &workflow.steps[step_index];
        let authored = step.prompt.as_deref().ok_or_else(|| {
            WorkflowKernelError::Invalid(format!("Step {} has no Agent prompt", step.key))
        })?;
        let contexts = selected_context(workflow, run, step)?;
        let mut prompts = Vec::with_capacity(step.followups.len() + 1);
        prompts.push(prompt_with_context(authored, &contexts));
        prompts.extend(step.followups.iter().cloned());

        let resuming_turns =
            run.steps[step_index].attempts[attempt_index].phase == StepPhase::RunningAgent;
        if resuming_turns {
            if run.steps[step_index].attempts[attempt_index]
                .agent_turn_in_flight
                .is_some()
            {
                recovery_required(
                    run,
                    step_index,
                    attempt_index,
                    now_unix_ms,
                    "Agent turn outcome requires reconciliation",
                );
                self.store.save_run(run).await?;
                return Ok(SchedulerProgress::RecoveryRequired);
            }
            claim_attempt_phase(run, step_index, attempt_index, &self.worker_id, now_unix_ms);
            push_attempt_event(
                run,
                step_index,
                attempt_index,
                now_unix_ms,
                "agent_session_resumed",
                "resuming persisted Agent follow-ups",
            );
        } else {
            if run.agent_runs_consumed >= run.max_agent_runs {
                run.status = WorkflowRunStatus::NeedsInput;
                run.steps[step_index].summary = Some(format!(
                    "Agent run budget exhausted ({}/{})",
                    run.agent_runs_consumed, run.max_agent_runs
                ));
                push_attempt_event(
                    run,
                    step_index,
                    attempt_index,
                    now_unix_ms,
                    "agent_budget_exhausted",
                    "needs input",
                );
                self.store.save_run(run).await?;
                self.run_cancellations.lock().await.remove(&run.id);
                return Ok(SchedulerProgress::NeedsInput);
            }
            run.agent_runs_consumed += 1;
            run.steps[step_index].attempts[attempt_index].phase = StepPhase::RunningAgent;
            run.steps[step_index].phase = StepPhase::RunningAgent;
            claim_attempt_phase(run, step_index, attempt_index, &self.worker_id, now_unix_ms);
            push_attempt_event(
                run,
                step_index,
                attempt_index,
                now_unix_ms,
                "agent_started",
                "fresh Agent Session",
            );
        }
        self.store.save_run(run).await?;

        let attempt_id = run.steps[step_index].attempts[attempt_index].id.clone();
        let cancellation = self.run_cancellation(&run.id).await;
        let worktree_lock = self.worktree_lock(&run.subject.worktree).await;
        let _worktree_claim = tokio::select! {
            claim = worktree_lock.lock_owned() => claim,
            _ = wait_for_cancellation(&cancellation) => {
                return Err(WorkflowKernelError::PhaseCancelled);
            }
        };
        let mut turn_started_unix_ms = now_unix_ms;

        loop {
            let turn_index = run.steps[step_index].attempts[attempt_index]
                .agent_turns
                .len();
            if turn_index == prompts.len() {
                break;
            }
            let turn_number = u32::try_from(turn_index + 1).map_err(|_| {
                WorkflowKernelError::Invalid("Agent follow-up count exceeds u32".into())
            })?;
            let total_turns = prompts.len();
            let resume_session_id = run.steps[step_index].attempts[attempt_index]
                .agent_turns
                .last()
                .map(|outcome| outcome.session_id.clone());
            run.steps[step_index].attempts[attempt_index].agent_turn_in_flight = Some(turn_number);
            run.steps[step_index].summary = Some(format!(
                "running Agent turn {}/{}",
                turn_index + 1,
                total_turns
            ));
            push_attempt_event(
                run,
                step_index,
                attempt_index,
                turn_started_unix_ms,
                "agent_turn_started",
                &format!("turn {}/{}", turn_index + 1, total_turns),
            );
            self.store.save_run(run).await?;

            let agent_future = self.agents.execute(AgentRequest {
                run_id: run.id.clone(),
                step_key: step.key.clone(),
                attempt_id: attempt_id.clone(),
                repository: run.subject.repository.clone(),
                worktree: run.subject.worktree.clone(),
                harness: step.agent.harness.clone(),
                model: step.agent.model.clone(),
                variant: step.agent.variant.clone(),
                prompt: prompts[turn_index].clone(),
                resume_session_id: resume_session_id.clone(),
                require_resumable_session: total_turns > 1 && turn_index == 0,
                cancellation: cancellation.clone(),
            });
            let execution = self
                .supervise_phase(
                    run,
                    step_index,
                    attempt_index,
                    turn_started_unix_ms,
                    &cancellation,
                    agent_future,
                )
                .await;
            let (outcome, completed_unix_ms) = match execution {
                Ok(execution) => execution,
                Err(error) => {
                    return Err(error);
                }
            };
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(AgentExecutionError::Cancelled) => {
                    run.steps[step_index].attempts[attempt_index].agent_turn_in_flight = None;
                    cancel_run(run, completed_unix_ms);
                    self.store.save_run(run).await?;
                    return Ok(SchedulerProgress::Cancelled);
                }
                Err(error) => {
                    run.steps[step_index].attempts[attempt_index].agent_turn_in_flight = None;
                    fail_attempt(
                        run,
                        step_index,
                        attempt_index,
                        completed_unix_ms,
                        error.to_string(),
                    );
                    self.store.save_run(run).await?;
                    return Ok(SchedulerProgress::Failed);
                }
            };
            if let Some(expected) = resume_session_id.as_deref()
                && outcome.session_id != expected
            {
                run.steps[step_index].attempts[attempt_index].agent_turn_in_flight = None;
                fail_attempt(
                    run,
                    step_index,
                    attempt_index,
                    completed_unix_ms,
                    format!(
                        "Agent follow-up resumed session {expected}, but reported {}",
                        outcome.session_id
                    ),
                );
                self.store.save_run(run).await?;
                return Ok(SchedulerProgress::Failed);
            }
            run.steps[step_index].attempts[attempt_index]
                .agent_turns
                .push(outcome);
            run.steps[step_index].attempts[attempt_index].agent_turn_in_flight = None;
            push_attempt_event(
                run,
                step_index,
                attempt_index,
                completed_unix_ms,
                "agent_turn_succeeded",
                &format!("turn {}/{}", turn_index + 1, total_turns),
            );
            self.store.save_run(run).await?;
            turn_started_unix_ms = completed_unix_ms;
        }

        drop(_worktree_claim);
        let outcome = run.steps[step_index].attempts[attempt_index]
            .agent_turns
            .last()
            .cloned()
            .ok_or_else(|| {
                WorkflowKernelError::Invalid("Agent produced no completed turns".into())
            })?;
        run.steps[step_index].attempts[attempt_index].agent_outcome = Some(outcome);
        release_attempt_phase(run, step_index, attempt_index);
        run.steps[step_index].attempts[attempt_index].phase = StepPhase::AgentSucceeded;
        run.steps[step_index].phase = StepPhase::AgentSucceeded;
        push_attempt_event(
            run,
            step_index,
            attempt_index,
            turn_started_unix_ms,
            "agent_succeeded",
            "final Agent turn persisted",
        );
        self.store.save_run(run).await?;
        self.finalize_attempt(
            workflow,
            run,
            step_index,
            attempt_index,
            turn_started_unix_ms,
        )
        .await
    }

    async fn finalize_attempt(
        &self,
        workflow: &CompiledWorkflow,
        run: &mut WorkflowRunState,
        step_index: usize,
        attempt_index: usize,
        now_unix_ms: i64,
    ) -> Result<SchedulerProgress, WorkflowKernelError> {
        let step = &workflow.steps[step_index];
        let (completion, completed_unix_ms) = if let Some(trigger_revision) = &step.trigger {
            let trigger = resolve_trigger(&self.triggers, trigger_revision).ok_or_else(|| {
                WorkflowKernelError::MissingTrigger(trigger_revision.name.clone())
            })?;
            run.steps[step_index].attempts[attempt_index].phase = StepPhase::Finalizing;
            run.steps[step_index].phase = StepPhase::Finalizing;
            claim_attempt_phase(run, step_index, attempt_index, &self.worker_id, now_unix_ms);
            self.store.save_run(run).await?;
            let context = trigger_context(
                run,
                step,
                Some(&run.steps[step_index].attempts[attempt_index].id),
            );
            let prepared = run.steps[step_index].attempts[attempt_index]
                .prepared_state
                .clone()
                .unwrap_or_default();
            let outcome = run.steps[step_index].attempts[attempt_index]
                .agent_outcome
                .clone()
                .ok_or_else(|| WorkflowKernelError::Invalid("missing Agent outcome".into()))?;
            let cancellation = self.run_cancellation(&run.id).await;
            let worktree_lock = self.worktree_lock(&run.subject.worktree).await;
            let _worktree_claim = tokio::select! {
                claim = worktree_lock.lock_owned() => claim,
                _ = wait_for_cancellation(&cancellation) => {
                        return Err(WorkflowKernelError::PhaseCancelled);
                }
            };
            let finalization = self
                .supervise_phase(
                    run,
                    step_index,
                    attempt_index,
                    now_unix_ms,
                    &cancellation,
                    trigger.post_step_run(&context, &prepared, &outcome),
                )
                .await;
            let (completion, completed_unix_ms) = match finalization {
                Ok(finalization) => finalization,
                Err(WorkflowKernelError::PhaseCancelled)
                    if !trigger.recovery_policy().finalize_repeatable =>
                {
                    recovery_required(
                        run,
                        step_index,
                        attempt_index,
                        now_unix_ms,
                        "post-Step hook was cancelled after its durable mutation boundary; authoritative reconciliation is required",
                    );
                    self.store.save_run(run).await?;
                    return Ok(SchedulerProgress::RecoveryRequired);
                }
                Err(error) => return Err(error),
            };
            match completion {
                Ok(completion) => (completion, completed_unix_ms),
                Err(error) => {
                    fail_attempt(
                        run,
                        step_index,
                        attempt_index,
                        completed_unix_ms,
                        format!("post-Step Trigger failed: {error}"),
                    );
                    self.store.save_run(run).await?;
                    return Ok(SchedulerProgress::Failed);
                }
            }
        } else {
            (
                PostStepResult::Success {
                    summary: "Agent completed".into(),
                },
                now_unix_ms,
            )
        };
        match completion {
            PostStepResult::Fail { reason } => {
                fail_attempt(run, step_index, attempt_index, completed_unix_ms, reason);
                self.store.save_run(run).await?;
                Ok(SchedulerProgress::Failed)
            }
            PostStepResult::Wait {
                summary,
                wake_at_unix_ms,
            } => {
                release_attempt_phase(run, step_index, attempt_index);
                run.steps[step_index].attempts[attempt_index].phase = StepPhase::AgentSucceeded;
                run.steps[step_index].phase = StepPhase::Waiting;
                run.steps[step_index].summary = Some(bounded(&summary));
                run.steps[step_index].wake_at_unix_ms = Some(wake_at_unix_ms);
                // Keep the attempt active with its prepared state and Agent outcome. One more
                // scheduler pass may advance independent branches; the hook itself resumes only
                // after its durable wake.
                run.status = WorkflowRunStatus::Running;
                self.store.save_run(run).await?;
                Ok(SchedulerProgress::Advanced)
            }
            PostStepResult::Success { summary } => {
                finish_attempt(run, step_index, attempt_index, completed_unix_ms);
                run.steps[step_index].summary = Some(bounded(&summary));
                if step.trigger.is_none() {
                    run.steps[step_index].unconditional_completed = true;
                    run.steps[step_index].phase = StepPhase::Completed;
                }
                begin_new_cycle(workflow, run, completed_unix_ms);
                self.store.save_run(run).await?;
                Ok(SchedulerProgress::Advanced)
            }
        }
    }

    async fn supervise_phase<F, T>(
        &self,
        run: &mut WorkflowRunState,
        step_index: usize,
        attempt_index: usize,
        started_unix_ms: i64,
        cancellation: &AgentCancellation,
        future: F,
    ) -> Result<(T, i64), WorkflowKernelError>
    where
        F: Future<Output = T>,
    {
        let fencing_token = run.steps[step_index].attempts[attempt_index].fencing_token;
        let started = tokio::time::Instant::now();
        let mut next_renewal_ms = PHASE_LEASE_RENEW_MS;
        tokio::pin!(future);
        loop {
            tokio::select! {
                output = &mut future => {
                    let completed_unix_ms = phase_time(started_unix_ms, started.elapsed());
                    validate_attempt_phase(
                        run,
                        step_index,
                        attempt_index,
                        &self.worker_id,
                        fencing_token,
                        completed_unix_ms,
                    )?;
                    return Ok((output, completed_unix_ms));
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                    if cancellation.is_cancelled() {
                        return Err(WorkflowKernelError::PhaseCancelled);
                    }
                    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    if elapsed_ms >= next_renewal_ms {
                        let renewed_unix_ms = phase_time(started_unix_ms, started.elapsed());
                        renew_attempt_phase(
                            run,
                            step_index,
                            attempt_index,
                            &self.worker_id,
                            fencing_token,
                            renewed_unix_ms,
                        )?;
                        run.updated_unix_ms = renewed_unix_ms;
                        self.store.save_run(run).await?;
                        next_renewal_ms = elapsed_ms.saturating_add(PHASE_LEASE_RENEW_MS);
                    }
                }
            }
        }
    }

    async fn resume_attempt(
        &self,
        workflow: &CompiledWorkflow,
        run: &mut WorkflowRunState,
        step_index: usize,
        attempt_index: usize,
        now_unix_ms: i64,
    ) -> Result<SchedulerProgress, WorkflowKernelError> {
        let phase = run.steps[step_index].attempts[attempt_index].phase;
        let step = &workflow.steps[step_index];
        match phase {
            StepPhase::Prepared => {
                self.run_agent(workflow, run, step_index, attempt_index, now_unix_ms)
                    .await
            }
            StepPhase::AgentSucceeded => {
                self.finalize_attempt(workflow, run, step_index, attempt_index, now_unix_ms)
                    .await
            }
            StepPhase::Preparing => {
                let trigger = step
                    .trigger
                    .as_ref()
                    .and_then(|revision| resolve_trigger(&self.triggers, revision))
                    .ok_or_else(|| WorkflowKernelError::MissingTrigger(step.key.clone()))?;
                if trigger.recovery_policy().prepare_repeatable {
                    self.prepare_attempt(
                        workflow,
                        run,
                        step_index,
                        attempt_index,
                        trigger,
                        now_unix_ms,
                    )
                    .await
                } else {
                    recovery_required(
                        run,
                        step_index,
                        attempt_index,
                        now_unix_ms,
                        "pre-Step hook outcome is uncertain",
                    );
                    self.store.save_run(run).await?;
                    Ok(SchedulerProgress::RecoveryRequired)
                }
            }
            StepPhase::Finalizing => {
                let trigger = step
                    .trigger
                    .as_ref()
                    .and_then(|revision| resolve_trigger(&self.triggers, revision))
                    .ok_or_else(|| WorkflowKernelError::MissingTrigger(step.key.clone()))?;
                if trigger.recovery_policy().finalize_repeatable {
                    // Persisted Agent outcome and prepared state make repeatable finalization safe.
                    run.steps[step_index].attempts[attempt_index].phase = StepPhase::AgentSucceeded;
                    self.finalize_attempt(workflow, run, step_index, attempt_index, now_unix_ms)
                        .await
                } else {
                    recovery_required(
                        run,
                        step_index,
                        attempt_index,
                        now_unix_ms,
                        "post-Step hook outcome is uncertain",
                    );
                    self.store.save_run(run).await?;
                    Ok(SchedulerProgress::RecoveryRequired)
                }
            }
            StepPhase::RunningAgent => {
                self.run_agent(workflow, run, step_index, attempt_index, now_unix_ms)
                    .await
            }
            _ => Err(WorkflowKernelError::Invalid(format!(
                "active Attempt {} has non-resumable phase {phase:?}",
                run.steps[step_index].attempts[attempt_index].id
            ))),
        }
    }

    /// Wake a durable Wait from an external subscription (for example a completed coalesced
    /// provider observation). The next tick invalidates transient Trigger decisions from roots.
    pub async fn wake(&self, run_id: &str, now: i64) -> Result<(), WorkflowKernelError> {
        let run_lock = self.named_run_lock(run_id).await;
        let _claim = run_lock.lock().await;
        let mut run = self
            .store
            .load_run(run_id)
            .await?
            .ok_or_else(|| WorkflowKernelError::UnknownRun(run_id.into()))?;
        if run.status != WorkflowRunStatus::Waiting {
            return Ok(());
        }
        for step in &mut run.steps {
            if step.phase == StepPhase::Waiting {
                step.wake_at_unix_ms = Some(now);
            }
        }
        run.status = WorkflowRunStatus::Queued;
        run.updated_unix_ms = now;
        push_event(
            &mut run,
            now,
            None,
            None,
            "external_observation_woke",
            "fresh provider observation available",
        );
        self.store.save_run(&mut run).await
    }

    pub async fn pause(&self, run_id: &str, now: i64) -> Result<(), WorkflowKernelError> {
        self.control(run_id, now, WorkflowRunStatus::Paused, "run_paused")
            .await
    }

    pub async fn resume(&self, run_id: &str, now: i64) -> Result<(), WorkflowKernelError> {
        self.control(run_id, now, WorkflowRunStatus::Queued, "run_resumed")
            .await
    }

    pub async fn cancel(&self, run_id: &str, now: i64) -> Result<(), WorkflowKernelError> {
        let cancellation = self.run_cancellation(run_id).await;
        cancellation.cancel();
        let run_lock = self.named_run_lock(run_id).await;
        let _claim = run_lock.lock().await;
        let Some(mut run) = self.store.load_run(run_id).await? else {
            self.run_cancellations.lock().await.remove(run_id);
            return Err(WorkflowKernelError::UnknownRun(run_id.into()));
        };
        if run.status != WorkflowRunStatus::RecoveryRequired {
            run.cancellation_requested = true;
            cancel_run(&mut run, now);
        }
        let saved = self.store.save_run(&mut run).await;
        if saved.is_ok() {
            self.run_cancellations.lock().await.remove(run_id);
        }
        saved
    }

    /// Records authoritative reconciliation and safely requeues the affected Step.
    pub async fn recover(
        &self,
        run_id: &str,
        now: i64,
        evidence: &str,
    ) -> Result<(), WorkflowKernelError> {
        if evidence.trim().is_empty() {
            return Err(WorkflowKernelError::Invalid(
                "authoritative recovery evidence must not be empty".into(),
            ));
        }
        let run_lock = self.named_run_lock(run_id).await;
        let _claim = run_lock.lock().await;
        let mut run = self
            .store
            .load_run(run_id)
            .await?
            .ok_or_else(|| WorkflowKernelError::UnknownRun(run_id.into()))?;
        if run.status != WorkflowRunStatus::RecoveryRequired {
            return Err(WorkflowKernelError::Invalid(
                "run does not require authoritative recovery".into(),
            ));
        }
        let step_index = run
            .steps
            .iter()
            .position(|step| step.phase == StepPhase::RecoveryRequired)
            .ok_or_else(|| {
                WorkflowKernelError::Invalid("run has no recovery-required Step".into())
            })?;
        let attempt_index = run.steps[step_index]
            .attempts
            .iter()
            .rposition(|attempt| attempt.status == AttemptStatus::RecoveryRequired)
            .ok_or_else(|| {
                WorkflowKernelError::Invalid("Step has no recovery-required Attempt".into())
            })?;
        run.steps[step_index].attempts[attempt_index].status = AttemptStatus::Cancelled;
        run.steps[step_index].attempts[attempt_index].phase = StepPhase::Cancelled;
        run.steps[step_index].attempts[attempt_index]
            .finished_unix_ms
            .get_or_insert(now);
        run.steps[step_index].phase = StepPhase::Pending;
        run.steps[step_index].summary =
            Some("authoritative reconciliation accepted; Step requeued".into());
        run.steps[step_index].wake_at_unix_ms = None;
        run.status = WorkflowRunStatus::Queued;
        run.cancellation_requested = false;
        run.updated_unix_ms = now;
        let step_key = run.steps[step_index].key.clone();
        let attempt_id = run.steps[step_index].attempts[attempt_index].id.clone();
        push_event(
            &mut run,
            now,
            Some(&step_key),
            Some(&attempt_id),
            "recovery_reconciled",
            evidence.trim(),
        );
        self.run_cancellations.lock().await.remove(run_id);
        self.store.save_run(&mut run).await
    }

    /// Explicitly discards a recovery-required run after the operator accepts the uncertainty.
    pub async fn discard(&self, run_id: &str, now: i64) -> Result<(), WorkflowKernelError> {
        let cancellation = self.run_cancellation(run_id).await;
        cancellation.cancel();
        let run_lock = self.named_run_lock(run_id).await;
        let _claim = run_lock.lock().await;
        let mut run = self
            .store
            .load_run(run_id)
            .await?
            .ok_or_else(|| WorkflowKernelError::UnknownRun(run_id.into()))?;
        if run.status != WorkflowRunStatus::RecoveryRequired {
            return Err(WorkflowKernelError::Invalid(
                "only a recovery-required run can be discarded".into(),
            ));
        }
        discard_recovery_run(&mut run, now);
        let saved = self.store.save_run(&mut run).await;
        if saved.is_ok() {
            self.run_cancellations.lock().await.remove(run_id);
        }
        saved
    }

    pub async fn retry(&self, run_id: &str, now: i64) -> Result<(), WorkflowKernelError> {
        let run_lock = self.named_run_lock(run_id).await;
        let _claim = run_lock.lock().await;
        let mut run = self
            .store
            .load_run(run_id)
            .await?
            .ok_or_else(|| WorkflowKernelError::UnknownRun(run_id.into()))?;
        let step = run
            .steps
            .iter_mut()
            .find(|step| step.phase == StepPhase::Failed)
            .ok_or_else(|| {
                WorkflowKernelError::Invalid("run has no failed Step to retry".into())
            })?;
        if let Some(attempt) = step.attempts.last_mut()
            && attempt.status == AttemptStatus::Active
        {
            attempt.status = AttemptStatus::RecoveryRequired;
            attempt.finished_unix_ms = Some(now);
        }
        step.phase = StepPhase::Pending;
        step.summary = Some("retry requested".into());
        step.wake_at_unix_ms = None;
        run.status = WorkflowRunStatus::Queued;
        run.updated_unix_ms = now;
        push_event(&mut run, now, None, None, "run_retried", "retry queued");
        self.run_cancellations.lock().await.remove(run_id);
        self.store.save_run(&mut run).await
    }

    async fn control(
        &self,
        run_id: &str,
        now: i64,
        status: WorkflowRunStatus,
        event: &str,
    ) -> Result<(), WorkflowKernelError> {
        let run_lock = self.named_run_lock(run_id).await;
        let _claim = run_lock.lock().await;
        let mut run = self
            .store
            .load_run(run_id)
            .await?
            .ok_or_else(|| WorkflowKernelError::UnknownRun(run_id.into()))?;
        if run.status.terminal() {
            return Err(WorkflowKernelError::Invalid(
                "run is already terminal".into(),
            ));
        }
        run.status = status;
        run.updated_unix_ms = now;
        push_event(&mut run, now, None, None, event, event);
        self.store.save_run(&mut run).await
    }

    async fn run_cancellation(&self, run_id: &str) -> AgentCancellation {
        self.run_cancellations
            .lock()
            .await
            .entry(run_id.to_string())
            .or_default()
            .clone()
    }

    async fn named_run_lock(&self, run_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.run_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(run_id).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(run_id.to_string(), Arc::downgrade(&lock));
        lock
    }

    async fn worktree_lock(&self, worktree: &Path) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.worktree_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(worktree).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(worktree.to_path_buf(), Arc::downgrade(&lock));
        lock
    }
}

struct UnconditionalWaveBranch {
    step_index: usize,
    attempt_index: usize,
    step_key: String,
    prompts: Vec<String>,
    harness: Option<String>,
    model: Option<String>,
    variant: Option<String>,
    run_id: String,
    repository: PathBuf,
    worktree: PathBuf,
}

struct UnconditionalWaveTurn {
    step_index: usize,
    attempt_index: usize,
    turn_number: u32,
    total_turns: u32,
    outcome: AgentOutcome,
    elapsed: std::time::Duration,
    persisted: tokio::sync::oneshot::Sender<()>,
}

struct UnconditionalWaveResult {
    step_index: usize,
    attempt_index: usize,
    elapsed: std::time::Duration,
    error: Option<AgentExecutionError>,
}

async fn execute_unconditional_branch(
    agents: Arc<dyn AgentExecutor>,
    branch: UnconditionalWaveBranch,
    cancellation: AgentCancellation,
    turn_tx: tokio::sync::mpsc::UnboundedSender<UnconditionalWaveTurn>,
) -> UnconditionalWaveResult {
    let started = tokio::time::Instant::now();
    let mut last_outcome: Option<AgentOutcome> = None;
    let mut error = None;
    for (turn_index, prompt) in branch.prompts.iter().enumerate() {
        if cancellation.is_cancelled() {
            error = Some(AgentExecutionError::Cancelled);
            break;
        }
        let resume_session_id = last_outcome
            .as_ref()
            .map(|outcome| outcome.session_id.clone());
        let execution = agents
            .execute(AgentRequest {
                run_id: branch.run_id.clone(),
                step_key: branch.step_key.clone(),
                attempt_id: format!(
                    "{}:{}:{}",
                    branch.run_id,
                    branch.step_key,
                    branch.attempt_index + 1
                ),
                repository: branch.repository.clone(),
                worktree: branch.worktree.clone(),
                harness: branch.harness.clone(),
                model: branch.model.clone(),
                variant: branch.variant.clone(),
                prompt: prompt.clone(),
                resume_session_id: resume_session_id.clone(),
                require_resumable_session: branch.prompts.len() > 1 && turn_index == 0,
                cancellation: cancellation.clone(),
            })
            .await;
        match execution {
            Ok(outcome) => {
                if let Some(expected) = resume_session_id.as_deref()
                    && outcome.session_id != expected
                {
                    error = Some(AgentExecutionError::Protocol(format!(
                        "Agent follow-up resumed session {expected}, but reported {}",
                        outcome.session_id
                    )));
                    break;
                }
                last_outcome = Some(outcome.clone());
                let (persisted, persisted_rx) = tokio::sync::oneshot::channel();
                let turn_number = u32::try_from(turn_index + 1).unwrap_or(u32::MAX);
                let total_turns = u32::try_from(branch.prompts.len()).unwrap_or(u32::MAX);
                if turn_tx
                    .send(UnconditionalWaveTurn {
                        step_index: branch.step_index,
                        attempt_index: branch.attempt_index,
                        turn_number,
                        total_turns,
                        outcome,
                        elapsed: started.elapsed(),
                        persisted,
                    })
                    .is_err()
                    || persisted_rx.await.is_err()
                {
                    error = Some(AgentExecutionError::Protocol(
                        "concurrent Agent result could not be persisted".into(),
                    ));
                    break;
                }
            }
            Err(branch_error) => {
                error = Some(branch_error);
                break;
            }
        }
    }
    UnconditionalWaveResult {
        step_index: branch.step_index,
        attempt_index: branch.attempt_index,
        elapsed: started.elapsed(),
        error,
    }
}

fn resolve_trigger(
    registry: &TriggerRegistry,
    revision: &crate::workflow::source::TriggerRevision,
) -> Option<Arc<dyn StepTrigger>> {
    registry
        .get(&revision.digest)
        .or_else(|| registry.get(&revision.name))
}

fn begin_attempt(
    run: &mut WorkflowRunState,
    step_index: usize,
    now: i64,
) -> Result<usize, WorkflowKernelError> {
    let step = &mut run.steps[step_index];
    let number = u32::try_from(step.attempts.len() + 1)
        .map_err(|_| WorkflowKernelError::Invalid("too many Step Attempts".into()))?;
    let id = format!("{}:{}:{number}", run.id, step.key);
    step.phase = StepPhase::Preparing;
    step.attempts.push(WorkflowAttemptState {
        id,
        number,
        status: AttemptStatus::Active,
        phase: StepPhase::Preparing,
        prepared_state: None,
        agent_turns: Vec::new(),
        agent_turn_in_flight: None,
        agent_outcome: None,
        error: None,
        started_unix_ms: now,
        finished_unix_ms: None,
        fencing_token: u64::from(number),
        phase_owner: None,
        lease_expires_unix_ms: None,
    });
    let index = step.attempts.len() - 1;
    push_attempt_event(run, step_index, index, now, "attempt_started", "preparing");
    Ok(index)
}

fn claim_attempt_phase(
    run: &mut WorkflowRunState,
    step: usize,
    attempt: usize,
    worker_id: &str,
    now: i64,
) {
    let attempt = &mut run.steps[step].attempts[attempt];
    attempt.fencing_token = attempt.fencing_token.saturating_add(1);
    attempt.phase_owner = Some(worker_id.to_string());
    attempt.lease_expires_unix_ms = Some(now.saturating_add(PHASE_LEASE_MS));
}

fn renew_attempt_phase(
    run: &mut WorkflowRunState,
    step: usize,
    attempt: usize,
    worker_id: &str,
    fencing_token: u64,
    now: i64,
) -> Result<(), WorkflowKernelError> {
    validate_attempt_phase(run, step, attempt, worker_id, fencing_token, now)?;
    run.steps[step].attempts[attempt].lease_expires_unix_ms =
        Some(now.saturating_add(PHASE_LEASE_MS));
    Ok(())
}

fn validate_attempt_phase(
    run: &WorkflowRunState,
    step: usize,
    attempt: usize,
    worker_id: &str,
    fencing_token: u64,
    now: i64,
) -> Result<(), WorkflowKernelError> {
    let attempt = &run.steps[step].attempts[attempt];
    if attempt.phase_owner.as_deref() != Some(worker_id)
        || attempt.fencing_token != fencing_token
        || attempt
            .lease_expires_unix_ms
            .is_none_or(|expires| expires < now)
    {
        return Err(WorkflowKernelError::Conflict(format!(
            "stale lifecycle phase claim for Attempt {}",
            attempt.id
        )));
    }
    Ok(())
}

fn phase_time(started_unix_ms: i64, elapsed: std::time::Duration) -> i64 {
    started_unix_ms.saturating_add(i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
}

fn release_attempt_phase(run: &mut WorkflowRunState, step: usize, attempt: usize) {
    run.steps[step].attempts[attempt].phase_owner = None;
    run.steps[step].attempts[attempt].lease_expires_unix_ms = None;
}

fn finish_attempt(run: &mut WorkflowRunState, step: usize, attempt: usize, now: i64) {
    release_attempt_phase(run, step, attempt);
    run.steps[step].attempts[attempt].status = AttemptStatus::Succeeded;
    run.steps[step].attempts[attempt].finished_unix_ms = Some(now);
    push_attempt_event(run, step, attempt, now, "attempt_succeeded", "succeeded");
}

fn fail_attempt(
    run: &mut WorkflowRunState,
    step: usize,
    attempt: usize,
    now: i64,
    reason: impl Into<String>,
) {
    let reason = bounded(&reason.into());
    release_attempt_phase(run, step, attempt);
    run.steps[step].attempts[attempt].status = AttemptStatus::Failed;
    run.steps[step].attempts[attempt].phase = StepPhase::Failed;
    run.steps[step].attempts[attempt].error = Some(reason.clone());
    run.steps[step].attempts[attempt].finished_unix_ms = Some(now);
    run.steps[step].phase = StepPhase::Failed;
    run.steps[step].summary = Some(reason.clone());
    run.status = WorkflowRunStatus::Failed;
    push_attempt_event(run, step, attempt, now, "attempt_failed", &reason);
}

fn cancel_attempt(run: &mut WorkflowRunState, step: usize, attempt: usize, now: i64, reason: &str) {
    release_attempt_phase(run, step, attempt);
    run.steps[step].attempts[attempt].status = AttemptStatus::Cancelled;
    run.steps[step].attempts[attempt].phase = StepPhase::Cancelled;
    run.steps[step].attempts[attempt].error = Some(reason.into());
    run.steps[step].attempts[attempt].finished_unix_ms = Some(now);
    run.steps[step].phase = StepPhase::Cancelled;
    run.steps[step].summary = Some(reason.into());
    push_attempt_event(run, step, attempt, now, "attempt_cancelled", reason);
}

fn fail_step(run: &mut WorkflowRunState, step: usize, now: i64, reason: &str) {
    let reason = bounded(reason);
    run.steps[step].phase = StepPhase::Failed;
    run.steps[step].summary = Some(reason.clone());
    run.status = WorkflowRunStatus::Failed;
    let key = run.steps[step].key.clone();
    push_event(&mut *run, now, Some(&key), None, "step_failed", &reason);
}

fn recovery_required(
    run: &mut WorkflowRunState,
    step: usize,
    attempt: usize,
    now: i64,
    reason: &str,
) {
    release_attempt_phase(run, step, attempt);
    run.steps[step].attempts[attempt].status = AttemptStatus::RecoveryRequired;
    run.steps[step].attempts[attempt].phase = StepPhase::RecoveryRequired;
    run.steps[step].attempts[attempt].error = Some(reason.into());
    run.steps[step].phase = StepPhase::RecoveryRequired;
    run.steps[step].summary = Some(reason.into());
    run.status = WorkflowRunStatus::RecoveryRequired;
    push_attempt_event(run, step, attempt, now, "recovery_required", reason);
}

fn begin_new_cycle(workflow: &CompiledWorkflow, run: &mut WorkflowRunState, now: i64) {
    run.cycle = run.cycle.saturating_add(1);
    run.cycle_started_unix_ms = now.saturating_add(1);
    invalidate_trigger_observations(workflow, run);
    run.status = WorkflowRunStatus::Queued;
    run.updated_unix_ms = now;
    push_event(
        run,
        now,
        None,
        None,
        "cycle_invalidated",
        "Agent lifecycle changed observed state",
    );
}

fn invalidate_trigger_observations(workflow: &CompiledWorkflow, run: &mut WorkflowRunState) {
    for (compiled, state) in workflow.steps.iter().zip(&mut run.steps) {
        if compiled.trigger.is_some() {
            if state.phase == StepPhase::Waiting && state.active_attempt_index().is_some() {
                continue;
            }
            state.phase = StepPhase::Pending;
            state.summary = None;
            state.wake_at_unix_ms = None;
            state.satisfied_cycle = None;
        }
    }
}

fn dependencies_satisfied(
    workflow: &CompiledWorkflow,
    run: &WorkflowRunState,
    step_index: usize,
) -> bool {
    workflow.steps[step_index].dependencies.iter().all(|key| {
        workflow
            .steps
            .iter()
            .position(|step| &step.key == key)
            .is_some_and(|index| {
                run.steps[index].unconditional_completed
                    || run.steps[index].satisfied_cycle == Some(run.cycle)
            })
    })
}

fn graph_satisfied(workflow: &CompiledWorkflow, run: &WorkflowRunState) -> bool {
    workflow.steps.iter().enumerate().all(|(index, step)| {
        if step.trigger.is_some() {
            run.steps[index].satisfied_cycle == Some(run.cycle)
        } else {
            run.steps[index].unconditional_completed
        }
    })
}

fn selected_context(
    workflow: &CompiledWorkflow,
    run: &WorkflowRunState,
    step: &CompiledWorkflowStep,
) -> Result<Vec<(String, String)>, WorkflowKernelError> {
    step.context
        .iter()
        .map(|key| {
            let index = workflow
                .steps
                .iter()
                .position(|candidate| &candidate.key == key)
                .ok_or_else(|| {
                    WorkflowKernelError::Invalid(format!("unknown context Step {key}"))
                })?;
            let text = run.steps[index].final_text().ok_or_else(|| {
                WorkflowKernelError::Invalid(format!("context Step {key} has no final Agent text"))
            })?;
            Ok((key.clone(), text.to_string()))
        })
        .collect()
}

fn trigger_context(
    run: &WorkflowRunState,
    step: &CompiledWorkflowStep,
    attempt_id: Option<&str>,
) -> TriggerContext {
    TriggerContext {
        run_id: run.id.clone(),
        step_key: step.key.clone(),
        attempt_id: attempt_id
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}:{}:check:{}", run.id, step.key, run.cycle)),
        cycle: run.cycle,
        cycle_started_unix_ms: run.cycle_started_unix_ms,
        subject: run.subject.clone(),
        cancellation_requested: run.cancellation_requested,
    }
}

fn discard_recovery_run(run: &mut WorkflowRunState, now: i64) {
    run.status = WorkflowRunStatus::Cancelled;
    run.cancellation_requested = true;
    run.updated_unix_ms = now;
    for step in &mut run.steps {
        if step.phase == StepPhase::RecoveryRequired {
            step.phase = StepPhase::Cancelled;
            step.summary = Some("discarded after operator accepted uncertain effects".into());
            if let Some(attempt) = step
                .attempts
                .iter_mut()
                .rev()
                .find(|attempt| attempt.status == AttemptStatus::RecoveryRequired)
            {
                attempt.status = AttemptStatus::Cancelled;
                attempt.phase = StepPhase::Cancelled;
                attempt.finished_unix_ms.get_or_insert(now);
                attempt.phase_owner = None;
                attempt.lease_expires_unix_ms = None;
            }
        }
    }
    push_event(
        run,
        now,
        None,
        None,
        "recovery_discarded",
        "operator discarded run after accepting uncertain effects",
    );
}

fn cancel_run(run: &mut WorkflowRunState, now: i64) {
    if run.status == WorkflowRunStatus::RecoveryRequired
        || run
            .steps
            .iter()
            .any(|step| step.phase == StepPhase::RecoveryRequired)
    {
        return;
    }
    run.status = WorkflowRunStatus::Cancelled;
    run.updated_unix_ms = now;
    for step in &mut run.steps {
        if let Some(index) = step.active_attempt_index() {
            step.attempts[index].status = AttemptStatus::Cancelled;
            step.attempts[index].phase = StepPhase::Cancelled;
            step.attempts[index].finished_unix_ms = Some(now);
            step.attempts[index].phase_owner = None;
            step.attempts[index].lease_expires_unix_ms = None;
        }
        if !matches!(step.phase, StepPhase::Completed | StepPhase::Satisfied) {
            step.phase = StepPhase::Cancelled;
        }
    }
    push_event(run, now, None, None, "run_cancelled", "cancelled");
}

fn push_attempt_event(
    run: &mut WorkflowRunState,
    step: usize,
    attempt: usize,
    now: i64,
    kind: &str,
    summary: &str,
) {
    let step_key = run.steps[step].key.clone();
    let attempt_id = run.steps[step].attempts[attempt].id.clone();
    push_event(run, now, Some(&step_key), Some(&attempt_id), kind, summary);
}

fn push_event(
    run: &mut WorkflowRunState,
    now: i64,
    step: Option<&str>,
    attempt: Option<&str>,
    kind: &str,
    summary: &str,
) {
    let sequence = run.events.last().map_or(1, |event| event.sequence + 1);
    if run.events.len() >= MAX_RUN_EVENTS {
        run.events.remove(0);
    }
    run.events.push(WorkflowEvent {
        sequence,
        time_unix_ms: now,
        step_key: step.map(str::to_string),
        attempt_id: attempt.map(str::to_string),
        kind: kind.into(),
        summary: bounded(summary),
    });
}

fn bounded(value: &str) -> String {
    const LIMIT: usize = 1024;
    let mut value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.len() > LIMIT {
        value.truncate(LIMIT);
        while !value.is_char_boundary(value.len()) {
            value.pop();
        }
    }
    value
}

fn progress_for(status: WorkflowRunStatus) -> SchedulerProgress {
    match status {
        WorkflowRunStatus::Queued | WorkflowRunStatus::Running => SchedulerProgress::Advanced,
        WorkflowRunStatus::Waiting => SchedulerProgress::Waiting,
        WorkflowRunStatus::NeedsInput => SchedulerProgress::NeedsInput,
        WorkflowRunStatus::Paused => SchedulerProgress::Paused,
        WorkflowRunStatus::Succeeded => SchedulerProgress::Succeeded,
        WorkflowRunStatus::Failed => SchedulerProgress::Failed,
        WorkflowRunStatus::Cancelled => SchedulerProgress::Cancelled,
        WorkflowRunStatus::RecoveryRequired => SchedulerProgress::RecoveryRequired,
    }
}

#[derive(Default)]
pub struct MemoryWorkflowRunStore {
    workflows: Mutex<BTreeMap<String, CompiledWorkflow>>,
    runs: Mutex<BTreeMap<String, WorkflowRunState>>,
}

impl WorkflowRunStore for MemoryWorkflowRunStore {
    fn retain_workflow<'a>(&'a self, workflow: &'a CompiledWorkflow) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let mut workflows = self.workflows.lock().unwrap();
            if let Some(existing) = workflows.get(&workflow.digest) {
                if existing != workflow {
                    return Err(WorkflowKernelError::Conflict(
                        "immutable Workflow digest has different content".into(),
                    ));
                }
            } else {
                workflows.insert(workflow.digest.clone(), workflow.clone());
            }
            Ok(())
        })
    }

    fn load_workflow<'a>(&'a self, digest: &'a str) -> StoreFuture<'a, CompiledWorkflow> {
        Box::pin(async move {
            self.workflows
                .lock()
                .unwrap()
                .get(digest)
                .cloned()
                .ok_or_else(|| {
                    WorkflowKernelError::Invalid(format!("missing Workflow snapshot {digest}"))
                })
        })
    }

    fn create_run<'a>(&'a self, run: &'a WorkflowRunState) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            if self
                .runs
                .lock()
                .unwrap()
                .insert(run.id.clone(), run.clone())
                .is_some()
            {
                return Err(WorkflowKernelError::Conflict(format!(
                    "Workflow Run {} already exists",
                    run.id
                )));
            }
            Ok(())
        })
    }

    fn load_run<'a>(&'a self, run_id: &'a str) -> StoreFuture<'a, Option<WorkflowRunState>> {
        Box::pin(async move { Ok(self.runs.lock().unwrap().get(run_id).cloned()) })
    }

    fn save_run<'a>(&'a self, run: &'a mut WorkflowRunState) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let mut runs = self.runs.lock().unwrap();
            let current = runs
                .get(&run.id)
                .ok_or_else(|| WorkflowKernelError::UnknownRun(run.id.clone()))?;
            if current.revision != run.revision {
                return Err(WorkflowKernelError::Conflict(format!(
                    "Workflow Run {} changed concurrently",
                    run.id
                )));
            }
            run.revision += 1;
            runs.insert(run.id.clone(), run.clone());
            Ok(())
        })
    }
}

#[derive(Debug)]
pub enum WorkflowKernelError {
    Invalid(String),
    UnknownRun(String),
    MissingTrigger(String),
    Conflict(String),
    PhaseCancelled,
    Trigger {
        step: String,
        error: TriggerError,
    },
    Agent {
        step: String,
        error: AgentExecutionError,
    },
    Persistence(String),
}

impl std::fmt::Display for WorkflowKernelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(error) | Self::Conflict(error) | Self::Persistence(error) => {
                formatter.write_str(error)
            }
            Self::UnknownRun(run) => write!(formatter, "unknown Workflow Run {run}"),
            Self::PhaseCancelled => formatter.write_str("Workflow lifecycle phase cancelled"),
            Self::MissingTrigger(trigger) => {
                write!(formatter, "Trigger '{trigger}' is unavailable")
            }
            Self::Trigger { step, error } => {
                write!(formatter, "Step {step} Trigger failed: {error}")
            }
            Self::Agent { step, error } => write!(formatter, "Step {step} Agent failed: {error}"),
        }
    }
}

impl std::error::Error for WorkflowKernelError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Trigger { error, .. } => Some(error),
            Self::Agent { error, .. } => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::agent_phase::RecordingAgentExecutor;
    use crate::workflow::source::{TriggerCatalog, compile_workflow};
    use crate::workflow::step_trigger::{AgentOutcomeStatus, ScriptedTrigger};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use tokio::sync::{Barrier, Notify};

    fn subject() -> TriggerSubject {
        TriggerSubject {
            repository: "/repo".into(),
            worktree: "/repo/wt".into(),
            change_request: Some("cr:1".into()),
            change_request_head: Some("abc".into()),
        }
    }

    fn workflow(source: &str) -> CompiledWorkflow {
        compile_workflow(Path::new("test.toml"), source, &TriggerCatalog::builtins()).unwrap()
    }

    fn outcome(session: &str, text: &str) -> AgentOutcome {
        AgentOutcome {
            status: AgentOutcomeStatus::Succeeded,
            process_id: Some(42),
            session_id: session.into(),
            final_text: text.into(),
        }
    }

    #[tokio::test]
    async fn launch_requires_all_declared_inputs_to_be_bound() {
        let workflow = workflow("[inputs.plan]\nglob='*.md'\n[[step]]\nprompt='review {{plan}}'\n");
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let scheduler = WorkflowScheduler::new(
            store,
            TriggerRegistry::default(),
            Arc::new(RecordingAgentExecutor::default()),
        );
        let error = scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not fully bound"));
    }

    #[tokio::test]
    async fn waits_reruns_trigger_and_completes_only_after_full_cycle() {
        let workflow = workflow(
            "[[step]]\nid='repair'\ntrigger='needs_review'\nprompt='fix'\n[[step]]\ntrigger='ready_to_merge'\n",
        );
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let repair = ScriptedTrigger::new([
            TriggerDecision::Run {
                summary: "review".into(),
            },
            TriggerDecision::Satisfied {
                summary: "clean".into(),
            },
            TriggerDecision::Satisfied {
                summary: "clean".into(),
            },
        ]);
        let ready = ScriptedTrigger::new([
            TriggerDecision::Wait {
                summary: "CI".into(),
                wake_at_unix_ms: 20,
            },
            TriggerDecision::Satisfied {
                summary: "ready".into(),
            },
        ]);
        let triggers = TriggerRegistry::default();
        triggers.insert("needs_review", repair).unwrap();
        triggers.insert("ready_to_merge", ready).unwrap();
        let agents = Arc::new(RecordingAgentExecutor::default());
        agents.push_outcome(outcome("fresh-1", "fixed"));
        let scheduler = WorkflowScheduler::new(store.clone(), triggers, agents.clone());
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();

        assert_eq!(
            scheduler.tick("run", 2).await.unwrap(),
            SchedulerProgress::Advanced
        );
        assert_eq!(
            scheduler.tick("run", 3).await.unwrap(),
            SchedulerProgress::Waiting
        );
        assert_eq!(
            scheduler.tick("run", 19).await.unwrap(),
            SchedulerProgress::Waiting
        );
        assert_eq!(
            scheduler.tick("run", 20).await.unwrap(),
            SchedulerProgress::Succeeded
        );
        let run = store.load_run("run").await.unwrap().unwrap();
        assert_eq!(run.agent_runs_consumed, 1);
        assert_eq!(agents.requests()[0].prompt, "fix");
    }

    #[tokio::test]
    async fn independent_branch_advances_while_another_root_waits() {
        let workflow = workflow(
            "[[step]]\nid='remote'\ntrigger='ready_to_merge'\n\n[[step]]\nid='local'\ndepends_on=[]\nprompt='local work'\n",
        );
        let waiting = ScriptedTrigger::new([
            TriggerDecision::Wait {
                summary: "provider queue".into(),
                wake_at_unix_ms: 20,
            },
            TriggerDecision::Satisfied {
                summary: "ready".into(),
            },
        ]);
        let triggers = TriggerRegistry::default();
        triggers.insert("ready_to_merge", waiting).unwrap();
        let agents = Arc::new(RecordingAgentExecutor::default());
        agents.push_outcome(outcome("local-session", "done"));
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let scheduler = WorkflowScheduler::new(store, triggers, agents.clone());
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();

        assert_eq!(
            scheduler.tick("run", 2).await.unwrap(),
            SchedulerProgress::Advanced
        );
        assert_eq!(agents.requests()[0].step_key, "local");
        // The independent Agent settlement starts a fresh cycle, so the earlier Wait is
        // deliberately re-observed before its original wake and can now satisfy the run.
        assert_eq!(
            scheduler.tick("run", 3).await.unwrap(),
            SchedulerProgress::Succeeded
        );
    }

    #[tokio::test]
    async fn parallel_roots_overlap_and_join_waits_for_both() {
        struct OverlapAgent {
            barrier: Arc<Barrier>,
            roots_finished: Arc<AtomicUsize>,
            active_roots: Arc<AtomicUsize>,
            max_active_roots: Arc<AtomicUsize>,
            join_started: Arc<Notify>,
        }

        impl AgentExecutor for OverlapAgent {
            fn execute<'a>(
                &'a self,
                request: AgentRequest,
            ) -> super::super::agent_phase::AgentFuture<'a> {
                Box::pin(async move {
                    if request.step_key == "join" {
                        assert_eq!(
                            self.roots_finished.load(AtomicOrdering::Acquire),
                            2,
                            "join must not start until both roots have settled"
                        );
                        self.join_started.notify_one();
                        return Ok(outcome("join-session", "joined"));
                    }

                    let active = self.active_roots.fetch_add(1, AtomicOrdering::AcqRel) + 1;
                    self.max_active_roots
                        .fetch_max(active, AtomicOrdering::AcqRel);
                    self.barrier.wait().await;
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    self.active_roots.fetch_sub(1, AtomicOrdering::AcqRel);
                    self.roots_finished.fetch_add(1, AtomicOrdering::AcqRel);
                    Ok(outcome(
                        &format!("{}-session", request.step_key),
                        &format!("{} done", request.step_key),
                    ))
                })
            }
        }

        let workflow = workflow(
            "[[step]]\nid='a'\ndepends_on=[]\nprompt='root a'\n[[step]]\nid='b'\ndepends_on=[]\nprompt='root b'\n[[step]]\nid='join'\ndepends_on=['a','b']\ncontext=['a','b']\nprompt='join'\n",
        );
        let roots_finished = Arc::new(AtomicUsize::new(0));
        let max_active_roots = Arc::new(AtomicUsize::new(0));
        let join_started = Arc::new(Notify::new());
        let agents = Arc::new(OverlapAgent {
            barrier: Arc::new(Barrier::new(2)),
            roots_finished: roots_finished.clone(),
            active_roots: Arc::new(AtomicUsize::new(0)),
            max_active_roots: max_active_roots.clone(),
            join_started: join_started.clone(),
        });
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let scheduler = WorkflowScheduler::new(store.clone(), TriggerRegistry::default(), agents);
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();

        assert_eq!(
            scheduler.tick("run", 2).await.unwrap(),
            SchedulerProgress::Advanced
        );
        assert_eq!(max_active_roots.load(AtomicOrdering::Acquire), 2);
        assert_eq!(roots_finished.load(AtomicOrdering::Acquire), 2);
        let wave_run = store.load_run("run").await.unwrap().unwrap();
        assert_eq!(wave_run.step("join").unwrap().phase, StepPhase::Pending);
        let root_finished = ["a", "b"]
            .into_iter()
            .filter_map(|key| wave_run.step(key).unwrap().attempts[0].finished_unix_ms)
            .max()
            .unwrap();
        assert!(wave_run.cycle_started_unix_ms >= root_finished);
        assert!(wave_run.updated_unix_ms >= root_finished);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                join_started.notified()
            )
            .await
            .is_err(),
            "join started during the root wave"
        );

        assert_eq!(
            scheduler.tick("run", 3).await.unwrap(),
            SchedulerProgress::Advanced
        );
        join_started.notified().await;
        assert_eq!(
            scheduler.tick("run", 4).await.unwrap(),
            SchedulerProgress::Succeeded
        );
        let run = store.load_run("run").await.unwrap().unwrap();
        assert_eq!(run.agent_runs_consumed, 3);
        assert_eq!(run.step("join").unwrap().final_text(), Some("joined"));
    }

    #[tokio::test]
    async fn predecessor_context_is_plain_text_and_sessions_are_distinct() {
        let workflow = workflow(
            "[[step]]\nid='a'\ndepends_on=[]\nprompt='review a'\n[[step]]\nid='b'\ndepends_on=[]\nprompt='review b'\n[[step]]\nid='join'\ndepends_on=['a','b']\ncontext=['a','b']\nprompt='implement'\n",
        );
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let agents = Arc::new(RecordingAgentExecutor::default());
        agents.push_outcome(outcome("one", "A final"));
        agents.push_outcome(outcome("two", "B final"));
        agents.push_outcome(outcome("three", "implemented"));
        let scheduler =
            WorkflowScheduler::new(store.clone(), TriggerRegistry::default(), agents.clone());
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();
        for now in 2..10 {
            if scheduler.tick("run", now).await.unwrap() == SchedulerProgress::Succeeded {
                break;
            }
        }
        let requests = agents.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[2].prompt,
            "implement\n\n--- Context from a ---\nA final\n\n--- Context from b ---\nB final"
        );
        let sessions = store
            .load_run("run")
            .await
            .unwrap()
            .unwrap()
            .steps
            .into_iter()
            .flat_map(|step| step.attempts)
            .filter_map(|attempt| attempt.agent_outcome.map(|outcome| outcome.session_id))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(sessions.len(), 3);
    }

    #[tokio::test]
    async fn followups_share_one_session_and_one_agent_budget_unit() {
        let workflow =
            workflow("[[step]]\nprompt='audit'\nfollowups=['implement gaps','verify']\n");
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let agents = Arc::new(RecordingAgentExecutor::default());
        agents.push_outcome(outcome("shared", "found one gap"));
        agents.push_outcome(outcome("shared", "implemented"));
        agents.push_outcome(outcome("shared", "verified"));
        let scheduler =
            WorkflowScheduler::new(store.clone(), TriggerRegistry::default(), agents.clone());
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();

        assert_eq!(
            scheduler.tick("run", 2).await.unwrap(),
            SchedulerProgress::Advanced
        );
        let requests = agents.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].prompt, "audit");
        assert_eq!(requests[0].resume_session_id, None);
        assert!(requests[0].require_resumable_session);
        assert_eq!(requests[1].prompt, "implement gaps");
        assert_eq!(requests[1].resume_session_id.as_deref(), Some("shared"));
        assert_eq!(requests[2].prompt, "verify");
        assert_eq!(requests[2].resume_session_id.as_deref(), Some("shared"));

        let run = store.load_run("run").await.unwrap().unwrap();
        let attempt = &run.steps[0].attempts[0];
        assert_eq!(run.agent_runs_consumed, 1);
        assert_eq!(attempt.agent_turns.len(), 3);
        assert_eq!(
            attempt.agent_outcome.as_ref().unwrap().final_text,
            "verified"
        );
    }

    #[tokio::test]
    async fn restart_between_followups_resumes_without_repeating_a_completed_turn() {
        let workflow = workflow("[[step]]\nprompt='audit'\nfollowups=['implement gaps']\n");
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let agents = Arc::new(RecordingAgentExecutor::default());
        agents.push_outcome(outcome("shared", "implemented"));
        let scheduler =
            WorkflowScheduler::new(store.clone(), TriggerRegistry::default(), agents.clone());
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();

        let mut crashed = store.load_run("run").await.unwrap().unwrap();
        let attempt_index = begin_attempt(&mut crashed, 0, 2).unwrap();
        let attempt = &mut crashed.steps[0].attempts[attempt_index];
        attempt.prepared_state = Some(PreparedState::default());
        attempt.phase = StepPhase::RunningAgent;
        attempt.agent_turns.push(outcome("shared", "found one gap"));
        crashed.steps[0].phase = StepPhase::RunningAgent;
        crashed.agent_runs_consumed = 1;
        store.save_run(&mut crashed).await.unwrap();

        assert_eq!(
            scheduler.tick("run", 3).await.unwrap(),
            SchedulerProgress::Advanced
        );
        let requests = agents.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].prompt, "implement gaps");
        assert_eq!(requests[0].resume_session_id.as_deref(), Some("shared"));
        let run = store.load_run("run").await.unwrap().unwrap();
        assert_eq!(run.agent_runs_consumed, 1);
        assert_eq!(run.steps[0].attempts[0].agent_turns.len(), 2);
    }

    #[tokio::test]
    async fn interrupted_followup_requires_reconciliation_instead_of_repeating() {
        let workflow = workflow("[[step]]\nprompt='audit'\nfollowups=['implement gaps']\n");
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let scheduler = WorkflowScheduler::new(
            store.clone(),
            TriggerRegistry::default(),
            Arc::new(RecordingAgentExecutor::default()),
        );
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();
        let mut crashed = store.load_run("run").await.unwrap().unwrap();
        let attempt_index = begin_attempt(&mut crashed, 0, 2).unwrap();
        let attempt = &mut crashed.steps[0].attempts[attempt_index];
        attempt.prepared_state = Some(PreparedState::default());
        attempt.phase = StepPhase::RunningAgent;
        attempt.agent_turns.push(outcome("shared", "found one gap"));
        attempt.agent_turn_in_flight = Some(2);
        crashed.steps[0].phase = StepPhase::RunningAgent;
        crashed.agent_runs_consumed = 1;
        store.save_run(&mut crashed).await.unwrap();

        assert_eq!(
            scheduler.tick("run", 3).await.unwrap(),
            SchedulerProgress::RecoveryRequired
        );
    }

    #[tokio::test]
    async fn restart_after_prepare_resumes_with_agent_then_finalize() {
        let workflow = workflow("[[step]]\ntrigger='needs_review'\nprompt='fix'\n");
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let trigger = ScriptedTrigger::new([]);
        trigger.push_completion(Ok(PostStepResult::Success {
            summary: "resolved".into(),
        }));
        let triggers = TriggerRegistry::default();
        triggers.insert("needs_review", trigger).unwrap();
        let agents = Arc::new(RecordingAgentExecutor::default());
        agents.push_outcome(outcome("fresh", "fixed"));
        let scheduler = WorkflowScheduler::new(store.clone(), triggers, agents.clone());
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();

        let mut crashed = store.load_run("run").await.unwrap().unwrap();
        let attempt = begin_attempt(&mut crashed, 0, 2).unwrap();
        crashed.steps[0].attempts[attempt].prepared_state =
            Some(PreparedState(serde_json::json!({"threads":["T1"]})));
        crashed.steps[0].attempts[attempt].phase = StepPhase::Prepared;
        crashed.steps[0].phase = StepPhase::Prepared;
        store.save_run(&mut crashed).await.unwrap();

        assert_eq!(
            scheduler.tick("run", 3).await.unwrap(),
            SchedulerProgress::Advanced
        );
        let resumed = store.load_run("run").await.unwrap().unwrap();
        assert_eq!(
            resumed.steps[0].attempts[0].status,
            AttemptStatus::Succeeded
        );
        assert_eq!(
            resumed.steps[0].attempts[0]
                .prepared_state
                .as_ref()
                .unwrap()
                .0["threads"][0],
            "T1"
        );
        assert_eq!(agents.requests().len(), 1);
    }

    #[tokio::test]
    async fn uncertain_external_finalize_requires_recovery_instead_of_repeating() {
        let workflow = workflow("[[step]]\ntrigger='ci_failure'\nprompt='fix'\n");
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let trigger = ScriptedTrigger::new([])
            .with_recovery_policy(crate::workflow::step_trigger::TriggerRecoveryPolicy::UNCERTAIN);
        let triggers = TriggerRegistry::default();
        triggers.insert("ci_failure", trigger).unwrap();
        let agents = Arc::new(RecordingAgentExecutor::default());
        let scheduler = WorkflowScheduler::new(store.clone(), triggers, agents);
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();

        let mut crashed = store.load_run("run").await.unwrap().unwrap();
        let attempt = begin_attempt(&mut crashed, 0, 2).unwrap();
        crashed.steps[0].attempts[attempt].prepared_state = Some(PreparedState::default());
        crashed.steps[0].attempts[attempt].agent_outcome = Some(outcome("fresh", "fixed"));
        crashed.steps[0].attempts[attempt].phase = StepPhase::Finalizing;
        crashed.steps[0].phase = StepPhase::Finalizing;
        store.save_run(&mut crashed).await.unwrap();

        assert_eq!(
            scheduler.tick("run", 3).await.unwrap(),
            SchedulerProgress::RecoveryRequired
        );
        assert_eq!(
            store.load_run("run").await.unwrap().unwrap().status,
            WorkflowRunStatus::RecoveryRequired
        );
    }

    #[tokio::test]
    async fn cancelled_uncertain_prepare_requires_reconciliation() {
        struct BlockingPrepare {
            started: Arc<std::sync::atomic::AtomicBool>,
        }
        impl StepTrigger for BlockingPrepare {
            fn should_run_step<'a>(
                &'a self,
                _context: &'a TriggerContext,
            ) -> super::super::step_trigger::TriggerFuture<'a, TriggerDecision> {
                Box::pin(async {
                    Ok(TriggerDecision::Run {
                        summary: "run".into(),
                    })
                })
            }

            fn pre_step_run<'a>(
                &'a self,
                _context: &'a TriggerContext,
            ) -> super::super::step_trigger::TriggerFuture<'a, PreparedState> {
                self.started.store(true, Ordering::Release);
                Box::pin(async {
                    std::future::pending::<()>().await;
                    Ok(PreparedState::default())
                })
            }

            fn recovery_policy(&self) -> crate::workflow::step_trigger::TriggerRecoveryPolicy {
                crate::workflow::step_trigger::TriggerRecoveryPolicy::UNCERTAIN
            }
        }

        let workflow = workflow("[[step]]\ntrigger='needs_review'\nprompt='fix'\n");
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let triggers = TriggerRegistry::default();
        triggers
            .insert(
                "needs_review",
                BlockingPrepare {
                    started: started.clone(),
                },
            )
            .unwrap();
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let scheduler = WorkflowScheduler::new(
            store.clone(),
            triggers,
            Arc::new(RecordingAgentExecutor::default()),
        );
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();
        let ticking = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.tick("run", 2).await })
        };
        while !started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        scheduler.cancel("run", 3).await.unwrap();
        assert_eq!(
            ticking.await.unwrap().unwrap(),
            SchedulerProgress::RecoveryRequired
        );
        assert_eq!(
            store.load_run("run").await.unwrap().unwrap().status,
            WorkflowRunStatus::RecoveryRequired
        );
    }

    #[tokio::test]
    async fn cancelled_uncertain_finalize_requires_reconciliation() {
        struct BlockingFinalize {
            started: Arc<std::sync::atomic::AtomicBool>,
        }
        impl StepTrigger for BlockingFinalize {
            fn should_run_step<'a>(
                &'a self,
                _context: &'a TriggerContext,
            ) -> super::super::step_trigger::TriggerFuture<'a, TriggerDecision> {
                Box::pin(async {
                    Ok(TriggerDecision::Run {
                        summary: "run".into(),
                    })
                })
            }

            fn post_step_run<'a>(
                &'a self,
                _context: &'a TriggerContext,
                _prepared: &'a PreparedState,
                _outcome: &'a AgentOutcome,
            ) -> super::super::step_trigger::TriggerFuture<'a, PostStepResult> {
                self.started.store(true, Ordering::Release);
                Box::pin(async {
                    std::future::pending::<()>().await;
                    Ok(PostStepResult::Success {
                        summary: "never".into(),
                    })
                })
            }

            fn recovery_policy(&self) -> crate::workflow::step_trigger::TriggerRecoveryPolicy {
                crate::workflow::step_trigger::TriggerRecoveryPolicy::UNCERTAIN
            }
        }

        let workflow = workflow("[[step]]\ntrigger='needs_review'\nprompt='fix'\n");
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let triggers = TriggerRegistry::default();
        triggers
            .insert(
                "needs_review",
                BlockingFinalize {
                    started: started.clone(),
                },
            )
            .unwrap();
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let agents = Arc::new(RecordingAgentExecutor::default());
        agents.push_outcome(outcome("agent", "fixed"));
        let scheduler = WorkflowScheduler::new(store.clone(), triggers, agents);
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();
        let ticking = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.tick("run", 2).await })
        };
        while !started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        scheduler.cancel("run", 3).await.unwrap();
        assert_eq!(
            ticking.await.unwrap().unwrap(),
            SchedulerProgress::RecoveryRequired
        );
        assert_eq!(
            store.load_run("run").await.unwrap().unwrap().status,
            WorkflowRunStatus::RecoveryRequired
        );
        assert!(scheduler.run_cancellations.lock().await.is_empty());
        assert!(matches!(
            scheduler.retry("run", 4).await,
            Err(WorkflowKernelError::Invalid(message)) if message.contains("no failed Step")
        ));
    }

    #[tokio::test]
    async fn authoritative_recovery_requeues_and_records_evidence() {
        let workflow = workflow("[[step]]\nprompt='work'\n");
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let scheduler = WorkflowScheduler::new(
            store.clone(),
            TriggerRegistry::default(),
            Arc::new(RecordingAgentExecutor::default()),
        );
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();
        let mut run = store.load_run("run").await.unwrap().unwrap();
        let attempt = begin_attempt(&mut run, 0, 2).unwrap();
        recovery_required(&mut run, 0, attempt, 3, "uncertain effect");
        store.save_run(&mut run).await.unwrap();

        assert!(
            scheduler
                .recover("run", 4, "provider event E-17 confirms completion")
                .await
                .is_ok()
        );
        let run = store.load_run("run").await.unwrap().unwrap();
        assert_eq!(run.status, WorkflowRunStatus::Queued);
        assert_eq!(run.steps[0].phase, StepPhase::Pending);
        assert_eq!(run.steps[0].attempts[0].status, AttemptStatus::Cancelled);
        assert!(run.events.iter().any(|event| {
            event.kind == "recovery_reconciled" && event.summary.contains("E-17")
        }));
    }

    #[tokio::test]
    async fn explicit_recovery_discard_terminally_cancels() {
        let workflow = workflow("[[step]]\nprompt='work'\n");
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let scheduler = WorkflowScheduler::new(
            store.clone(),
            TriggerRegistry::default(),
            Arc::new(RecordingAgentExecutor::default()),
        );
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();
        let mut run = store.load_run("run").await.unwrap().unwrap();
        let attempt = begin_attempt(&mut run, 0, 2).unwrap();
        recovery_required(&mut run, 0, attempt, 3, "uncertain effect");
        store.save_run(&mut run).await.unwrap();

        scheduler.discard("run", 4).await.unwrap();
        let run = store.load_run("run").await.unwrap().unwrap();
        assert_eq!(run.status, WorkflowRunStatus::Cancelled);
        assert_eq!(run.steps[0].phase, StepPhase::Cancelled);
        assert!(
            run.events
                .iter()
                .any(|event| event.kind == "recovery_discarded")
        );
    }

    #[tokio::test]
    async fn cancel_unknown_run_does_not_poison_a_later_run() {
        let workflow = workflow("[[step]]\nprompt='work'\n");
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let agents = Arc::new(RecordingAgentExecutor::default());
        agents.push_outcome(outcome("agent", "done"));
        let scheduler = WorkflowScheduler::new(store, TriggerRegistry::default(), agents);
        assert!(matches!(
            scheduler.cancel("reused", 1).await,
            Err(WorkflowKernelError::UnknownRun(_))
        ));
        assert!(scheduler.run_cancellations.lock().await.is_empty());
        scheduler
            .start(StartPromptWorkflow {
                run_id: "reused",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 2,
            })
            .await
            .unwrap();
        assert_eq!(
            scheduler.tick("reused", 3).await.unwrap(),
            SchedulerProgress::Advanced
        );
    }

    #[tokio::test]
    async fn cancel_interrupts_an_owned_trigger_phase() {
        struct BlockingTrigger {
            started: Arc<std::sync::atomic::AtomicBool>,
        }
        impl StepTrigger for BlockingTrigger {
            fn should_run_step<'a>(
                &'a self,
                _context: &'a TriggerContext,
            ) -> super::super::step_trigger::TriggerFuture<'a, TriggerDecision> {
                Box::pin(async {
                    Ok(TriggerDecision::Run {
                        summary: "run".into(),
                    })
                })
            }

            fn pre_step_run<'a>(
                &'a self,
                _context: &'a TriggerContext,
            ) -> super::super::step_trigger::TriggerFuture<'a, PreparedState> {
                self.started.store(true, Ordering::Release);
                Box::pin(async {
                    std::future::pending::<()>().await;
                    Ok(PreparedState::default())
                })
            }
        }

        let workflow = workflow("[[step]]\ntrigger='needs_review'\nprompt='fix'\n");
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let triggers = TriggerRegistry::default();
        triggers
            .insert(
                "needs_review",
                BlockingTrigger {
                    started: started.clone(),
                },
            )
            .unwrap();
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let scheduler = WorkflowScheduler::new(
            store.clone(),
            triggers,
            Arc::new(RecordingAgentExecutor::default()),
        );
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();
        let ticking = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.tick("run", 2).await })
        };
        while !started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }

        scheduler.cancel("run", 3).await.unwrap();
        assert!(matches!(
            ticking.await.unwrap(),
            Err(WorkflowKernelError::PhaseCancelled)
        ));
        assert_eq!(
            store.load_run("run").await.unwrap().unwrap().status,
            WorkflowRunStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn cancel_interrupts_a_blocked_trigger_check() {
        struct BlockingCheck {
            started: Arc<std::sync::atomic::AtomicBool>,
        }
        impl StepTrigger for BlockingCheck {
            fn should_run_step<'a>(
                &'a self,
                _context: &'a TriggerContext,
            ) -> super::super::step_trigger::TriggerFuture<'a, TriggerDecision> {
                self.started.store(true, Ordering::Release);
                Box::pin(async {
                    std::future::pending::<()>().await;
                    Ok(TriggerDecision::Satisfied {
                        summary: "never".into(),
                    })
                })
            }
        }

        let workflow = workflow("[[step]]\ntrigger='needs_review'\nprompt='fix'\n");
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let triggers = TriggerRegistry::default();
        triggers
            .insert(
                "needs_review",
                BlockingCheck {
                    started: started.clone(),
                },
            )
            .unwrap();
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let scheduler = WorkflowScheduler::new(
            store.clone(),
            triggers,
            Arc::new(RecordingAgentExecutor::default()),
        );
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();
        let ticking = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.tick("run", 2).await })
        };
        while !started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            scheduler.cancel("run", 3),
        )
        .await
        .expect("cancellation must interrupt Trigger checks")
        .unwrap();
        assert!(matches!(
            ticking.await.unwrap(),
            Err(WorkflowKernelError::PhaseCancelled)
        ));
    }

    #[tokio::test]
    async fn cancellation_published_before_tick_prevents_phase_registration() {
        let workflow = workflow("[[step]]\ntrigger='needs_review'\nprompt='fix'\n");
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let scheduler = WorkflowScheduler::new(
            store.clone(),
            TriggerRegistry::default(),
            Arc::new(RecordingAgentExecutor::default()),
        );
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();
        scheduler.run_cancellation("run").await.cancel();
        assert!(matches!(
            scheduler.tick("run", 2).await,
            Err(WorkflowKernelError::PhaseCancelled)
        ));
        assert_eq!(
            store.load_run("run").await.unwrap().unwrap().status,
            WorkflowRunStatus::Queued
        );
    }

    #[tokio::test]
    async fn weak_lock_registries_preserve_live_identity_and_prune_dead_keys() {
        let scheduler = WorkflowScheduler::new(
            Arc::new(MemoryWorkflowRunStore::default()),
            TriggerRegistry::default(),
            Arc::new(RecordingAgentExecutor::default()),
        );
        let live = scheduler.named_run_lock("live").await;
        for index in 0..100 {
            drop(scheduler.named_run_lock(&format!("dead-{index}")).await);
        }
        let same = scheduler.named_run_lock("live").await;
        assert!(Arc::ptr_eq(&live, &same));
        drop(same);
        drop(live);
        drop(scheduler.named_run_lock("prune").await);
        assert!(scheduler.run_locks.lock().await.len() <= 1);

        let worktree = scheduler.worktree_lock(Path::new("/worktree/live")).await;
        let same = scheduler.worktree_lock(Path::new("/worktree/live")).await;
        assert!(Arc::ptr_eq(&worktree, &same));
        drop(worktree);
        drop(same);
        drop(scheduler.worktree_lock(Path::new("/worktree/prune")).await);
        assert!(scheduler.worktree_locks.lock().await.len() <= 1);
    }

    #[tokio::test]
    async fn long_agent_phase_renews_its_persisted_lease() {
        struct SlowAgent;
        impl AgentExecutor for SlowAgent {
            fn execute<'a>(
                &'a self,
                _request: AgentRequest,
            ) -> super::super::agent_phase::AgentFuture<'a> {
                Box::pin(async {
                    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                    Ok(outcome("slow", "done"))
                })
            }
        }

        let workflow = workflow("[[step]]\nprompt='work'\n");
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let scheduler = WorkflowScheduler::new(
            store.clone(),
            TriggerRegistry::default(),
            Arc::new(SlowAgent),
        );
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();

        assert_eq!(
            scheduler.tick("run", 2).await.unwrap(),
            SchedulerProgress::Advanced
        );
        let run = store.load_run("run").await.unwrap().unwrap();
        let attempt = &run.steps[0].attempts[0];
        assert!(
            attempt
                .finished_unix_ms
                .is_some_and(|finished| finished >= 100)
        );
        assert!(attempt.phase_owner.is_none());
        assert!(attempt.lease_expires_unix_ms.is_none());
        assert!(run.revision >= 5, "lease renewal should add a durable save");
    }

    #[tokio::test]
    async fn waiting_finalize_resumes_same_attempt_without_another_agent() {
        let workflow = workflow("[[step]]\ntrigger='needs_review'\nprompt='fix'\n");
        let trigger = ScriptedTrigger::new([
            TriggerDecision::Run {
                summary: "review".into(),
            },
            TriggerDecision::Satisfied {
                summary: "clean".into(),
            },
        ]);
        trigger.push_completion(Ok(PostStepResult::Wait {
            summary: "waiting for provider slot".into(),
            wake_at_unix_ms: 20,
        }));
        trigger.push_completion(Ok(PostStepResult::Success {
            summary: "captured threads resolved".into(),
        }));
        let triggers = TriggerRegistry::default();
        triggers.insert("needs_review", trigger).unwrap();
        let agents = Arc::new(RecordingAgentExecutor::default());
        agents.push_outcome(outcome("fresh", "fixed"));
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let scheduler = WorkflowScheduler::new(store.clone(), triggers, agents.clone());
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();

        assert_eq!(
            scheduler.tick("run", 2).await.unwrap(),
            SchedulerProgress::Advanced
        );
        assert_eq!(
            scheduler.tick("run", 3).await.unwrap(),
            SchedulerProgress::Waiting
        );
        assert_eq!(
            scheduler.tick("run", 19).await.unwrap(),
            SchedulerProgress::Waiting
        );
        assert_eq!(
            scheduler.tick("run", 20).await.unwrap(),
            SchedulerProgress::Advanced
        );
        assert_eq!(
            scheduler.tick("run", 21).await.unwrap(),
            SchedulerProgress::Succeeded
        );
        let run = store.load_run("run").await.unwrap().unwrap();
        assert_eq!(run.steps[0].attempts.len(), 1);
        assert_eq!(run.steps[0].attempts[0].status, AttemptStatus::Succeeded);
        assert_eq!(agents.requests().len(), 1);
    }

    #[tokio::test]
    async fn endless_trigger_uses_bounded_agent_budget() {
        let mut workflow = workflow(
            "[defaults]\nmax_agent_runs=2\n[[step]]\ntrigger='ci_failure'\nprompt='fix'\n",
        );
        workflow.max_agent_runs = 2;
        let trigger = ScriptedTrigger::new((0..4).map(|_| TriggerDecision::Run {
            summary: "still failing".into(),
        }));
        let triggers = TriggerRegistry::default();
        triggers.insert("ci_failure", trigger).unwrap();
        let agents = Arc::new(RecordingAgentExecutor::default());
        agents.push_outcome(outcome("one", "first"));
        agents.push_outcome(outcome("two", "second"));
        let store = Arc::new(MemoryWorkflowRunStore::default());
        let scheduler = WorkflowScheduler::new(store.clone(), triggers, agents);
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: subject(),
                now_unix_ms: 1,
            })
            .await
            .unwrap();
        assert_eq!(
            scheduler.tick("run", 2).await.unwrap(),
            SchedulerProgress::Advanced
        );
        assert_eq!(
            scheduler.tick("run", 3).await.unwrap(),
            SchedulerProgress::Advanced
        );
        assert_eq!(
            scheduler.tick("run", 4).await.unwrap(),
            SchedulerProgress::NeedsInput
        );
        assert_eq!(
            store
                .load_run("run")
                .await
                .unwrap()
                .unwrap()
                .agent_runs_consumed,
            2
        );
    }
}
