#![allow(dead_code)] // Protected adapters remain test-only before production cutover.

pub(crate) mod adapters;

use std::collections::BTreeMap;
use std::sync::Arc;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::coordinator::{AttemptLease, ResourceClaimSpec};
use crate::definition::Capability;
use crate::run::{AuthorityGrantId, EffectIntentId, RunId, RunLedger, StepId, now_ms, random_id};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EffectState {
    Prepared,
    Dispatched,
    Applied,
    NotApplied,
    ExternallySatisfied,
    Diverged,
    Indeterminate,
    Failed,
}

impl EffectState {
    fn label(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Dispatched => "dispatched",
            Self::Applied => "applied",
            Self::NotApplied => "not_applied",
            Self::ExternallySatisfied => "externally_satisfied",
            Self::Diverged => "diverged",
            Self::Indeterminate => "indeterminate",
            Self::Failed => "failed",
        }
    }

    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Applied | Self::ExternallySatisfied | Self::Diverged | Self::Failed
        )
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct GateRequirement {
    pub gate_result_id: String,
    pub subject_digest: String,
    pub subject_generation: String,
    pub policy_revision: String,
}

#[derive(Clone, Debug)]
pub(crate) struct DispatchEffect {
    pub run_id: RunId,
    pub step_id: StepId,
    pub lease: AttemptLease,
    pub kind: String,
    pub target: serde_json::Value,
    pub expected_pre_state: serde_json::Value,
    pub desired_post_state: serde_json::Value,
    pub exact_head: Option<String>,
    pub input_revisions: Vec<String>,
    pub gate_requirements: Vec<GateRequirement>,
    pub policy_revisions: Vec<String>,
    pub authority_grant_id: AuthorityGrantId,
    pub reconciliation_key: String,
    pub resource_claims: Vec<ResourceClaimSpec>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct EffectIntent {
    pub id: EffectIntentId,
    pub run_id: RunId,
    pub step_id: StepId,
    pub attempt_id: crate::run::AttemptId,
    pub kind: String,
    pub state: EffectState,
    pub target: serde_json::Value,
    pub expected_pre_state: serde_json::Value,
    pub desired_post_state: serde_json::Value,
    pub exact_head: Option<String>,
    pub input_revisions: Vec<String>,
    pub gate_requirements: Vec<GateRequirement>,
    pub policy_revisions: Vec<String>,
    pub authority_grant_id: AuthorityGrantId,
    pub reconciliation_key: String,
    pub resource_claims: Vec<ResourceClaimSpec>,
    pub dispatch_generation: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum ReconciliationResult {
    Applied {
        evidence: serde_json::Value,
    },
    NotApplied {
        preconditions_still_hold: bool,
        evidence: serde_json::Value,
    },
    ExternallySatisfied {
        evidence: serde_json::Value,
    },
    Diverged {
        reason: String,
        evidence: serde_json::Value,
    },
    Indeterminate {
        reason: String,
    },
    Failed {
        reason: String,
    },
}

impl ReconciliationResult {
    fn state(&self) -> EffectState {
        match self {
            Self::Applied { .. } => EffectState::Applied,
            Self::NotApplied { .. } => EffectState::NotApplied,
            Self::ExternallySatisfied { .. } => EffectState::ExternallySatisfied,
            Self::Diverged { .. } => EffectState::Diverged,
            Self::Indeterminate { .. } => EffectState::Indeterminate,
            Self::Failed { .. } => EffectState::Failed,
        }
    }
}

pub(crate) trait EffectAdapter: Send + Sync {
    fn dispatch(&self, intent: &EffectIntent) -> Result<ReconciliationResult, String>;
    fn reconcile(&self, intent: &EffectIntent) -> Result<ReconciliationResult, String>;
}

#[derive(Clone)]
pub(crate) struct EffectBroker {
    ledger: RunLedger,
    adapters: BTreeMap<String, Arc<dyn EffectAdapter>>,
}

impl EffectBroker {
    pub(crate) fn new(ledger: RunLedger) -> Self {
        Self {
            ledger,
            adapters: BTreeMap::new(),
        }
    }

    pub(crate) fn register(
        &mut self,
        kind: impl Into<String>,
        adapter: Arc<dyn EffectAdapter>,
    ) -> Result<(), String> {
        let kind = kind.into();
        if self.adapters.contains_key(&kind) {
            return Err(format!("effect adapter '{kind}' is already registered"));
        }
        self.adapters.insert(kind, adapter);
        Ok(())
    }

    pub(crate) fn dispatch(&self, request: DispatchEffect) -> Result<EffectIntent, String> {
        let adapter = self
            .adapters
            .get(&request.kind)
            .ok_or_else(|| format!("effect adapter '{}' is unavailable", request.kind))?
            .clone();
        let intent = self.prepare(&request)?;
        if intent.state != EffectState::Prepared {
            return Ok(intent);
        }
        self.mark_dispatched(&intent, &request.lease)?;
        let mut dispatched = intent;
        dispatched.state = EffectState::Dispatched;
        dispatched.dispatch_generation += 1;
        let result = match adapter.dispatch(&dispatched) {
            Ok(result) => result,
            // Once dispatch has started, an adapter error cannot prove whether the
            // protected mutation happened. It must never become a retry signal.
            Err(error) => ReconciliationResult::Indeterminate {
                reason: format!("transport result is uncertain: {error}"),
            },
        };
        self.record_result(
            &dispatched.id,
            &result,
            Some(&request.lease),
            dispatched.dispatch_generation,
        )?;
        dispatched.state = result.state();
        Ok(dispatched)
    }

    pub(crate) fn reconcile(&self, intent_id: &EffectIntentId) -> Result<EffectIntent, String> {
        let started = std::time::Instant::now();
        let mut intent = self.load(intent_id)?;
        if intent.state.terminal() {
            return Ok(intent);
        }
        let adapter = self
            .adapters
            .get(&intent.kind)
            .ok_or_else(|| format!("effect adapter '{}' is unavailable", intent.kind))?;
        let result = match adapter.reconcile(&intent) {
            Ok(ReconciliationResult::Applied { evidence })
                if intent.state == EffectState::Prepared =>
            {
                // The broker never dispatched a prepared intent. If its desired
                // state already exists, another actor satisfied it.
                ReconciliationResult::ExternallySatisfied { evidence }
            }
            Ok(result) => result,
            Err(error) => ReconciliationResult::Indeterminate { reason: error },
        };
        self.record_result(intent_id, &result, None, intent.dispatch_generation)?;
        intent.state = result.state();
        crate::flight_recorder::record(
            "workflow_effect",
            "reconcile",
            Some(started.elapsed()),
            vec![crate::flight_recorder::text(
                "outcome",
                intent.state.label(),
            )],
        );
        Ok(intent)
    }

    pub(crate) fn retry_proven_not_applied(
        &self,
        intent_id: &EffectIntentId,
        lease: &AttemptLease,
    ) -> Result<EffectIntent, String> {
        let intent = self.load(intent_id)?;
        if intent.state != EffectState::NotApplied {
            return Err("only an authoritatively not-applied effect can be retried".to_string());
        }
        let result: serde_json::Value = self
            .ledger
            .connection()?
            .query_row(
                "select result_json from effect_intent where id=?1",
                [intent_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .map_err(sql_error)
            .and_then(|value| serde_json::from_str(&value).map_err(|error| error.to_string()))?;
        if result
            .get("preconditions_still_hold")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            return Err("effect preconditions no longer hold".to_string());
        }
        validate_current_authority(&self.ledger.connection()?, &intent, lease)?;
        let adapter = self
            .adapters
            .get(&intent.kind)
            .ok_or_else(|| format!("effect adapter '{}' is unavailable", intent.kind))?;
        self.mark_dispatched(&intent, lease)?;
        let mut retried = intent;
        retried.state = EffectState::Dispatched;
        retried.dispatch_generation += 1;
        let result = adapter
            .dispatch(&retried)
            .unwrap_or_else(|error| ReconciliationResult::Indeterminate { reason: error });
        self.record_result(intent_id, &result, Some(lease), retried.dispatch_generation)?;
        retried.state = result.state();
        Ok(retried)
    }

    fn prepare(&self, request: &DispatchEffect) -> Result<EffectIntent, String> {
        let mut conn = self.ledger.connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(existing) = transaction
            .query_row(
                "select id from effect_intent where kind=?1 and reconciliation_key=?2",
                params![request.kind, request.reconciliation_key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sql_error)?
        {
            let intent = load_intent(&transaction, &EffectIntentId(existing))?;
            if !request_matches_intent(request, &intent) {
                return Err(
                    "effect reconciliation key collides with a different request".to_string(),
                );
            }
            transaction.commit().map_err(sql_error)?;
            return Ok(intent);
        }
        validate_request(&transaction, request)?;
        let mutation_budget: i64 = transaction
            .query_row(
                "select b.remaining_mutations from workflow_run r join workflow_budget b on b.id=r.budget_id where r.id=?1",
                [request.run_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if mutation_budget <= 0 {
            return Err("shared mutation budget is exhausted".to_string());
        }
        let id = EffectIntentId(random_id(&transaction)?);
        let now = now_ms();
        transaction.execute("insert into effect_intent(id,run_id,step_id,attempt_id,kind,state,target_json,expected_pre_state_json,desired_post_state_json,exact_head,input_revisions_json,gate_requirements_json,policy_revisions_json,authority_grant_id,reconciliation_key,resource_claims_json,dispatch_generation,created_unix_ms,updated_unix_ms) values(?1,?2,?3,?4,?5,'prepared',?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,0,?16,?16)",params![id.as_str(),request.run_id.as_str(),request.step_id.as_str(),request.lease.attempt_id.as_str(),request.kind,request.target.to_string(),request.expected_pre_state.to_string(),request.desired_post_state.to_string(),request.exact_head,serde_json::to_string(&request.input_revisions).unwrap(),serde_json::to_string(&request.gate_requirements).unwrap(),serde_json::to_string(&request.policy_revisions).unwrap(),request.authority_grant_id.as_str(),request.reconciliation_key,serde_json::to_string(&request.resource_claims).unwrap(),now]).map_err(sql_error)?;
        transaction.execute("update workflow_run set remaining_mutations=remaining_mutations-1,revision=revision+1,updated_unix_ms=?2 where id=?1 and remaining_mutations>0",params![request.run_id.as_str(),now]).map_err(sql_error)?;
        transaction.execute("update workflow_budget set remaining_mutations=remaining_mutations-1,updated_unix_ms=?2 where id=(select budget_id from workflow_run where id=?1) and remaining_mutations>0",params![request.run_id.as_str(),now]).map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(EffectIntent {
            id,
            run_id: request.run_id.clone(),
            step_id: request.step_id.clone(),
            attempt_id: request.lease.attempt_id.clone(),
            kind: request.kind.clone(),
            state: EffectState::Prepared,
            target: request.target.clone(),
            expected_pre_state: request.expected_pre_state.clone(),
            desired_post_state: request.desired_post_state.clone(),
            exact_head: request.exact_head.clone(),
            input_revisions: request.input_revisions.clone(),
            gate_requirements: request.gate_requirements.clone(),
            policy_revisions: request.policy_revisions.clone(),
            authority_grant_id: request.authority_grant_id.clone(),
            reconciliation_key: request.reconciliation_key.clone(),
            resource_claims: request.resource_claims.clone(),
            dispatch_generation: 0,
        })
    }

    fn mark_dispatched(&self, intent: &EffectIntent, lease: &AttemptLease) -> Result<(), String> {
        let conn = self.ledger.connection()?;
        validate_current_authority(&conn, intent, lease)?;
        let changed=conn.execute("update effect_intent set state='dispatched',dispatch_generation=dispatch_generation+1,updated_unix_ms=?2 where id=?1 and state in ('prepared','not_applied')",params![intent.id.as_str(),now_ms()]).map_err(sql_error)?;
        if changed == 0 {
            return Err("effect is not dispatchable from its current state".to_string());
        }
        Ok(())
    }

    fn record_result(
        &self,
        id: &EffectIntentId,
        result: &ReconciliationResult,
        lease: Option<&AttemptLease>,
        expected_dispatch_generation: u32,
    ) -> Result<(), String> {
        let conn = self.ledger.connection()?;
        if let Some(lease) = lease {
            let valid:bool=conn.query_row("select exists(select 1 from attempt_lease where attempt_id=?1 and worker_id=?2 and fencing_token=?3 and expires_unix_ms>?4)",params![lease.attempt_id.as_str(),lease.worker_id,lease.fencing_token,now_ms()],|row|row.get(0)).map_err(sql_error)?;
            if !valid {
                conn.execute("update effect_intent set state='indeterminate',result_json=?2,updated_unix_ms=?3 where id=?1 and dispatch_generation=?4 and state not in ('applied','externally_satisfied','diverged','failed')",params![id.as_str(),serde_json::json!({"reason":"lease lost before result commit"}).to_string(),now_ms(),expected_dispatch_generation]).map_err(sql_error)?;
                return Err(
                    "lease lost after effect dispatch; reconciliation is required".to_string(),
                );
            }
        }
        let changed = conn.execute(
            "update effect_intent set state=?2,result_json=?3,updated_unix_ms=?4 where id=?1 and dispatch_generation=?5 and state not in ('applied','externally_satisfied','diverged','failed')",
            params![
                id.as_str(),
                result.state().label(),
                serde_json::to_string(result).map_err(|error| error.to_string())?,
                now_ms(),
                expected_dispatch_generation,
            ],
        )
        .map_err(sql_error)?;
        if changed != 1 {
            return Err("effect state changed while reconciliation was in progress".to_string());
        }
        Ok(())
    }

    fn load(&self, id: &EffectIntentId) -> Result<EffectIntent, String> {
        let conn = self.ledger.connection()?;
        load_intent(&conn, id)
    }
}

fn validate_request(conn: &rusqlite::Connection, request: &DispatchEffect) -> Result<(), String> {
    let lease_valid:bool=conn.query_row("select exists(select 1 from attempt_lease where attempt_id=?1 and worker_id=?2 and target_id=?3 and fencing_token=?4 and expires_unix_ms>?5)",params![request.lease.attempt_id.as_str(),request.lease.worker_id,request.lease.target_id,request.lease.fencing_token,now_ms()],|row|row.get(0)).map_err(sql_error)?;
    if !lease_valid {
        return Err("stale Attempt cannot prepare an effect".to_string());
    }
    let attempt_matches: bool = conn.query_row("select exists(select 1 from step_attempt where id=?1 and run_id=?2 and step_id=?3 and target_id=?4)",params![request.lease.attempt_id.as_str(),request.run_id.as_str(),request.step_id.as_str(),request.lease.target_id],|row|row.get(0)).map_err(sql_error)?;
    if !attempt_matches {
        return Err(
            "Attempt lease does not belong to the effect Run, Step, and target".to_string(),
        );
    }
    if let Some(effect_workspace) = request
        .target
        .get("workspace_id")
        .and_then(serde_json::Value::as_str)
    {
        let attempt_workspace: Option<String> = conn
            .query_row(
                "select workspace_id from step_attempt where id=?1",
                [request.lease.attempt_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if attempt_workspace.as_deref() != Some(effect_workspace) {
            return Err("effect target workspace does not match the Attempt workspace".to_string());
        }
    }
    let grant: Option<(String, String, Option<i64>)> = conn.query_row("select g.capabilities_json,g.target_scope_json,g.expires_unix_ms from authority_grant g join workflow_run r on r.authority_grant_id=g.id where r.id=?1 and g.id=?2",params![request.run_id.as_str(),request.authority_grant_id.as_str()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional().map_err(sql_error)?;
    let (capabilities_json, target_scope_json, expires_unix_ms) =
        grant.ok_or_else(|| "Authority Grant is stale or mismatched".to_string())?;
    if expires_unix_ms.is_some_and(|expires| expires <= now_ms()) {
        return Err("Authority Grant expired".to_string());
    }
    let capabilities: std::collections::BTreeSet<Capability> =
        serde_json::from_str(&capabilities_json).map_err(|error| error.to_string())?;
    let required = capability_for_effect(&request.kind)?;
    if !capabilities.contains(&required) {
        return Err(format!("Authority Grant does not include {required:?}"));
    }
    let target_scope: std::collections::BTreeSet<String> =
        serde_json::from_str(&target_scope_json).map_err(|error| error.to_string())?;
    if !target_scope.contains(&request.lease.target_id) {
        return Err("Authority Grant does not include the selected target".to_string());
    }
    let step_capabilities: String = conn
        .query_row(
            "select capabilities_json from workflow_step where id=?1 and run_id=?2",
            params![request.step_id.as_str(), request.run_id.as_str()],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    let step_capabilities: std::collections::BTreeSet<Capability> =
        serde_json::from_str(&step_capabilities).map_err(|error| error.to_string())?;
    if !step_capabilities.contains(&required) {
        return Err(format!("Step does not declare {required:?}"));
    }
    for artifact_revision in &request.input_revisions {
        let (artifact_id, revision) = artifact_revision
            .rsplit_once('@')
            .ok_or_else(|| format!("invalid Artifact revision '{artifact_revision}'"))?;
        let revision = revision
            .parse::<u32>()
            .map_err(|_| format!("invalid Artifact revision '{artifact_revision}'"))?;
        let exists: bool = conn
            .query_row(
                "select exists(select 1 from artifact where id=?1 and revision=?2 and run_id=?3)",
                params![artifact_id, revision, request.run_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if !exists {
            return Err(format!(
                "Artifact revision '{artifact_revision}' is not bound to this Run"
            ));
        }
    }
    let mut persisted_inputs = {
        let mut statement=conn.prepare("select i.artifact_id || '@' || i.artifact_revision from attempt_input i where i.attempt_id=?1 order by i.port").map_err(sql_error)?;
        statement
            .query_map([request.lease.attempt_id.as_str()], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?
    };
    let mut requested_inputs = request.input_revisions.clone();
    persisted_inputs.sort();
    requested_inputs.sort();
    if persisted_inputs != requested_inputs {
        return Err(
            "effect Artifact revisions do not exactly match the Attempt inputs".to_string(),
        );
    }
    let persisted_claims = load_claims(conn, &request.lease.attempt_id)?;
    if persisted_claims != request.resource_claims {
        return Err("effect resource claims do not exactly match the Attempt claims".to_string());
    }
    for claim in &request.resource_claims {
        if let Some(expected) = claim.expected_generation {
            let current: Option<i64> = conn
                .query_row(
                    "select generation from resource_generation where resource_key=?1",
                    [&claim.key],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            if current != Some(expected) {
                return Err(format!(
                    "resource '{}' is missing or no longer at generation {expected}",
                    claim.key
                ));
            }
        }
    }
    let dependency_ids: Vec<String> = conn
        .query_row(
            "select dependencies_json from workflow_step where id=?1 and run_id=?2",
            params![request.step_id.as_str(), request.run_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .map_err(sql_error)
        .and_then(|value| serde_json::from_str(&value).map_err(|error| error.to_string()))?;
    let mut required_gates = std::collections::BTreeSet::new();
    let mut pending = std::collections::VecDeque::from(dependency_ids);
    let mut visited = std::collections::BTreeSet::new();
    while let Some(definition_id) = pending.pop_front() {
        if !visited.insert(definition_id.clone()) {
            continue;
        }
        let dependency: Option<(String, String)> = conn
            .query_row(
                "select id,dependencies_json from workflow_step where run_id=?1 and definition_step_id=?2",
                params![request.run_id.as_str(), definition_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_error)?;
        let Some((step_id, dependencies_json)) = dependency else {
            continue;
        };
        let is_gate: bool = conn
            .query_row(
                "select class='gate' from workflow_step where id=?1",
                [&step_id],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if is_gate {
            required_gates.insert(step_id);
        }
        let ancestors: Vec<String> =
            serde_json::from_str(&dependencies_json).map_err(|error| error.to_string())?;
        pending.extend(ancestors);
    }
    let mut supplied_gates = std::collections::BTreeSet::new();
    let effect_subject = request
        .desired_post_state
        .get("subject_digest")
        .and_then(serde_json::Value::as_str)
        .or(request.exact_head.as_deref());
    for gate in &request.gate_requirements {
        if effect_subject != Some(gate.subject_digest.as_str()) {
            return Err(
                "Gate subject digest does not match the protected effect subject".to_string(),
            );
        }
        let gate_step: Option<String> = conn.query_row("select current.step_id from gate_result current join step_attempt current_attempt on current_attempt.id=current.attempt_id where current.id=?1 and current.run_id=?2 and current.status='satisfied' and current.evidence_quality='current' and current.subject_digest=?3 and current.subject_generation=?4 and current.policy_revision=?5 and (current.expires_unix_ms is null or current.expires_unix_ms>?6) and not exists(select 1 from gate_result newer join step_attempt newer_attempt on newer_attempt.id=newer.attempt_id where newer.step_id=current.step_id and newer_attempt.ordinal>current_attempt.ordinal)",params![gate.gate_result_id,request.run_id.as_str(),gate.subject_digest,gate.subject_generation,gate.policy_revision,now_ms()],|row|row.get(0)).optional().map_err(sql_error)?;
        let Some(gate_step) = gate_step else {
            return Err(format!(
                "Gate result '{}' is stale, unavailable, or mismatched",
                gate.gate_result_id
            ));
        };
        supplied_gates.insert(gate_step);
    }
    if supplied_gates.len() != request.gate_requirements.len() || supplied_gates != required_gates {
        return Err(
            "effect Gate requirements do not exactly match the Step's Gate dependencies"
                .to_string(),
        );
    }
    let mut expected_policies = request
        .gate_requirements
        .iter()
        .map(|gate| gate.policy_revision.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let current_admission: Option<String> = conn
        .query_row(
            "select policy_revision from admission_decision where run_id=?1 and outcome='allowed' and (expires_unix_ms is null or expires_unix_ms>?2) order by created_unix_ms desc,id desc limit 1",
            params![request.run_id.as_str(), now_ms()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    expected_policies.extend(current_admission);
    let supplied_policies = request
        .policy_revisions
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    if supplied_policies.len() != request.policy_revisions.len()
        || supplied_policies != expected_policies
    {
        return Err(
            "effect policy revisions do not exactly match current Gate and admission policy revisions"
                .to_string(),
        );
    }
    Ok(())
}

fn load_intent(conn: &rusqlite::Connection, id: &EffectIntentId) -> Result<EffectIntent, String> {
    conn.query_row("select run_id,step_id,attempt_id,kind,state,target_json,expected_pre_state_json,desired_post_state_json,exact_head,input_revisions_json,gate_requirements_json,policy_revisions_json,authority_grant_id,reconciliation_key,resource_claims_json,dispatch_generation from effect_intent where id=?1",[id.as_str()],|row|Ok(EffectIntent{id:id.clone(),run_id:RunId(row.get(0)?),step_id:StepId(row.get(1)?),attempt_id:crate::run::AttemptId(row.get(2)?),kind:row.get(3)?,state:parse_enum(&row.get::<_,String>(4)?).map_err(json_sql_error)?,target:serde_json::from_str(&row.get::<_,String>(5)?).map_err(json_sql_error)?,expected_pre_state:serde_json::from_str(&row.get::<_,String>(6)?).map_err(json_sql_error)?,desired_post_state:serde_json::from_str(&row.get::<_,String>(7)?).map_err(json_sql_error)?,exact_head:row.get(8)?,input_revisions:serde_json::from_str(&row.get::<_,String>(9)?).map_err(json_sql_error)?,gate_requirements:serde_json::from_str(&row.get::<_,String>(10)?).map_err(json_sql_error)?,policy_revisions:serde_json::from_str(&row.get::<_,String>(11)?).map_err(json_sql_error)?,authority_grant_id:AuthorityGrantId(row.get(12)?),reconciliation_key:row.get(13)?,resource_claims:serde_json::from_str(&row.get::<_,String>(14)?).map_err(json_sql_error)?,dispatch_generation:row.get(15)?})).optional().map_err(sql_error)?.ok_or_else(||format!("effect intent '{}' was not found",id.as_str()))
}

fn request_matches_intent(request: &DispatchEffect, intent: &EffectIntent) -> bool {
    intent.run_id == request.run_id
        && intent.step_id == request.step_id
        && intent.attempt_id == request.lease.attempt_id
        && intent.kind == request.kind
        && intent.target == request.target
        && intent.expected_pre_state == request.expected_pre_state
        && intent.desired_post_state == request.desired_post_state
        && intent.exact_head == request.exact_head
        && intent.input_revisions == request.input_revisions
        && intent.gate_requirements == request.gate_requirements
        && intent.policy_revisions == request.policy_revisions
        && intent.authority_grant_id == request.authority_grant_id
        && intent.reconciliation_key == request.reconciliation_key
        && intent.resource_claims == request.resource_claims
}

fn capability_for_effect(kind: &str) -> Result<Capability, String> {
    match kind {
        "git_commit" => Ok(Capability::GitCommit),
        "git_ref" => Ok(Capability::GitRefMutation),
        "push" => Ok(Capability::GitPush),
        "merge" => Ok(Capability::Merge),
        "worktrunk" | "cleanup" => Ok(Capability::WorktrunkLifecycle),
        "provider" | "create_change_request" | "resolve_review_thread" => {
            Ok(Capability::ProviderWrite)
        }
        "secret_delegation" => Ok(Capability::SecretUse),
        "child_run" => Ok(Capability::ChildWorkflowCreate),
        other => Err(format!("unknown protected effect kind '{other}'")),
    }
}

pub(crate) struct ChildIntentRequest<'a> {
    pub run_id: &'a RunId,
    pub step_id: &'a StepId,
    pub authority_grant_id: &'a AuthorityGrantId,
    pub reconciliation_key: &'a str,
    pub child_snapshot_digest: &'a str,
    pub input_digest: &'a str,
}

pub(crate) fn prepare_child_intent(
    conn: &rusqlite::Connection,
    request: ChildIntentRequest<'_>,
) -> Result<EffectIntentId, String> {
    let attempt_id = crate::run::AttemptId(random_id(conn)?);
    let intent_id = EffectIntentId(random_id(conn)?);
    let now = now_ms();
    conn.execute("insert into step_attempt(id,run_id,step_id,ordinal,state,input_digest,implementation_id,implementation_revision,requested_claims_json,created_unix_ms,updated_unix_ms) select ?1,?2,?3,attempt_count+1,'waiting',?4,implementation_id,implementation_revision,'[]',?5,?5 from workflow_step where id=?3 and run_id=?2",params![attempt_id.as_str(),request.run_id.as_str(),request.step_id.as_str(),request.input_digest,now]).map_err(sql_error)?;
    conn.execute(
        "update workflow_step set attempt_count=attempt_count+1,updated_unix_ms=?2 where id=?1",
        params![request.step_id.as_str(), now],
    )
    .map_err(sql_error)?;
    conn.execute("insert into effect_intent(id,run_id,step_id,attempt_id,kind,state,target_json,expected_pre_state_json,desired_post_state_json,input_revisions_json,gate_requirements_json,policy_revisions_json,authority_grant_id,reconciliation_key,resource_claims_json,dispatch_generation,created_unix_ms,updated_unix_ms) values(?1,?2,?3,?4,'child_run','prepared',?5,'{}',?6,'[]','[]','[]',?7,?8,'[]',0,?9,?9)",params![intent_id.as_str(),request.run_id.as_str(),request.step_id.as_str(),attempt_id.as_str(),serde_json::json!({"snapshot_digest":request.child_snapshot_digest,"input_digest":request.input_digest}).to_string(),serde_json::json!({"child_run_created":true}).to_string(),request.authority_grant_id.as_str(),request.reconciliation_key,now]).map_err(sql_error)?;
    Ok(intent_id)
}

pub(crate) fn complete_child_intent(
    conn: &rusqlite::Connection,
    intent_id: &EffectIntentId,
    child_run_id: &RunId,
) -> Result<(), String> {
    let now = now_ms();
    conn.execute("update effect_intent set state='applied',result_json=?2,dispatch_generation=1,updated_unix_ms=?3 where id=?1 and state='prepared'",params![intent_id.as_str(),serde_json::json!({"outcome":"applied","child_run_id":child_run_id}).to_string(),now]).map_err(sql_error)?;
    conn.execute("update step_attempt set state='waiting',terminal_reason='child created',updated_unix_ms=?2 where id=(select attempt_id from effect_intent where id=?1)",params![intent_id.as_str(),now]).map_err(sql_error)?;
    Ok(())
}

fn validate_current_authority(
    conn: &rusqlite::Connection,
    intent: &EffectIntent,
    lease: &AttemptLease,
) -> Result<(), String> {
    if intent.attempt_id != lease.attempt_id {
        return Err("Attempt lease does not belong to the persisted effect intent".to_string());
    }
    let request = DispatchEffect {
        run_id: intent.run_id.clone(),
        step_id: intent.step_id.clone(),
        lease: lease.clone(),
        kind: intent.kind.clone(),
        target: intent.target.clone(),
        expected_pre_state: intent.expected_pre_state.clone(),
        desired_post_state: intent.desired_post_state.clone(),
        exact_head: intent.exact_head.clone(),
        input_revisions: intent.input_revisions.clone(),
        gate_requirements: intent.gate_requirements.clone(),
        policy_revisions: intent.policy_revisions.clone(),
        authority_grant_id: intent.authority_grant_id.clone(),
        reconciliation_key: intent.reconciliation_key.clone(),
        resource_claims: intent.resource_claims.clone(),
    };
    validate_request(conn, &request)
}

fn load_claims(
    conn: &rusqlite::Connection,
    attempt: &crate::run::AttemptId,
) -> Result<Vec<ResourceClaimSpec>, String> {
    let mut statement=conn.prepare("select resource_key,access,expected_generation from resource_claim where attempt_id=?1 order by resource_key").map_err(sql_error)?;
    statement
        .query_map([attempt.as_str()], |row| {
            Ok(ResourceClaimSpec {
                key: row.get(0)?,
                access: parse_enum(&row.get::<_, String>(1)?).map_err(json_sql_error)?,
                expected_generation: row.get(2)?,
            })
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)
}
fn parse_enum<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(&format!("\"{value}\""))
}
fn json_sql_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}
fn sql_error(error: rusqlite::Error) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::{BoundArtifact, ClaimAccess, Coordinator, PrepareAttempt};
    use crate::definition::{Capability, DefinitionCatalog};
    use crate::run::{ArtifactInput, Sensitivity, StartRun, TrustClass};
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeAdapter {
        dispatches: AtomicUsize,
        result: ReconciliationResult,
    }
    impl EffectAdapter for FakeAdapter {
        fn dispatch(&self, _: &EffectIntent) -> Result<ReconciliationResult, String> {
            self.dispatches.fetch_add(1, Ordering::SeqCst);
            Ok(self.result.clone())
        }
        fn reconcile(&self, _: &EffectIntent) -> Result<ReconciliationResult, String> {
            Ok(self.result.clone())
        }
    }

    fn setup(
        mutations: u32,
    ) -> (
        RunLedger,
        AttemptLease,
        RunId,
        StepId,
        AuthorityGrantId,
        Vec<ResourceClaimSpec>,
        String,
        std::path::PathBuf,
    ) {
        let path = std::env::temp_dir().join(format!(
            "prism-effect-{}-{}-{:?}.db",
            std::process::id(),
            now_ms(),
            std::thread::current().id()
        ));
        let ledger = RunLedger::open(path.clone()).unwrap();
        let mut snapshot = DefinitionCatalog::discover(None)
            .resolve("builtin:action")
            .unwrap();
        snapshot.content.budgets.max_mutations = mutations;
        snapshot.content.capabilities.insert(Capability::GitPush);
        let run = ledger
            .start(StartRun {
                snapshot,
                repository_id: None,
                inputs: vec![ArtifactInput {
                    name: "task".into(),
                    artifact_type: "builtin:task@1".into(),
                    payload: serde_json::json!({}),
                    trust: TrustClass::Trusted,
                    sensitivity: Sensitivity::Internal,
                }],
                idempotency_key: None,
                actor: "test".into(),
                actor_capabilities: BTreeSet::from([Capability::GitPush]),
            })
            .unwrap();
        let projection = ledger.inspect(&run.run_id).unwrap();
        let step = projection.steps[0].id.clone();
        let conn = ledger.connection().unwrap();
        conn.execute(
            "update workflow_step set state='runnable',capabilities_json='[\"git_push\"]' where id=?1",
            [step.as_str()],
        )
        .unwrap();
        let grant = conn
            .query_row(
                "select authority_grant_id from workflow_run where id=?1",
                [run.run_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        conn.execute("insert into admission_decision(id,run_id,policy_revision,observation_revision,capabilities_json,outcome,created_unix_ms) values('admission',?1,'policy@1','observation@1','[]','allowed',?2)",params![run.run_id.as_str(),now_ms()]).unwrap();
        let (artifact_input, artifact_revision) = conn
            .query_row(
                "select id,revision,digest,artifact_type,id || '@' || revision,payload_inline from artifact where run_id=?1 limit 1",
                [run.run_id.as_str()],
                |row| Ok((BoundArtifact{port:"task".to_string(),artifact:crate::run::ArtifactRef{id:crate::run::ArtifactId(row.get(0)?),revision:row.get(1)?,digest:row.get(2)?,artifact_type:row.get(3)?},payload:serde_json::from_slice(&row.get::<_,Vec<u8>>(5)?).unwrap()},row.get::<_,String>(4)?)),
            )
            .unwrap();
        drop(conn);
        let claims = vec![ResourceClaimSpec {
            key: "git-ref:refs/heads/main".into(),
            access: ClaimAccess::Write,
            expected_generation: Some(1),
        }];
        ledger.connection().unwrap().execute("insert into resource_generation(resource_key,generation,updated_unix_ms) values(?1,1,?2)",params![claims[0].key,now_ms()]).unwrap();
        let coordinator = Coordinator::new(ledger.clone());
        coordinator
            .prepare(PrepareAttempt {
                run_id: run.run_id.clone(),
                step_id: step.clone(),
                input_digest: run.input_digest,
                target_id: "local".into(),
                workspace: None,
                resource_claims: claims.clone(),
                input_artifacts: vec![artifact_input],
            })
            .unwrap();
        let lease = coordinator
            .claim("worker", &BTreeSet::from(["local".to_string()]))
            .unwrap()
            .unwrap()
            .lease;
        (
            ledger,
            lease,
            run.run_id,
            step,
            AuthorityGrantId(grant),
            claims,
            artifact_revision,
            path,
        )
    }
    fn request(
        lease: AttemptLease,
        run: RunId,
        step: StepId,
        grant: AuthorityGrantId,
        claims: Vec<ResourceClaimSpec>,
        artifact_revision: String,
    ) -> DispatchEffect {
        DispatchEffect {
            run_id: run,
            step_id: step,
            lease,
            kind: "push".into(),
            target: serde_json::json!({"ref":"refs/heads/main"}),
            expected_pre_state: serde_json::json!({"head":"a"}),
            desired_post_state: serde_json::json!({"head":"b"}),
            exact_head: Some("b".into()),
            input_revisions: vec![artifact_revision],
            gate_requirements: vec![],
            policy_revisions: vec!["policy@1".into()],
            authority_grant_id: grant,
            reconciliation_key: "push:main:b".into(),
            resource_claims: claims,
        }
    }

    #[test]
    fn intent_is_persisted_before_single_dispatch() {
        let (ledger, lease, run, step, grant, claims, artifact, path) = setup(1);
        let adapter = Arc::new(FakeAdapter {
            dispatches: AtomicUsize::new(0),
            result: ReconciliationResult::Applied {
                evidence: serde_json::json!({"head":"b"}),
            },
        });
        let mut broker = EffectBroker::new(ledger);
        broker.register("push", adapter.clone()).unwrap();
        let intent = broker
            .dispatch(request(lease, run, step, grant, claims, artifact))
            .unwrap();
        assert_eq!(intent.state, EffectState::Applied);
        assert_eq!(adapter.dispatches.load(Ordering::SeqCst), 1);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn lost_response_reconciles_without_second_dispatch() {
        let (ledger, lease, run, step, grant, claims, artifact, path) = setup(1);
        struct LostThenApplied {
            dispatches: AtomicUsize,
        }
        impl EffectAdapter for LostThenApplied {
            fn dispatch(&self, _: &EffectIntent) -> Result<ReconciliationResult, String> {
                self.dispatches.fetch_add(1, Ordering::SeqCst);
                Err("response lost".into())
            }
            fn reconcile(&self, _: &EffectIntent) -> Result<ReconciliationResult, String> {
                Ok(ReconciliationResult::Applied {
                    evidence: serde_json::json!({"head":"b"}),
                })
            }
        }
        let adapter = Arc::new(LostThenApplied {
            dispatches: AtomicUsize::new(0),
        });
        let mut broker = EffectBroker::new(ledger);
        broker.register("push", adapter.clone()).unwrap();
        let intent = broker
            .dispatch(request(lease, run, step, grant, claims, artifact))
            .unwrap();
        assert_eq!(intent.state, EffectState::Indeterminate);
        assert_eq!(
            broker.reconcile(&intent.id).unwrap().state,
            EffectState::Applied
        );
        assert_eq!(adapter.dispatches.load(Ordering::SeqCst), 1);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn prepared_intent_reconciles_existing_desired_state_as_external() {
        let (ledger, lease, run, step, grant, claims, artifact, path) = setup(1);
        let adapter = Arc::new(FakeAdapter {
            dispatches: AtomicUsize::new(0),
            result: ReconciliationResult::Applied {
                evidence: serde_json::json!({"head":"b"}),
            },
        });
        let mut broker = EffectBroker::new(ledger.clone());
        broker.register("push", adapter.clone()).unwrap();
        let intent = broker
            .prepare(&request(lease, run.clone(), step, grant, claims, artifact))
            .unwrap();

        let reconciled = broker.reconcile(&intent.id).unwrap();

        assert_eq!(reconciled.state, EffectState::ExternallySatisfied);
        assert_eq!(adapter.dispatches.load(Ordering::SeqCst), 0);
        let projection = ledger.inspect(&run).unwrap();
        assert_eq!(projection.effects.len(), 1);
        assert_eq!(projection.effects[0].state, "externally_satisfied");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn mutation_without_budget_fails_before_adapter() {
        let (ledger, lease, run, step, grant, claims, artifact, path) = setup(0);
        let adapter = Arc::new(FakeAdapter {
            dispatches: AtomicUsize::new(0),
            result: ReconciliationResult::Applied {
                evidence: serde_json::json!({}),
            },
        });
        let mut broker = EffectBroker::new(ledger);
        broker.register("push", adapter.clone()).unwrap();
        assert!(
            broker
                .dispatch(request(lease, run, step, grant, claims, artifact))
                .unwrap_err()
                .contains("budget")
        );
        assert_eq!(adapter.dispatches.load(Ordering::SeqCst), 0);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn lease_cannot_authorize_an_effect_for_another_run() {
        let (ledger, lease, _run, step, grant, claims, artifact, path) = setup(1);
        let adapter = Arc::new(FakeAdapter {
            dispatches: AtomicUsize::new(0),
            result: ReconciliationResult::Applied {
                evidence: serde_json::json!({}),
            },
        });
        let mut broker = EffectBroker::new(ledger);
        broker.register("push", adapter.clone()).unwrap();
        let error = broker
            .dispatch(request(
                lease,
                RunId("another-run".into()),
                step,
                grant,
                claims,
                artifact,
            ))
            .unwrap_err();
        assert!(error.contains("does not belong"));
        assert_eq!(adapter.dispatches.load(Ordering::SeqCst), 0);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reconciliation_key_rejects_a_different_request() {
        let (ledger, lease, run, step, grant, claims, artifact, path) = setup(2);
        let adapter = Arc::new(FakeAdapter {
            dispatches: AtomicUsize::new(0),
            result: ReconciliationResult::Applied {
                evidence: serde_json::json!({}),
            },
        });
        let mut broker = EffectBroker::new(ledger);
        broker.register("push", adapter.clone()).unwrap();
        let first = request(
            lease.clone(),
            run.clone(),
            step.clone(),
            grant.clone(),
            claims.clone(),
            artifact.clone(),
        );
        broker.dispatch(first).unwrap();
        let mut collision = request(lease, run, step, grant, claims, artifact);
        collision.desired_post_state = serde_json::json!({"head":"different"});
        assert!(broker.dispatch(collision).unwrap_err().contains("collides"));
        assert_eq!(adapter.dispatches.load(Ordering::SeqCst), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn protected_effect_requires_exact_current_policy_set() {
        let (ledger, lease, run, step, grant, claims, artifact, path) = setup(1);
        let adapter = Arc::new(FakeAdapter {
            dispatches: AtomicUsize::new(0),
            result: ReconciliationResult::Applied {
                evidence: serde_json::json!({}),
            },
        });
        let mut broker = EffectBroker::new(ledger);
        broker.register("push", adapter.clone()).unwrap();
        let mut effect = request(lease, run, step, grant, claims, artifact);
        effect.policy_revisions.clear();

        let error = broker.dispatch(effect).unwrap_err();

        assert!(error.contains("policy revisions"));
        assert_eq!(adapter.dispatches.load(Ordering::SeqCst), 0);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn protected_effect_requires_every_gate_dependency() {
        let (ledger, lease, run, step, grant, claims, artifact, path) = setup(1);
        let conn = ledger.connection().unwrap();
        conn.execute("insert into workflow_step(id,run_id,definition_step_id,class,implementation_id,implementation_revision,dependencies_json,input_bindings_json,outputs_json,capabilities_json,state,attempt_count,created_unix_ms,updated_unix_ms) values('gate-step',?1,'required-gate','gate','builtin:gate',1,'[]','{}','{}','[]','completed',1,?2,?2)",params![run.as_str(),now_ms()]).unwrap();
        conn.execute(
            "update workflow_step set dependencies_json='[\"required-gate\"]' where id=?1",
            [step.as_str()],
        )
        .unwrap();
        drop(conn);
        let adapter = Arc::new(FakeAdapter {
            dispatches: AtomicUsize::new(0),
            result: ReconciliationResult::Applied {
                evidence: serde_json::json!({}),
            },
        });
        let mut broker = EffectBroker::new(ledger);
        broker.register("push", adapter.clone()).unwrap();
        let error = broker
            .dispatch(request(lease, run, step, grant, claims, artifact))
            .unwrap_err();
        assert!(error.contains("Gate requirements"));
        assert_eq!(adapter.dispatches.load(Ordering::SeqCst), 0);
        std::fs::remove_file(path).unwrap();
    }
}
