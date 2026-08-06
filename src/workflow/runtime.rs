use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rusqlite::{OptionalExtension, params};

use crate::config::Config;
use crate::coordinator::{
    AttemptResult, ClaimAccess, ClaimedAttempt, Coordinator, ResourceClaimSpec,
};
use crate::definition::{
    DefinitionSnapshot, EffectClass, ImplementationDescriptor, PrimitiveClass, SnapshotContent,
    TargetRequirement,
};
use crate::effect::adapters::{GitEffectAdapter, ProviderEffectAdapter, WorktrunkEffectAdapter};
use crate::effect::{DispatchEffect, EffectBroker, EffectState, GateRequirement};
use crate::gate::GateImplementation;
use crate::implementation::{
    AttemptExecutor, CodingAgentImplementation, CodingAgentKind, HarnessAgentImplementation,
    ImplementationRegistry, PlanPhaseImplementation, PlanProducerImplementation,
    PlanReviewImplementation, ProviderClassificationImplementation, StepImplementation,
    StructuredCommandImplementation,
};
use crate::operations::{ChildCall, EvidenceQuality, GateEvidence, GateStatus, WorkflowOperations};
use crate::repo::Repository;
use crate::run::{
    ApprovalMode, ArtifactInput, RunId, RunLedger, RunState, Sensitivity, StepId, StepState,
    TrustClass, now_ms,
};
use crate::target::LocalTarget;

/// Production-facing orchestration module for generalized Workflow Runs.
///
/// One tick materializes newly runnable primitive work and claims at most one
/// Attempt. Process execution remains outside the scheduling transaction.
#[derive(Clone)]
pub(crate) struct WorkflowRuntime {
    ledger: RunLedger,
    operations: WorkflowOperations,
    coordinator: Coordinator,
}

impl WorkflowRuntime {
    pub(crate) fn user() -> Result<Self, String> {
        Ok(Self::new(RunLedger::user()?))
    }

    pub(crate) fn new(ledger: RunLedger) -> Self {
        Self {
            operations: WorkflowOperations::new(ledger.clone()),
            coordinator: Coordinator::new(ledger.clone()),
            ledger,
        }
    }

    pub(crate) fn claim_next(&self, worker_id: &str) -> Result<Option<ClaimedAttempt>, String> {
        self.materialize_runnable()?;
        self.coordinator
            .claim(worker_id, &BTreeSet::from(["local".to_string()]))
    }

    pub(crate) fn execute(&self, claim: ClaimedAttempt) -> Result<(), String> {
        let descriptor = self.implementation_descriptor(&claim)?;
        if descriptor.id == "builtin:promote-workspace@1" {
            return self.promote_workspace(claim);
        }
        if descriptor.id == "builtin:commit@1" {
            return self.commit_workspace(claim);
        }
        if descriptor.id == "builtin:create-change-request@1" {
            return self.create_change_request(claim);
        }
        if descriptor.id == "builtin:merge@1" {
            return self.merge_change_request(claim);
        }
        if descriptor.id == "builtin:cleanup@1" {
            return self.cleanup_workspace(claim);
        }
        let mut registry = ImplementationRegistry::default();
        let implementation: Arc<dyn StepImplementation> = match descriptor.id.as_str() {
            "builtin:command@1" => {
                if claim.envelope.settings.command.is_empty() {
                    return self.fail_claim(
                        claim,
                        "Command Step has no structured settings.command argv",
                    );
                }
                let target = self.local_target(&claim)?;
                Arc::new(StructuredCommandImplementation::new(
                    descriptor,
                    target,
                    self.coordinator.clone(),
                    claim.envelope.settings.command.clone(),
                )?)
            }
            "builtin:agent@1" => Arc::new(self.harness_implementation(&claim, descriptor)?),
            "builtin:create-plan@1" => Arc::new(PlanProducerImplementation::new(descriptor)),
            "builtin:classify-provider-item@1" => {
                Arc::new(ProviderClassificationImplementation::new(descriptor))
            }
            "builtin:review-plan@1" => Arc::new(PlanReviewImplementation::new(descriptor)),
            "builtin:implement-plan-phase@1" => Arc::new(PlanPhaseImplementation::new(
                self.harness_implementation(&claim, descriptor)?,
            )),
            "builtin:implement@1" => Arc::new(CodingAgentImplementation::new(
                self.harness_implementation(&claim, descriptor)?,
                CodingAgentKind::Implement,
            )),
            "builtin:self-review@1" => Arc::new(CodingAgentImplementation::new(
                self.harness_implementation(&claim, descriptor)?,
                CodingAgentKind::SelfReview,
            )),
            "builtin:distinct-model-review@1" => Arc::new(CodingAgentImplementation::new(
                self.harness_implementation(&claim, descriptor)?,
                CodingAgentKind::DistinctModelReview,
            )),
            "builtin:repair@1" => Arc::new(CodingAgentImplementation::new(
                self.harness_implementation(&claim, descriptor)?,
                CodingAgentKind::Repair,
            )),
            _ if descriptor.class == PrimitiveClass::Action
                && !claim.envelope.settings.command.is_empty() =>
            {
                let target = self.local_target(&claim)?;
                Arc::new(StructuredCommandImplementation::new(
                    descriptor,
                    target,
                    self.coordinator.clone(),
                    claim.envelope.settings.command.clone(),
                )?)
            }
            _ => {
                return self.fail_claim(
                    claim,
                    "the pinned Step Implementation is not registered in the production runtime",
                );
            }
        };
        registry.register(implementation)?;
        AttemptExecutor::new(self.coordinator.clone(), Arc::new(registry)).execute(claim)
    }

    fn promote_workspace(&self, claim: ClaimedAttempt) -> Result<(), String> {
        let workspace = claim
            .envelope
            .workspace
            .as_ref()
            .ok_or_else(|| "Workspace promotion requires an Execution Workspace".to_string())?;
        let path = self.ledger.local_workspace_path(&workspace.id)?;
        let repo = Repository { root: path.clone() };
        let config = Arc::new(Config::load(&repo));
        let expected_head = crate::git::current_head_sha(&path, &config)?;
        let candidate = claim
            .envelope
            .inputs
            .iter()
            .find(|input| input.port == "commit")
            .ok_or_else(|| {
                "Workspace promotion requires its exact candidate Commit Artifact".to_string()
            })?;
        let phase_id = candidate
            .payload
            .get("phase_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("plan-phase");
        let harness = candidate
            .payload
            .get("harness")
            .cloned()
            .unwrap_or_default();
        let model = candidate.payload.get("model").cloned().unwrap_or_default();
        let message = format!("Implement plan phase {phase_id}");
        let mut broker = EffectBroker::new(self.ledger.clone());
        broker.register(
            "git_commit",
            Arc::new(GitEffectAdapter::new(
                config.clone(),
                BTreeMap::from([(workspace.id.as_str().to_string(), path.clone())]),
            )?),
        )?;
        let intent = broker.dispatch(DispatchEffect {
            run_id: claim.envelope.run_id.clone(),
            step_id: claim.envelope.step_id.clone(),
            lease: claim.lease.clone(),
            kind: "git_commit".to_string(),
            target: serde_json::json!({
                "workspace_id": workspace.id.as_str(),
                "remote": null,
                "branch": null,
                "message": message,
                "base_head": expected_head,
            }),
            expected_pre_state: serde_json::json!({"head": expected_head}),
            desired_post_state: serde_json::json!({"message": message}),
            exact_head: None,
            input_revisions: claim
                .envelope
                .inputs
                .iter()
                .map(|input| input.artifact.digest.clone())
                .collect(),
            gate_requirements: Vec::new(),
            policy_revisions: Vec::new(),
            authority_grant_id: claim.envelope.authority.id.clone(),
            reconciliation_key: format!("plan-phase-commit:{}", claim.envelope.attempt_id.as_str()),
            resource_claims: claim.envelope.resource_claims.clone(),
        })?;
        if !matches!(
            intent.state,
            EffectState::Applied | EffectState::ExternallySatisfied
        ) {
            if matches!(
                intent.state,
                EffectState::Indeterminate | EffectState::Dispatched
            ) {
                return Err("workspace promotion requires effect reconciliation".to_string());
            }
            return self.fail_claim(
                claim,
                "workspace promotion did not produce the guarded commit",
            );
        }
        let head = crate::git::current_head_sha(&path, &config)?;
        self.coordinator.finish(
            &claim.lease,
            AttemptResult {
                outcome: "promoted".to_string(),
                outputs: vec![ArtifactInput {
                    name: "commit".to_string(),
                    artifact_type: "builtin:commit@1".to_string(),
                    payload: serde_json::json!({
                        "head": head,
                        "parent": expected_head,
                        "phase_id": phase_id,
                        "harness": harness,
                        "model": model,
                    }),
                    trust: TrustClass::DerivedUntrusted,
                    sensitivity: Sensitivity::Internal,
                }],
            },
        )
    }

    fn commit_workspace(&self, claim: ClaimedAttempt) -> Result<(), String> {
        let workspace = claim
            .envelope
            .workspace
            .as_ref()
            .ok_or_else(|| "Commit Action requires an Execution Workspace".to_string())?;
        let path = self.ledger.local_workspace_path(&workspace.id)?;
        let config = Arc::new(Config::load(&Repository { root: path.clone() }));
        let expected_head = crate::git::current_head_sha(&path, &config)?;
        let mut broker = EffectBroker::new(self.ledger.clone());
        broker.register(
            "git_commit",
            Arc::new(GitEffectAdapter::new(
                config.clone(),
                BTreeMap::from([(workspace.id.as_str().to_string(), path.clone())]),
            )?),
        )?;
        let gates = self.gate_requirements(
            &claim.envelope.run_id,
            &["local-verification", "review-policy"],
        )?;
        let policies = self.effect_policy_revisions(&claim.envelope.run_id, &gates)?;
        let subject_digest = claim
            .envelope
            .inputs
            .first()
            .map(|input| input.artifact.digest.clone())
            .ok_or_else(|| "Commit Action requires its exact candidate Artifact".to_string())?;
        let intent = broker.dispatch(DispatchEffect {
            run_id: claim.envelope.run_id.clone(),
            step_id: claim.envelope.step_id.clone(),
            lease: claim.lease.clone(),
            kind: "git_commit".to_string(),
            target: serde_json::json!({
                "workspace_id": workspace.id.as_str(),
                "remote": null,
                "branch": null,
                "message": "Implement workflow task",
                "base_head": expected_head,
            }),
            expected_pre_state: serde_json::json!({"head":expected_head}),
            desired_post_state: serde_json::json!({"message":"Implement workflow task","subject_digest":subject_digest}),
            exact_head: None,
            input_revisions: claim
                .envelope
                .inputs
                .iter()
                .map(|input| input.artifact.digest.clone())
                .collect(),
            gate_requirements: gates,
            policy_revisions: policies,
            authority_grant_id: claim.envelope.authority.id.clone(),
            reconciliation_key: format!("coding-commit:{}", claim.envelope.attempt_id.as_str()),
            resource_claims: claim.envelope.resource_claims.clone(),
        })?;
        if !matches!(
            intent.state,
            EffectState::Applied | EffectState::ExternallySatisfied
        ) {
            return if matches!(
                intent.state,
                EffectState::Dispatched | EffectState::Indeterminate
            ) {
                Err("coding commit requires Effect reconciliation".to_string())
            } else {
                self.fail_claim(claim, "guarded coding commit was not applied")
            };
        }
        let head = crate::git::current_head_sha(&path, &config)?;
        self.coordinator.finish(&claim.lease, AttemptResult {
            outcome: "committed".to_string(),
            outputs: vec![ArtifactInput {
                name: "commit".to_string(),
                artifact_type: "builtin:commit@1".to_string(),
                payload: serde_json::json!({"head":head,"parent":expected_head,"generation":workspace.generation}),
                trust: TrustClass::DerivedUntrusted,
                sensitivity: Sensitivity::Internal,
            }],
        })
    }

    fn create_change_request(&self, claim: ClaimedAttempt) -> Result<(), String> {
        let workspace =
            claim.envelope.workspace.as_ref().ok_or_else(|| {
                "change-request creation requires an Execution Workspace".to_string()
            })?;
        let path = self.ledger.local_workspace_path(&workspace.id)?;
        let config = Arc::new(Config::load(&Repository { root: path.clone() }));
        let branch = crate::git::current_branch_name(&path, &config)?
            .ok_or_else(|| "cannot create a change request from detached HEAD".to_string())?;
        let head = crate::git::current_head_sha(&path, &config)?;
        let commit = claim
            .envelope
            .inputs
            .iter()
            .find(|input| input.port == "commit")
            .ok_or_else(|| {
                "change-request creation requires its exact Commit Artifact".to_string()
            })?;
        if commit
            .payload
            .get("head")
            .and_then(serde_json::Value::as_str)
            != Some(head.as_str())
        {
            return self.fail_claim(claim, "Commit Artifact no longer matches local HEAD");
        }
        let repository_id: String = self
            .ledger
            .connection()?
            .query_row(
                "select repository_id from workflow_run where id=?1",
                [claim.envelope.run_id.as_str()],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        let repository = Repository { root: path.clone() };
        let (origin, upstream) =
            crate::remote::dispatcher::create_change_request_targets(&path, &config)?;
        let target_repository = upstream.unwrap_or(origin);

        let mut broker = EffectBroker::new(self.ledger.clone());
        broker.register(
            "push",
            Arc::new(GitEffectAdapter::new(
                config.clone(),
                BTreeMap::from([(workspace.id.as_str().to_string(), path.clone())]),
            )?),
        )?;
        broker.register(
            "create_change_request",
            Arc::new(ProviderEffectAdapter::new(
                config.clone(),
                BTreeMap::from([(repository_id.clone(), repository)]),
                BTreeMap::from([(workspace.id.as_str().to_string(), path.clone())]),
            )?),
        )?;
        let gates = self.gate_requirements(
            &claim.envelope.run_id,
            &["local-verification", "review-policy"],
        )?;
        let policies = self.effect_policy_revisions(&claim.envelope.run_id, &gates)?;
        let subject_digest = gates
            .first()
            .map(|gate| gate.subject_digest.clone())
            .ok_or_else(|| "change-request creation requires exact Gate evidence".to_string())?;
        let remote_head =
            crate::git::push_remote_branch_head_sha(&path, "origin", &branch, &config)?;
        let push = broker.dispatch(DispatchEffect {
            run_id: claim.envelope.run_id.clone(),
            step_id: claim.envelope.step_id.clone(),
            lease: claim.lease.clone(),
            kind: "push".to_string(),
            target: serde_json::json!({
                "workspace_id":workspace.id.as_str(),
                "remote":"origin",
                "branch":branch,
                "message":null,
                "base_head":null,
            }),
            expected_pre_state: serde_json::json!({"head":remote_head}),
            desired_post_state: serde_json::json!({"head":head,"subject_digest":subject_digest}),
            exact_head: Some(head.clone()),
            input_revisions: vec![commit.artifact.digest.clone()],
            gate_requirements: gates.clone(),
            policy_revisions: policies.clone(),
            authority_grant_id: claim.envelope.authority.id.clone(),
            reconciliation_key: format!("coding-push:{branch}:{head}"),
            resource_claims: claim.envelope.resource_claims.clone(),
        })?;
        if !matches!(
            push.state,
            EffectState::Applied | EffectState::ExternallySatisfied
        ) {
            return if matches!(
                push.state,
                EffectState::Dispatched | EffectState::Indeterminate
            ) {
                Err("initial coding push requires Effect reconciliation".to_string())
            } else {
                self.fail_claim(claim, "guarded initial coding push was not applied")
            };
        }
        let intent = broker.dispatch(DispatchEffect {
            run_id: claim.envelope.run_id.clone(),
            step_id: claim.envelope.step_id.clone(),
            lease: claim.lease.clone(),
            kind: "create_change_request".to_string(),
            target: serde_json::json!({
                "workspace_id":workspace.id.as_str(),
                "repository_id":repository_id,
                "branch":branch,
                "expected_head":head,
                "target_provider":target_repository.provider(),
                "target_host":target_repository.host().to_string(),
                "target_project":target_repository.project_path(),
                "body":"Created by Prism's bundled coding workflow.",
            }),
            expected_pre_state: serde_json::json!({"head":head,"change_request":null}),
            desired_post_state: serde_json::json!({"head":head,"branch":branch,"subject_digest":subject_digest}),
            exact_head: Some(head.clone()),
            input_revisions: vec![commit.artifact.digest.clone()],
            gate_requirements: gates,
            policy_revisions: policies,
            authority_grant_id: claim.envelope.authority.id.clone(),
            reconciliation_key: format!("create-change-request:{head}"),
            resource_claims: claim.envelope.resource_claims.clone(),
        })?;
        if !matches!(
            intent.state,
            EffectState::Applied | EffectState::ExternallySatisfied
        ) {
            return if matches!(
                intent.state,
                EffectState::Dispatched | EffectState::Indeterminate
            ) {
                Err("change-request creation requires Effect reconciliation".to_string())
            } else {
                self.fail_claim(claim, "guarded change-request creation was not applied")
            };
        }
        let summary = crate::remote::dispatcher::observe_change_request_for_source(
            &path,
            &config,
            &target_repository,
            &branch,
            &head,
        )?
        .ok_or_else(|| "created change request is not yet observable".to_string())?;
        let identity = crate::remote::CanonicalChangeRequestIdentity::new(
            summary.change_request.id.repository(),
            summary.change_request.id.native_id(),
            &summary.change_request.source_repository,
            &summary.change_request.target_repository,
        );
        self.coordinator.finish(
            &claim.lease,
            AttemptResult {
                outcome: "created".to_string(),
                outputs: vec![crate::coding::change_request_artifact(&summary, identity)],
            },
        )
    }

    fn merge_change_request(&self, claim: ClaimedAttempt) -> Result<(), String> {
        let workspace = claim
            .envelope
            .workspace
            .as_ref()
            .ok_or_else(|| "Merge requires an Execution Workspace".to_string())?;
        let path = self.ledger.local_workspace_path(&workspace.id)?;
        let config = Arc::new(Config::load(&Repository { root: path.clone() }));
        let input = claim
            .envelope
            .inputs
            .iter()
            .find(|input| input.port == "change_request")
            .ok_or_else(|| "Merge requires an exact Change Request observation".to_string())?;
        let observation: crate::coding::ChangeRequestObservation =
            serde_json::from_value(input.payload.clone())
                .map_err(|error| format!("decode Change Request observation: {error}"))?;
        let identity: crate::remote::CanonicalChangeRequestIdentity =
            serde_json::from_value(observation.identity.clone())
                .map_err(|error| format!("decode Change Request identity: {error}"))?;
        let repository_id: String = self
            .ledger
            .connection()?
            .query_row(
                "select repository_id from workflow_run where id=?1",
                [claim.envelope.run_id.as_str()],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        let mut broker = EffectBroker::new(self.ledger.clone());
        broker.register(
            "merge",
            Arc::new(ProviderEffectAdapter::new(
                config,
                BTreeMap::from([(repository_id, Repository { root: path.clone() })]),
                BTreeMap::from([(workspace.id.as_str().to_string(), path)]),
            )?),
        )?;
        let gates = self.gate_requirements(
            &claim.envelope.run_id,
            &["ci", "provider-review", "policy", "mergeability"],
        )?;
        let policies = self.effect_policy_revisions(&claim.envelope.run_id, &gates)?;
        let intent = broker.dispatch(DispatchEffect {
            run_id: claim.envelope.run_id.clone(),
            step_id: claim.envelope.step_id.clone(),
            lease: claim.lease.clone(),
            kind: "merge".to_string(),
            target: serde_json::json!({
                "workspace_id": workspace.id.as_str(),
                "identity": identity,
                "display_number": observation.display_number,
                "expected_head": observation.head,
                "submission_mode": "immediate",
            }),
            expected_pre_state: serde_json::json!({
                "head": observation.head,
                "lifecycle": "open",
            }),
            desired_post_state: serde_json::json!({
                "head": observation.head,
                "lifecycle": "merged",
            }),
            exact_head: Some(observation.head.clone()),
            input_revisions: vec![input.artifact.digest.clone()],
            gate_requirements: gates,
            policy_revisions: policies,
            authority_grant_id: claim.envelope.authority.id.clone(),
            reconciliation_key: format!(
                "merge:{}:{}",
                observation.display_number, observation.head
            ),
            resource_claims: claim.envelope.resource_claims.clone(),
        })?;
        if !matches!(
            intent.state,
            EffectState::Applied | EffectState::ExternallySatisfied
        ) {
            return if matches!(
                intent.state,
                EffectState::Dispatched | EffectState::Indeterminate
            ) {
                Err("merge requires Effect reconciliation".to_string())
            } else {
                self.fail_claim(claim, "guarded merge was not applied")
            };
        }
        let mut payload = input.payload.clone();
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "lifecycle".to_string(),
                serde_json::Value::String("merged".to_string()),
            );
        }
        self.coordinator.finish(
            &claim.lease,
            AttemptResult {
                outcome: "merged".to_string(),
                outputs: vec![ArtifactInput {
                    name: "result".to_string(),
                    artifact_type: "builtin:change-request-observation@1".to_string(),
                    payload,
                    trust: TrustClass::DerivedUntrusted,
                    sensitivity: Sensitivity::Internal,
                }],
            },
        )
    }

    fn cleanup_workspace(&self, claim: ClaimedAttempt) -> Result<(), String> {
        let workspace = claim
            .envelope
            .workspace
            .as_ref()
            .ok_or_else(|| "Cleanup requires an Execution Workspace".to_string())?;
        let path = self.ledger.local_workspace_path(&workspace.id)?;
        let repository_id: String = self
            .ledger
            .connection()?
            .query_row(
                "select repository_id from workflow_run where id=?1",
                [claim.envelope.run_id.as_str()],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        let repository = Repository { root: path.clone() };
        let config = Arc::new(Config::load(&repository));
        let mut broker = EffectBroker::new(self.ledger.clone());
        broker.register(
            "worktrunk",
            Arc::new(WorktrunkEffectAdapter::new(
                config,
                BTreeMap::from([(repository_id.clone(), repository)]),
                BTreeMap::from([(workspace.id.as_str().to_string(), path)]),
            )?),
        )?;
        let gates = self.gate_requirements(
            &claim.envelope.run_id,
            &["ci", "provider-review", "policy", "mergeability"],
        )?;
        let policies = self.effect_policy_revisions(&claim.envelope.run_id, &gates)?;
        let subject_digest = claim
            .envelope
            .inputs
            .first()
            .and_then(|input| input.payload.get("head"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Cleanup requires the merged exact-head Artifact".to_string())?;
        let intent = broker.dispatch(DispatchEffect {
            run_id: claim.envelope.run_id.clone(),
            step_id: claim.envelope.step_id.clone(),
            lease: claim.lease.clone(),
            kind: "worktrunk".to_string(),
            target: serde_json::json!({
                "repository_id": repository_id,
                "workspace_id": workspace.id.as_str(),
                "operation": "remove",
                "branch": null,
                "create": null,
                "base": null,
            }),
            expected_pre_state: serde_json::json!({"present": true}),
            desired_post_state: serde_json::json!({"present": false,"subject_digest":subject_digest}),
            exact_head: None,
            input_revisions: claim.envelope.inputs.iter().map(|input| input.artifact.digest.clone()).collect(),
            gate_requirements: gates,
            policy_revisions: policies,
            authority_grant_id: claim.envelope.authority.id.clone(),
            reconciliation_key: format!("cleanup:{}", workspace.id.as_str()),
            resource_claims: claim.envelope.resource_claims.clone(),
        })?;
        if !matches!(
            intent.state,
            EffectState::Applied | EffectState::ExternallySatisfied
        ) {
            return if matches!(
                intent.state,
                EffectState::Dispatched | EffectState::Indeterminate
            ) {
                Err("cleanup requires Effect reconciliation".to_string())
            } else {
                self.fail_claim(claim, "guarded cleanup was not applied")
            };
        }
        self.coordinator.finish(&claim.lease, AttemptResult {
            outcome: "cleaned".to_string(),
            outputs: vec![ArtifactInput {
                name: "result".to_string(),
                artifact_type: "builtin:task@1".to_string(),
                payload: serde_json::json!({"workspace_id": workspace.id.as_str(), "removed": true}),
                trust: TrustClass::Trusted,
                sensitivity: Sensitivity::Internal,
            }],
        })
    }

    fn effect_policy_revisions(
        &self,
        run_id: &RunId,
        gates: &[GateRequirement],
    ) -> Result<Vec<String>, String> {
        let mut revisions = gates
            .iter()
            .map(|gate| gate.policy_revision.clone())
            .collect::<BTreeSet<_>>();
        let admission = self
            .ledger
            .connection()?
            .query_row(
                "select policy_revision from admission_decision where run_id=?1 and outcome='allowed' and (expires_unix_ms is null or expires_unix_ms>?2) order by created_unix_ms desc,id desc limit 1",
                params![run_id.as_str(), now_ms()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        revisions.extend(admission);
        Ok(revisions.into_iter().collect())
    }

    fn gate_requirements(
        &self,
        run_id: &RunId,
        definition_step_ids: &[&str],
    ) -> Result<Vec<GateRequirement>, String> {
        let conn = self.ledger.connection()?;
        definition_step_ids
            .iter()
            .map(|definition_step_id| {
                conn.query_row(
                    "select result.id,result.subject_digest,result.subject_generation,result.policy_revision from gate_result result join workflow_step step on step.id=result.step_id where result.run_id=?1 and step.definition_step_id=?2 and result.status='satisfied' order by result.created_unix_ms desc limit 1",
                    params![run_id.as_str(), definition_step_id],
                    |row| Ok(GateRequirement {
                        gate_result_id: row.get(0)?,
                        subject_digest: row.get(1)?,
                        subject_generation: row.get(2)?,
                        policy_revision: row.get(3)?,
                    }),
                )
                .optional()
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("required Gate '{definition_step_id}' is not satisfied"))
            })
            .collect()
    }

    fn fail_claim(&self, claim: ClaimedAttempt, reason: &str) -> Result<(), String> {
        self.coordinator.fail(&claim.lease, reason)?;
        Err(reason.to_string())
    }

    fn harness_implementation(
        &self,
        claim: &ClaimedAttempt,
        descriptor: ImplementationDescriptor,
    ) -> Result<HarnessAgentImplementation, String> {
        let target = self.local_target(claim)?;
        let workspace = claim
            .envelope
            .workspace
            .as_ref()
            .ok_or_else(|| "Harness Attempt has no Execution Workspace".to_string())?;
        let root = self.ledger.local_workspace_path(&workspace.id)?;
        let repo = Repository { root };
        let config = Config::load(&repo);
        let harness_id = claim
            .envelope
            .settings
            .harness
            .clone()
            .unwrap_or_else(|| config.default_harness.clone());
        let harness = config.harness_config(&harness_id)?;
        HarnessAgentImplementation::new(
            descriptor,
            target,
            self.coordinator.clone(),
            harness_id,
            harness,
        )
    }

    fn local_target(&self, claim: &ClaimedAttempt) -> Result<Arc<LocalTarget>, String> {
        let Some(workspace) = claim.envelope.workspace.as_ref() else {
            return Ok(Arc::new(LocalTarget::new(BTreeMap::new())?));
        };
        let path = self.ledger.local_workspace_path(&workspace.id)?;
        Ok(Arc::new(LocalTarget::single(workspace.id.clone(), &path)?))
    }

    fn materialize_runnable(&self) -> Result<(), String> {
        for run in self.ledger.list(512)? {
            if matches!(
                run.state,
                RunState::Completed | RunState::Failed | RunState::Cancelled
            ) || run.control != "running"
            {
                continue;
            }
            // Resume an interrupted child fan-out before projecting child
            // completion, otherwise a crash after the first child could make a
            // partially materialized call look complete.
            self.resume_child_materialization(&run.id)?;
            self.activate_plan_children(&run.id)?;
            self.operations.schedule(&run.id)?;
            let projection = self.operations.query(&run.id)?;
            for step in projection
                .steps
                .iter()
                .filter(|step| step.state == StepState::Runnable)
            {
                if self.step_has_live_attempt(&step.id)? {
                    continue;
                }
                match step.class {
                    PrimitiveClass::Action => self.prepare_action(&run.id, step)?,
                    PrimitiveClass::Approval => {
                        let (input, evidence) = self.approval_digests(&run.id, &step.id)?;
                        let mode = if step.implementation.contains("exact-mutation") {
                            ApprovalMode::ExactMutation
                        } else if step.implementation.contains("human-test") {
                            ApprovalMode::HumanAttestation
                        } else {
                            ApprovalMode::ArtifactAcceptance
                        };
                        let expires =
                            self.step_settings(&run.id, &step.id)?
                                .timeout_ms
                                .map(|timeout| {
                                    now_ms().saturating_add(timeout.min(i64::MAX as u64) as i64)
                                });
                        self.operations.request_approval(
                            &run.id, &step.id, mode, &input, &evidence, expires,
                        )?;
                    }
                    PrimitiveClass::Wait => {
                        let settings = self.step_settings(&run.id, &step.id)?;
                        let delay = settings.timeout_ms.unwrap_or(0).min(i64::MAX as u64) as i64;
                        self.operations.wait_until(
                            &run.id,
                            &step.id,
                            now_ms().saturating_add(delay),
                        )?;
                    }
                    PrimitiveClass::Notification => {
                        let settings = self.step_settings(&run.id, &step.id)?;
                        self.operations.notify(
                            &run.id,
                            &step.id,
                            "workflow",
                            settings
                                .prompt
                                .as_deref()
                                .unwrap_or("Workflow notification"),
                        )?;
                    }
                    PrimitiveClass::Gate => self.evaluate_gate(&run.id, step)?,
                    PrimitiveClass::WorkflowCall => {
                        self.call_child(&run.id, &step.id)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn evaluate_gate(&self, run_id: &RunId, step: &crate::run::StepSummary) -> Result<(), String> {
        let inputs = self.operations.step_inputs(run_id, &step.id)?;
        let evidence = match step.implementation.as_str() {
            "builtin:local-verification@1" => {
                let (_, path) = self.ledger.ensure_local_workspace(run_id)?;
                let config = Config::load(&Repository { root: path.clone() });
                let result = crate::verify::run_auto_verify(
                    &config,
                    &path,
                    crate::verify::VerifyMode::Normal,
                );
                let subject = inputs.first().ok_or_else(|| {
                    "local verification Gate is missing its candidate Artifact".to_string()
                })?;
                let (subject_digest, subject_revision) = self
                    .operations
                    .step_input_artifact_identity(run_id, &step.id, &subject.name)?;
                let report = crate::gate::verification_report(
                    subject_digest,
                    subject_revision.to_string(),
                    "configured-checks@1",
                    &result,
                );
                crate::gate::ReportGateImplementation::new(crate::gate::GateKind::Verification)
                    .evaluate(&report)?
            }
            "builtin:review-policy@1" => crate::coding::review_policy(&inputs)?,
            "builtin:ci@1" => crate::coding::observation_gate(
                &self.refresh_change_request_input(run_id, &inputs)?,
                "ci",
                "provider-ci@1",
            )?,
            "builtin:provider-review@1" => crate::coding::observation_gate(
                &self.refresh_change_request_input(run_id, &inputs)?,
                "review",
                "provider-review@1",
            )?,
            "builtin:policy@1" => self.evaluate_repository_policy(run_id, &inputs)?,
            "builtin:mergeability@1" => crate::coding::observation_gate(
                &self.refresh_change_request_input(run_id, &inputs)?,
                "mergeability",
                "provider-mergeability@1",
            )?,
            _ => GateEvidence {
                subject_digest: self.ledger.step_input_digest(run_id, &step.id)?,
                subject_generation: "unknown".to_string(),
                evidence: Vec::new(),
                quality: EvidenceQuality::Unavailable,
                policy_revision: "runtime@1".to_string(),
                status: GateStatus::Unavailable,
                reason: format!(
                    "Gate implementation '{}' has no current observation",
                    step.implementation
                ),
                expires_unix_ms: None,
            },
        };
        self.operations.record_gate(run_id, &step.id, &evidence)?;
        Ok(())
    }

    fn refresh_change_request_input(
        &self,
        run_id: &RunId,
        inputs: &[ArtifactInput],
    ) -> Result<ArtifactInput, String> {
        let input = inputs
            .first()
            .ok_or_else(|| "provider Gate is missing its Change Request observation".to_string())?;
        let observation: crate::coding::ChangeRequestObservation =
            serde_json::from_value(input.payload.clone())
                .map_err(|error| format!("decode Change Request observation: {error}"))?;
        let identity: crate::remote::CanonicalChangeRequestIdentity =
            serde_json::from_value(observation.identity)
                .map_err(|error| format!("decode Change Request identity: {error}"))?;
        let (_, path) = self.ledger.ensure_local_workspace(run_id)?;
        let repo = Repository { root: path.clone() };
        let config = Config::load(&repo);
        let summary = crate::remote::dispatcher::observe_change_request_identity(
            &path,
            &config,
            &identity,
            observation.display_number,
        )?;
        Ok(crate::coding::change_request_artifact(&summary, identity))
    }

    fn evaluate_repository_policy(
        &self,
        run_id: &RunId,
        inputs: &[ArtifactInput],
    ) -> Result<GateEvidence, String> {
        let refreshed = self.refresh_change_request_input(run_id, inputs)?;
        let observation: crate::coding::ChangeRequestObservation =
            serde_json::from_value(refreshed.payload)
                .map_err(|error| format!("decode Change Request observation: {error}"))?;
        let (_, path) = self.ledger.ensure_local_workspace(run_id)?;
        let repo = Repository { root: path.clone() };
        let config = Config::load(&repo);
        let policy = crate::remote::dispatcher::refresh_repository_policy(&repo, &path, &config)?;
        let current = policy.identity_complete && policy.error.is_none();
        let revision = crate::run::sha256(
            serde_json::json!({
                "provider":policy.provider,
                "host":policy.canonical_host,
                "project":policy.project_path,
                "target":policy.target_branch,
                "approvals":policy.required_approvals,
                "conversations":policy.require_conversation_resolution,
                "up_to_date":policy.require_branch_up_to_date,
                "checks":policy.required_checks,
                "queue":policy.merge_queue_required,
            })
            .to_string()
            .as_bytes(),
        );
        Ok(GateEvidence {
            subject_digest: observation.head,
            subject_generation: observation.generation,
            evidence: vec![format!("repository policy revision {revision}")],
            quality: if current {
                EvidenceQuality::Current
            } else {
                EvidenceQuality::Unavailable
            },
            policy_revision: revision,
            status: if current {
                GateStatus::Satisfied
            } else {
                GateStatus::Unavailable
            },
            reason: policy
                .error
                .unwrap_or_else(|| "authoritative repository policy observed".to_string()),
            expires_unix_ms: None,
        })
    }

    fn resume_child_materialization(&self, run_id: &RunId) -> Result<(), String> {
        let projection = self.operations.query(run_id)?;
        for step in projection.steps.iter().filter(|step| {
            step.class == PrimitiveClass::WorkflowCall && step.state == StepState::Waiting
        }) {
            self.call_child(run_id, &step.id)?;
        }
        Ok(())
    }

    fn call_child(&self, run_id: &RunId, step_id: &StepId) -> Result<(), String> {
        let (_, step) = self.snapshot_step(run_id, step_id)?;
        let pinned = step.child_workflow.ok_or_else(|| {
            "Workflow Call Step has no pinned child Workflow in its Definition Snapshot".to_string()
        })?;
        let phase_fan_out = pinned.qualified_name == "builtin:plan-phase";
        let snapshot = self.definition_snapshot(&pinned.digest)?;
        if snapshot.content.qualified_name != pinned.qualified_name
            || snapshot.content.source_revision != pinned.revision
        {
            return Err("pinned child Workflow identity does not match its Snapshot".to_string());
        }
        let inputs = self.operations.step_inputs(run_id, step_id)?;
        let plan_input = inputs
            .iter()
            .find(|input| input.artifact_type == "builtin:plan@1");
        if let Some(plan_input) = plan_input.filter(|_| phase_fan_out) {
            let manifest: crate::plan_artifact::PlanManifest =
                serde_json::from_value(plan_input.payload.clone())
                    .map_err(|error| format!("decode Plan Artifact: {error}"))?;
            manifest.validate()?;
            let selected = manifest.selected_phases().cloned().collect::<Vec<_>>();
            for phase in selected {
                let phase_manifest = manifest.phase_manifest(&phase.id)?;
                self.operations.call_child(ChildCall {
                    parent_run: run_id.clone(),
                    parent_step: step_id.clone(),
                    call_key: phase.id.clone(),
                    purpose: format!("plan-phase:{}", phase.id),
                    propagation: "lineage".to_string(),
                    start_paused: !phase.dependencies.is_empty(),
                    snapshot: snapshot.clone(),
                    inputs: vec![phase_manifest.into_artifact(plan_input.trust)],
                })?;
            }
        } else {
            self.operations.call_child(ChildCall {
                parent_run: run_id.clone(),
                parent_step: step_id.clone(),
                call_key: step.id.clone(),
                purpose: step.id,
                propagation: "lineage".to_string(),
                start_paused: false,
                snapshot,
                inputs,
            })?;
        }
        Ok(())
    }

    fn activate_plan_children(&self, parent_run: &RunId) -> Result<(), String> {
        let conn = self.ledger.connection()?;
        let plan_payload: Option<Vec<u8>> = conn
            .query_row(
                "select artifact.payload_inline from artifact join step_attempt attempt on attempt.id=artifact.producer_attempt_id join workflow_step step on step.id=attempt.step_id where artifact.run_id=?1 and artifact.artifact_type='builtin:plan@1' and attempt.state='completed' order by artifact.created_unix_ms desc limit 1",
                [parent_run.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let Some(payload) = plan_payload else {
            return Ok(());
        };
        let manifest: crate::plan_artifact::PlanManifest = serde_json::from_slice(&payload)
            .map_err(|error| format!("decode persisted Plan Artifact: {error}"))?;
        let mut links = BTreeMap::new();
        let mut statement = conn
            .prepare("select link.purpose,link.child_run_id,child.state,child.control from workflow_run_link link join workflow_run child on child.id=link.child_run_id where link.parent_run_id=?1 and link.purpose like 'plan-phase:%'")
            .map_err(|error| error.to_string())?;
        for row in statement
            .query_map([parent_run.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|error| error.to_string())?
        {
            let (purpose, child, state, control) = row.map_err(|error| error.to_string())?;
            if let Some(id) = purpose.strip_prefix("plan-phase:") {
                links.insert(id.to_string(), (child, state, control));
            }
        }
        drop(statement);
        for phase in manifest.selected_phases() {
            let Some((child, state, control)) = links.get(&phase.id) else {
                continue;
            };
            if state != "queued" || control != "pause_requested" {
                continue;
            }
            let dependencies_complete = phase.dependencies.iter().all(|dependency| {
                links
                    .get(dependency)
                    .is_some_and(|(_, state, _)| state == "completed")
            });
            if dependencies_complete {
                conn.execute(
                    "update workflow_run set control='running',revision=revision+1,updated_unix_ms=?2 where id=?1 and state='queued' and control='pause_requested'",
                    params![child, now_ms()],
                )
                .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    fn definition_snapshot(&self, digest: &str) -> Result<DefinitionSnapshot, String> {
        let bytes: Vec<u8> = self
            .ledger
            .connection()?
            .query_row(
                "select canonical_bytes from definition_snapshot where digest=?1",
                [digest],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("pinned child Definition Snapshot '{digest}' is unavailable"))?;
        if crate::run::sha256(&bytes) != digest {
            return Err("pinned child Definition Snapshot failed digest verification".to_string());
        }
        let content: SnapshotContent = serde_json::from_slice(&bytes)
            .map_err(|error| format!("decode child Definition Snapshot: {error}"))?;
        Ok(DefinitionSnapshot {
            digest: digest.to_string(),
            source_trust_digest: content.source_digest.clone(),
            content,
            canonical_bytes: bytes,
        })
    }

    fn prepare_action(&self, run_id: &RunId, step: &crate::run::StepSummary) -> Result<(), String> {
        let descriptor = self.step_descriptor(run_id, &step.id)?;
        let needs_workspace = matches!(descriptor.effect, EffectClass::WorkspaceMutation)
            || descriptor.target == TargetRequirement::Local;
        let (workspace, claims) = if needs_workspace {
            let (workspace, path) = self.ledger.ensure_local_workspace(run_id)?;
            // Different child Runs may currently map distinct durable Workspace
            // IDs to the same local checkout. Claim the local resource identity,
            // not merely the durable ID, so dependency-independent phases cannot
            // become concurrent writers until an isolating target allocates
            // genuinely distinct paths.
            let resource = crate::run::sha256(path.as_os_str().as_encoded_bytes());
            let claims = vec![ResourceClaimSpec {
                key: format!("local-workspace:{resource}"),
                access: if descriptor.effect == EffectClass::ReadOnly {
                    ClaimAccess::MutableRead
                } else {
                    ClaimAccess::Write
                },
                expected_generation: None,
            }];
            (Some(workspace), claims)
        } else {
            (None, Vec::new())
        };
        self.operations
            .prepare_action(run_id, &step.id, "local".to_string(), workspace, claims)?;
        Ok(())
    }

    fn step_has_live_attempt(&self, step_id: &StepId) -> Result<bool, String> {
        self.ledger
            .connection()?
            .query_row(
                "select exists(select 1 from step_attempt where step_id=?1 and state in ('prepared','leased','executing','waiting'))",
                [step_id.as_str()],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())
    }

    fn approval_digests(
        &self,
        run_id: &RunId,
        step_id: &StepId,
    ) -> Result<(String, String), String> {
        let input_digest = self.ledger.step_input_digest(run_id, step_id)?;
        let snapshot_digest = self
            .ledger
            .connection()?
            .query_row(
                "select snapshot_digest from workflow_run where id=?1",
                [run_id.as_str()],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        Ok((input_digest, snapshot_digest))
    }

    fn step_settings(
        &self,
        run_id: &RunId,
        step_id: &StepId,
    ) -> Result<crate::definition::StepSettings, String> {
        let (_, step) = self.snapshot_step(run_id, step_id)?;
        Ok(step.settings)
    }

    fn step_descriptor(
        &self,
        run_id: &RunId,
        step_id: &StepId,
    ) -> Result<ImplementationDescriptor, String> {
        let (snapshot, step) = self.snapshot_step(run_id, step_id)?;
        snapshot
            .implementations
            .into_iter()
            .find(|descriptor| {
                descriptor.id == step.implementation
                    && descriptor.revision == step.implementation_revision
            })
            .ok_or_else(|| "pinned Step Implementation descriptor is absent".to_string())
    }

    fn implementation_descriptor(
        &self,
        claim: &ClaimedAttempt,
    ) -> Result<ImplementationDescriptor, String> {
        self.step_descriptor(&claim.envelope.run_id, &claim.envelope.step_id)
    }

    fn snapshot_step(
        &self,
        run_id: &RunId,
        step_id: &StepId,
    ) -> Result<(SnapshotContent, crate::definition::CompiledStep), String> {
        let conn = self.ledger.connection()?;
        let (bytes, definition_step_id): (Vec<u8>, String) = conn
            .query_row(
                "select snapshot.canonical_bytes,step.definition_step_id from workflow_run run join definition_snapshot snapshot on snapshot.digest=run.snapshot_digest join workflow_step step on step.run_id=run.id where run.id=?1 and step.id=?2",
                params![run_id.as_str(), step_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "Workflow Step Snapshot was not found".to_string())?;
        let snapshot: SnapshotContent = serde_json::from_slice(&bytes)
            .map_err(|error| format!("decode Definition Snapshot: {error}"))?;
        let step = snapshot
            .steps
            .iter()
            .find(|step| step.id == definition_step_id)
            .cloned()
            .ok_or_else(|| {
                "materialized Step is absent from its Definition Snapshot".to_string()
            })?;
        Ok((snapshot, step))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::StartRun;

    #[test]
    fn bundled_plan_copies_source_and_creates_dependency_ordered_phase_children() {
        let root = std::env::temp_dir().join(format!(
            "prism-plan-workflow-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&root)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .status()
            .unwrap();
        std::fs::write(root.join("seed"), "seed").unwrap();
        std::process::Command::new("git")
            .args(["add", "seed"])
            .current_dir(&root)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-qm", "seed"])
            .current_dir(&root)
            .status()
            .unwrap();
        let source = root.join("plan.md");
        std::fs::write(
            &source,
            "# Work\n## Phase 1: Build {#build}\nDo build.\n## Phase 2: Test {#test}\nDo test.\n",
        )
        .unwrap();
        let task =
            crate::plan_artifact::PlanManifest::launch_task_from_file(&source, None, 2).unwrap();
        std::fs::remove_file(&source).unwrap();

        let db = root.join("workflow.db");
        let ledger = RunLedger::open(db).unwrap();
        let repository_id = ledger.repository_id(&root).unwrap();
        let snapshot = crate::definition::DefinitionCatalog::discover(None)
            .resolve("builtin:plan")
            .unwrap();
        let run = ledger
            .start(StartRun {
                actor_capabilities: snapshot.content.transitive_capabilities.clone(),
                snapshot,
                repository_id: Some(repository_id),
                inputs: vec![task],
                idempotency_key: None,
                actor: "test".to_string(),
            })
            .unwrap()
            .run_id;
        let runtime = WorkflowRuntime::new(ledger.clone());

        for worker in ["create", "review"] {
            let claim = runtime
                .claim_next(worker)
                .unwrap()
                .expect("Plan Action claim");
            runtime.execute(claim).unwrap();
        }
        assert!(runtime.claim_next("approval").unwrap().is_none());
        let approval = ledger.inspect(&run).unwrap().approvals[0].clone();
        let invocation_digest: String = ledger
            .connection()
            .unwrap()
            .query_row(
                "select input_digest from workflow_run where id=?1",
                [run.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(approval.request.input_digest, invocation_digest);
        WorkflowOperations::new(ledger.clone())
            .decide_pending(
                approval.request.id,
                true,
                "reviewer".to_string(),
                Some("exact Plan accepted".to_string()),
            )
            .unwrap();

        // The parent tick materializes child Runs; the following tick can
        // schedule one of their Attempts.
        assert!(runtime.claim_next("fan-out").unwrap().is_none());
        let root_phase_claim = runtime
            .claim_next("phase")
            .unwrap()
            .expect("dependency root phase becomes claimable");
        let conn = ledger.connection().unwrap();
        let children: Vec<(String, String, String)> = {
            let mut statement = conn.prepare("select link.purpose,child.control,child.state from workflow_run_link link join workflow_run child on child.id=link.child_run_id where link.parent_run_id=?1 order by link.purpose").unwrap();
            statement
                .query_map([run.as_str()], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].0, "plan-phase:build");
        assert_eq!(children[0].1, "running");
        assert_eq!(
            children[1],
            (
                "plan-phase:test".to_string(),
                "pause_requested".to_string(),
                "queued".to_string()
            )
        );
        let bound_plan = root_phase_claim
            .envelope
            .inputs
            .iter()
            .find(|input| input.port == "plan")
            .unwrap();
        let manifest: crate::plan_artifact::PlanManifest =
            serde_json::from_value(bound_plan.payload.clone()).unwrap();
        assert_eq!(manifest.selected_phase_ids, ["build"]);
        assert!(manifest.content.contains("Do test."));
        assert_eq!(bound_plan.artifact.artifact_type, "builtin:plan@1");
        assert_eq!(
            ledger
                .inspect(&run)
                .unwrap()
                .artifacts
                .iter()
                .find(|artifact| artifact.artifact.artifact_type == "builtin:plan@1")
                .unwrap()
                .trust,
            "trusted"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
