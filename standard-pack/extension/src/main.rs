use prism_extension_sdk::protocol::{
    AgentRequest, ArtifactSchemaDescriptor, AttemptEnvelope, BrokeredEffectRequest, EffectBoundary,
    ExtensionDescriptor, HostOperation, ImplementationDescriptor, InputDescriptor,
    NotificationChannelDescriptor, PortDescriptor, ProcessRequest, ProviderObservationRequest,
    RenderedSpan, RendererDescriptor, StepClass, StructuredRender, TriggerDescriptor,
};
use prism_extension_sdk::{ExecuteContext, ExecuteFuture, Extension, InputFuture};
use serde_json::{Value, json};

const TASK: &str = "prism.task/v1";
const PLAN_SOURCE: &str = "prism.plan-source/v1";
const PLAN: &str = "prism.plan/v1";
const WORKTREE: &str = "prism.worktree-session/v1";
const PROCESS_REQUEST: &str = "prism.process-request/v1";
const PROCESS_RESULT: &str = "prism.process-result/v1";
const AGENT_REQUEST: &str = "prism.agent-request/v1";
const AGENT_RESULT: &str = "prism.agent-result/v1";
const PROVIDER_OBSERVATION_REQUEST: &str = "prism.provider-observation-request/v1";
const PROVIDER_OBSERVATION: &str = "prism.provider-observation/v1";
const REVIEW_OBSERVATION: &str = "prism.review-observation/v1";
const REVIEW_REPORT: &str = "prism.review-report/v1";
const PROMPT_TEMPLATE: &str = "prism.prompt-template/v1";
const PROMPT: &str = "prism.prompt/v1";
const EFFECT_REQUEST: &str = "prism.effect-request/v1";
const EFFECT_RESULT: &str = "prism.effect-result/v1";
const GATE_EVIDENCE: &str = "prism.gate-evidence/v1";
const GATE_RESULT: &str = "prism.gate-result/v1";
const APPROVAL_PRESENTATION: &str = "prism.approval-presentation/v1";
const NOTIFICATION: &str = "prism.notification/v1";
const CANDIDATE_CHANGE: &str = "prism.candidate-change/v1";
const CHANGE_REQUEST: &str = "prism.change-request/v1";
const BOOLEAN: &str = "prism.boolean/v1";
const REPAIR_KIND: &str = "prism.repair-kind/v1";
const REPAIR_REPORT: &str = "prism.repair-report/v1";
const REOBSERVATION: &str = "prism.change-request-observation/v1";
const MERGED_CHANGE: &str = "prism.merged-change/v1";
const ISSUE_INTAKE: &str = "prism.issue-intake/v1";
const ISSUE_CLASSIFICATION: &str = "prism.issue-classification/v1";

struct StandardExtension;

impl Extension for StandardExtension {
    fn id(&self) -> &str {
        "prism.standard/extension"
    }

    fn revision(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn descriptor(&self) -> ExtensionDescriptor {
        ExtensionDescriptor {
            implementations: implementations(),
            artifact_schemas: artifact_schemas(),
            input_support: vec![
                InputDescriptor {
                    schema_id: TASK.into(),
                    editor: "prism.standard/task-input".into(),
                },
                InputDescriptor {
                    schema_id: PLAN.into(),
                    editor: "prism.standard/plan-input".into(),
                },
            ],
            renderers: [
                TASK,
                PLAN_SOURCE,
                PLAN,
                WORKTREE,
                PROCESS_REQUEST,
                PROCESS_RESULT,
                AGENT_REQUEST,
                AGENT_RESULT,
                PROVIDER_OBSERVATION_REQUEST,
                PROVIDER_OBSERVATION,
                REVIEW_OBSERVATION,
                REVIEW_REPORT,
                CANDIDATE_CHANGE,
                CHANGE_REQUEST,
                BOOLEAN,
                REPAIR_KIND,
                REPAIR_REPORT,
                REOBSERVATION,
                MERGED_CHANGE,
                ISSUE_INTAKE,
                ISSUE_CLASSIFICATION,
                PROMPT_TEMPLATE,
                PROMPT,
                EFFECT_REQUEST,
                EFFECT_RESULT,
                GATE_EVIDENCE,
                GATE_RESULT,
                APPROVAL_PRESENTATION,
                NOTIFICATION,
            ]
            .into_iter()
            .map(|schema_id| RendererDescriptor {
                schema_id: schema_id.into(),
                renderer: "prism.standard/structured-text".into(),
            })
            .collect(),
            triggers: vec![TriggerDescriptor {
                id: "prism.standard/manual-trigger".into(),
                capabilities: Vec::new(),
            }],
            notification_channels: vec![NotificationChannelDescriptor {
                id: "prism.standard/diagnostic-notification".into(),
                capabilities: Vec::new(),
            }],
        }
    }

    fn execute(&self, context: ExecuteContext, attempt: AttemptEnvelope) -> ExecuteFuture {
        Box::pin(async move {
            if context.is_cancelled() {
                return Err("cancelled".into());
            }
            if attempt.implementation_id == "prism.standard/echo" {
                return Ok(json!({}));
            }
            let input = attempt
                .input
                .get("request")
                .cloned()
                .unwrap_or(attempt.input);
            let result = match attempt.implementation_id.as_str() {
                "prism.standard/plan-parse" => parse_plan(input),
                "prism.standard/plan-phase-task" => select_plan_phase(input),
                "prism.standard/agent" => {
                    host_call::<AgentRequest>(&context, input, |request| HostOperation::RunAgent {
                        request,
                    })
                    .await
                }
                "prism.standard/command" | "prism.standard/verify" => {
                    host_call::<ProcessRequest>(&context, input, |request| {
                        HostOperation::RunProcess { request }
                    })
                    .await
                }
                "prism.standard/provider-observe" => {
                    host_call::<ProviderObservationRequest>(&context, input, |request| {
                        HostOperation::ObserveProvider { request }
                    })
                    .await
                }
                "prism.standard/review-filter" => filter_review(input),
                "prism.standard/render-prompt" => render_prompt(input),
                "prism.standard/git-commit" => {
                    protected(&context, input, |request| HostOperation::Commit { request }).await
                }
                "prism.standard/git-push" => {
                    protected(&context, input, |request| HostOperation::Push { request }).await
                }
                "prism.standard/resolve-addressed-threads" => {
                    return resolve_addressed_threads(&context, input).await;
                }
                "prism.standard/squash-merge" => {
                    protected(&context, input, |request| HostOperation::SquashMerge {
                        request,
                    })
                    .await
                }
                "prism.standard/worktrunk-delete" => {
                    protected(&context, input, |request| HostOperation::DeleteWorktree {
                        request,
                    })
                    .await
                }
                "prism.standard/gate" => evaluate_gate(input),
                "prism.standard/approval-presentation" => Ok(json!({
                    "summary": input.get("summary").and_then(Value::as_str).unwrap_or("Approval required"),
                    "request": input
                })),
                "prism.standard/notification" => return Ok(json!({})),
                "prism.standard/choose-stabilization-repair" => {
                    return choose_stabilization_repair(input);
                }
                "prism.standard/summarize-stabilization" => return summarize_stabilization(input),
                "prism.standard/verified-candidate" => return verified_candidate(input),
                "prism.standard/reobserve-change-request" => {
                    return reobserve_change_request(&context, input).await;
                }
                "prism.standard/classify-issue" => return classify_issue(input),
                "prism.standard/agent-implement"
                | "prism.standard/agent-repair-ci"
                | "prism.standard/agent-repair-review" => {
                    return semantic_agent(&context, &attempt.implementation_id, input).await;
                }
                "prism.standard/observe-ci"
                | "prism.standard/observe-review"
                | "prism.standard/observe-policy"
                | "prism.standard/observe-mergeability"
                | "prism.standard/observe-merge-relation" => {
                    return semantic_observation(&context, &attempt.implementation_id, input).await;
                }
                "prism.standard/local-verification" | "prism.standard/verify-repair" => {
                    return semantic_verification(&context, input).await;
                }
                "prism.standard/self-review" | "prism.standard/revalidate-merge" => {
                    return semantic_gate(input);
                }
                "prism.standard/commit-candidate"
                | "prism.standard/commit-repair"
                | "prism.standard/push-candidate"
                | "prism.standard/push-successor"
                | "prism.standard/create-change-request"
                | "prism.standard/squash-merge-exact-head"
                | "prism.standard/delete-exact-worktree" => {
                    return semantic_effect(&context, &attempt.implementation_id, input).await;
                }
                "prism.standard/plan-approval"
                | "prism.standard/merge-approval"
                | "prism.standard/issue-admission" => {
                    return Ok(json!({"result": approval_presentation(input)?}));
                }
                implementation => Err(format!(
                    "unknown Standard implementation '{implementation}'"
                )),
            }?;
            Ok(json!({"result":result}))
        })
    }

    fn invoke_trigger(
        &self,
        _context: ExecuteContext,
        _adapter_id: String,
        input: Value,
    ) -> ExecuteFuture {
        Box::pin(async move { Ok(json!({"occurrence": input})) })
    }

    fn send_notification(
        &self,
        _context: ExecuteContext,
        _channel_id: String,
        notification: Value,
    ) -> ExecuteFuture {
        Box::pin(async move { Ok(json!({"accepted": true, "notification": notification})) })
    }

    fn suggest_input(&self, schema_id: String, context: Value) -> InputFuture<Vec<Value>> {
        Box::pin(async move {
            match schema_id.as_str() {
                TASK => Ok(context.get("selected_issue").cloned().into_iter().collect()),
                PLAN => Ok(Vec::new()),
                _ => Err(prism_extension_sdk::protocol::ProtocolError::new(
                    "unsupported_schema",
                    format!("no Standard input support for '{schema_id}'"),
                )),
            }
        })
    }

    fn validate_input(&self, schema_id: String, value: Value) -> InputFuture<()> {
        Box::pin(async move {
            let valid = match schema_id.as_str() {
                TASK => non_empty_string(&value, "title") || non_empty_string(&value, "body"),
                PLAN => value
                    .get("steps")
                    .and_then(Value::as_array)
                    .is_some_and(|steps| !steps.is_empty()),
                _ => false,
            };
            valid.then_some(()).ok_or_else(|| {
                prism_extension_sdk::protocol::ProtocolError::new(
                    "invalid_input",
                    format!("value does not satisfy '{schema_id}'"),
                )
            })
        })
    }

    fn render_artifact(
        &self,
        schema_id: String,
        value: Value,
        width: u16,
    ) -> InputFuture<StructuredRender> {
        Box::pin(async move {
            let summary = artifact_summary(&schema_id, &value);
            let limit = usize::from(width.max(16)).saturating_mul(8);
            let summary = truncate(&summary, limit);
            Ok(StructuredRender {
                spans: vec![RenderedSpan {
                    text: summary.clone(),
                    style: render_style(&schema_id).into(),
                }],
                summary,
            })
        })
    }
}

async fn host_call<T: serde::de::DeserializeOwned>(
    context: &ExecuteContext,
    input: Value,
    operation: impl FnOnce(T) -> HostOperation,
) -> Result<Value, String> {
    let request = serde_json::from_value(input).map_err(|error| error.to_string())?;
    context
        .host_operation(operation(request))
        .await
        .map_err(|error| error.message)
}

async fn protected(
    context: &ExecuteContext,
    input: Value,
    operation: impl FnOnce(BrokeredEffectRequest) -> HostOperation,
) -> Result<Value, String> {
    host_call(context, input, operation).await
}

fn parse_plan(input: Value) -> Result<Value, String> {
    if let Some(steps) = input.get("steps").and_then(Value::as_array) {
        if steps.is_empty() {
            return Err("Plan must contain at least one step".into());
        }
        return Ok(json!({"steps": steps}));
    }
    let source = input
        .get("source")
        .and_then(Value::as_str)
        .ok_or_else(|| "Plan input requires 'steps' or textual 'source'".to_string())?;
    let mut phases = source
        .lines()
        .filter_map(|line| {
            let heading = line.trim_start_matches('#').trim_start();
            let rest = heading.strip_prefix("Phase ")?;
            let digits = rest
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>();
            let phase = digits.parse::<u64>().ok()?;
            Some((phase, heading.to_string()))
        })
        .collect::<Vec<_>>();
    phases.sort_by_key(|(phase, _)| *phase);
    phases.dedup_by_key(|(phase, _)| *phase);
    let steps = if phases.is_empty() {
        source
            .lines()
            .map(str::trim)
            .filter_map(|line| {
                line.strip_prefix("- ")
                    .or_else(|| line.strip_prefix("* "))
                    .map(str::trim)
            })
            .filter(|line| !line.is_empty())
            .map(|instruction| json!({"instruction": instruction}))
            .collect::<Vec<_>>()
    } else {
        phases
            .into_iter()
            .map(|(phase, instruction)| json!({"phase":phase,"instruction":instruction}))
            .collect()
    };
    if steps.is_empty() {
        return Err("Plan source contains no list steps".into());
    }
    Ok(json!({"steps":steps}))
}

fn select_plan_phase(input: Value) -> Result<Value, String> {
    let plan = input.get("plan").unwrap_or(&input);
    let steps = plan
        .get("steps")
        .and_then(Value::as_array)
        .filter(|steps| !steps.is_empty())
        .ok_or_else(|| "Plan has no phase to execute".to_string())?;
    let phase = steps
        .iter()
        .find(|step| step.get("status").and_then(Value::as_str) != Some("completed"))
        .unwrap_or(&steps[0]);
    let phase_index = steps
        .iter()
        .position(|candidate| candidate == phase)
        .unwrap_or(0);
    Ok(json!({"task": {
        "title": phase.get("instruction").and_then(Value::as_str).unwrap_or("Implement Plan phase"),
        "body": phase,
        "plan_revision": plan.get("revision"),
        "plan": plan,
        "phase_index": phase_index
    }}))
}

fn choose_stabilization_repair(input: Value) -> Result<Value, String> {
    let evidence = input
        .as_object()
        .ok_or_else(|| "stabilization evidence must be an object".to_string())?;
    let head = evidence
        .get("candidate")
        .and_then(|candidate| candidate.get("head"))
        .and_then(Value::as_str)
        .ok_or_else(|| "stabilization candidate has no exact head".to_string())?;
    for name in ["ci", "review", "policy", "mergeability", "merge_relation"] {
        let item = evidence
            .get(name)
            .ok_or_else(|| format!("missing {name} observation"))?;
        match item.get("quality").and_then(Value::as_str) {
            Some("current") => {}
            Some("unsupported") => {
                let reason = item
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("provider capability is unavailable");
                return Err(format!("{name} observation is unsupported: {reason}"));
            }
            Some(quality) => {
                return Err(format!(
                    "{name} observation is {quality}; reobserve before repair"
                ));
            }
            None => return Err(format!("{name} observation has no quality")),
        }
        if item.get("head").and_then(Value::as_str) != Some(head) {
            return Err(format!("{name} observation is not for exact head {head}"));
        }
    }
    let unsatisfied =
        |name: &str| evidence[name].get("satisfied").and_then(Value::as_bool) == Some(false);
    let repair_kind = if unsatisfied("ci") {
        "ci"
    } else if unsatisfied("review") {
        "review"
    } else if ["policy", "mergeability", "merge_relation"]
        .into_iter()
        .any(unsatisfied)
    {
        "blocked"
    } else {
        "none"
    };
    Ok(json!({
        "needs_repair": matches!(repair_kind, "ci" | "review"),
        "repair_kind": repair_kind
    }))
}

fn summarize_stabilization(input: Value) -> Result<Value, String> {
    let observation = input.get("observation").unwrap_or(&input);
    let candidate = observation
        .get("candidate")
        .cloned()
        .ok_or_else(|| "reobservation has no exact Change Request candidate".to_string())?;
    let facts = observation
        .get("facts")
        .and_then(Value::as_object)
        .ok_or_else(|| "reobservation has no provider facts".to_string())?;
    let mut outputs = serde_json::Map::new();
    let head = candidate
        .get("head")
        .and_then(Value::as_str)
        .ok_or_else(|| "reobserved Change Request has no exact head".to_string())?
        .to_owned();
    outputs.insert("candidate".into(), candidate);
    let mut ready = true;
    for name in ["ci", "review", "policy", "mergeability", "merge_relation"] {
        let evidence = facts
            .get(name)
            .cloned()
            .ok_or_else(|| format!("reobservation is missing {name}"))?;
        ready &= evidence.get("quality").and_then(Value::as_str) == Some("current")
            && evidence.get("satisfied").and_then(Value::as_bool) == Some(true)
            && evidence.get("head").and_then(Value::as_str) == Some(head.as_str());
        outputs.insert(name.into(), evidence);
    }
    outputs.insert("ready".into(), Value::Bool(ready));
    Ok(Value::Object(outputs))
}

fn verified_candidate(input: Value) -> Result<Value, String> {
    for gate in ["local", "review"] {
        if input[gate].get("status").and_then(Value::as_str) != Some("satisfied") {
            return Err(format!("{gate} Gate is not satisfied"));
        }
    }
    let mut candidate = input["candidate"].clone();
    let task = candidate.get("task").cloned().unwrap_or_else(|| {
        json!({
            "title":"Completed Task", "body":"No additional Plan phase"
        })
    });
    let phase_index = task.get("phase_index").and_then(Value::as_u64);
    let plan_steps = task
        .get("plan")
        .and_then(|plan| plan.get("steps"))
        .and_then(Value::as_array);
    let next_index = phase_index.map(|index| index.saturating_add(1));
    let next_phase =
        next_index.and_then(|index| plan_steps.and_then(|steps| steps.get(index as usize)));
    let next_task = next_phase.map_or_else(
        || task.clone(),
        |phase| json!({
            "title":phase.get("instruction").and_then(Value::as_str).unwrap_or("Implement Plan phase"),
            "body":phase,
            "plan_revision":task.get("plan_revision"),
            "plan":task.get("plan"),
            "phase_index":next_index
        }),
    );
    let verified_tree = input["local"]
        .get("tree")
        .and_then(Value::as_str)
        .ok_or_else(|| "local Gate has no exact verified Git tree".to_string())?;
    if let Some(object) = candidate.as_object_mut() {
        object.insert("verified_tree".into(), Value::String(verified_tree.into()));
    }
    Ok(json!({
        "candidate": candidate,
        "next_task": next_task,
        "plan_complete": next_phase.is_none()
    }))
}

async fn reobserve_change_request(context: &ExecuteContext, input: Value) -> Result<Value, String> {
    let candidate = input
        .get("successor")
        .filter(|value| !value.is_null())
        .or_else(|| input.get("original"))
        .ok_or_else(|| "reobservation requires a Change Request candidate".to_string())?;
    let subject = candidate
        .get("change_request")
        .or_else(|| candidate.get("subject"))
        .ok_or_else(|| "Change Request candidate has no opaque provider subject".to_string())?;
    let subject: prism_extension_sdk::protocol::OpaqueReference =
        serde_json::from_value(subject.clone())
            .map_err(|error| format!("invalid Change Request subject: {error}"))?;
    let mut facts = serde_json::Map::new();
    for operation in ["ci", "review", "policy", "mergeability", "merge_relation"] {
        let evidence = context
            .host_operation(HostOperation::ObserveProvider {
                request: ProviderObservationRequest {
                    subject: subject.clone(),
                    operation: operation.into(),
                },
            })
            .await
            .map_err(|error| format!("{operation} reobservation failed: {}", error.message))?;
        facts.insert(operation.into(), evidence);
    }
    Ok(json!({"observation":{"candidate":candidate,"facts":facts}}))
}

fn classify_issue(input: Value) -> Result<Value, String> {
    let intake = input.get("intake").unwrap_or(&input);
    Ok(json!({"classification":{
        "issue": intake,
        "quarantined": true,
        "admission_required": true
    }}))
}

fn approval_presentation(input: Value) -> Result<Value, String> {
    let candidate = input.get("candidate");
    Ok(json!({
        "summary": candidate
            .and_then(|candidate| candidate.get("head"))
            .and_then(Value::as_str)
            .map(|head| format!("Approve squash merge of exact head {head}"))
            .unwrap_or_else(|| "Approval required".into()),
        "subject": candidate,
        "evidence": input
    }))
}

async fn semantic_agent(
    context: &ExecuteContext,
    implementation: &str,
    input: Value,
) -> Result<Value, String> {
    let worktree = input
        .get("worktree")
        .or_else(|| {
            input
                .get("candidate")
                .and_then(|value| value.get("worktree"))
        })
        .ok_or_else(|| format!("{implementation} requires an exact Worktree Session"))?;
    let working_scope = serde_json::from_value(worktree.clone())
        .map_err(|error| format!("invalid Worktree Session: {error}"))?;
    let subject = input
        .get("task")
        .or_else(|| input.get("evidence"))
        .cloned()
        .unwrap_or(Value::Null);
    let request = AgentRequest {
        harness: "default".into(),
        model: None,
        prompt: repair_prompt(implementation, &subject),
        working_scope,
        continuation: None,
        tool_policy: json!({"protected_effects":false}),
        timeout_ms: 3_600_000,
        max_output_bytes: 1_048_576,
    };
    let result = context
        .host_operation(HostOperation::RunAgent { request })
        .await
        .map_err(|error| error.message)?;
    let mut candidate = result
        .get("candidate")
        .cloned()
        .unwrap_or_else(|| json!({"agent_result":result}));
    if let Some(original) = input.get("candidate") {
        candidate = merge_objects(original, &candidate);
    }
    if let Some(object) = candidate.as_object_mut() {
        object.entry("worktree").or_insert_with(|| worktree.clone());
        if let Some(task) = input.get("task") {
            object.entry("task").or_insert_with(|| task.clone());
        }
    }
    if implementation.ends_with("repair-review") {
        let addressed = result
            .get("addressed_thread_ids")
            .cloned()
            .unwrap_or_else(|| json!([]));
        Ok(json!({"candidate":candidate,"report":{
            "addressed_thread_ids":addressed,
            "source_observation_revision":input["evidence"].get("revision")
        }}))
    } else {
        Ok(json!({"candidate":candidate}))
    }
}

async fn semantic_observation(
    context: &ExecuteContext,
    implementation: &str,
    input: Value,
) -> Result<Value, String> {
    let candidate = &input["candidate"];
    let subject = candidate
        .get("change_request")
        .or_else(|| candidate.get("subject"))
        .ok_or_else(|| "Change Request candidate has no opaque provider subject".to_string())?;
    let request = ProviderObservationRequest {
        subject: serde_json::from_value(subject.clone())
            .map_err(|error| format!("invalid Change Request subject: {error}"))?,
        operation: implementation
            .trim_start_matches("prism.standard/observe-")
            .replace('-', "_"),
    };
    let evidence = context
        .host_operation(HostOperation::ObserveProvider { request })
        .await
        .map_err(|error| error.message)?;
    Ok(json!({"evidence":evidence}))
}

fn repair_prompt(implementation: &str, subject: &Value) -> String {
    let task = implementation.replace("prism.standard/", "");
    let output_contract = if implementation.ends_with("repair-review") {
        "Finish with one JSON object containing summary (string) and addressed_thread_ids (an array containing only IDs you actually addressed)."
    } else if implementation.ends_with("repair-ci") {
        "Finish with one JSON object containing summary (string) and addressed_thread_ids (an empty array)."
    } else {
        "Finish with one JSON object containing summary (string)."
    };
    format!(
        "{task}\nWork only on the supplied evidence. Do not commit, push, resolve provider threads, or perform any other protected effect. {output_contract}\n{subject}"
    )
}

async fn semantic_verification(context: &ExecuteContext, input: Value) -> Result<Value, String> {
    let candidate = input
        .get("ci_candidate")
        .filter(|candidate| !candidate.is_null())
        .or_else(|| {
            input
                .get("review_candidate")
                .filter(|candidate| !candidate.is_null())
        })
        .or_else(|| input.get("candidate"))
        .ok_or_else(|| "local verification requires a repaired candidate".to_string())?;
    let worktree = candidate
        .get("worktree")
        .ok_or_else(|| "local verification requires an exact Worktree Session".to_string())?;
    let request = ProcessRequest {
        executable: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            "git diff --check && if [ -x scripts/full-check.sh ]; then scripts/full-check.sh; elif [ -f Cargo.toml ]; then cargo test --all-targets; else git status --short; fi && index=$(mktemp) && trap 'rm -f \"$index\"' EXIT && GIT_INDEX_FILE=$index git read-tree HEAD && GIT_INDEX_FILE=$index git add -A && tree=$(GIT_INDEX_FILE=$index git write-tree) && printf '\\nPRISM_VERIFIED_TREE=%s\\n' \"$tree\"".into(),
        ],
        working_scope: serde_json::from_value(worktree.clone())
            .map_err(|error| format!("invalid Worktree Session: {error}"))?,
        environment: Default::default(),
        timeout_ms: 3_600_000,
        max_output_bytes: 1_048_576,
    };
    let process = context
        .host_operation(HostOperation::RunProcess { request })
        .await
        .map_err(|error| error.message)?;
    let tree = process
        .get("stdout")
        .and_then(Value::as_str)
        .and_then(|output| {
            output
                .lines()
                .rev()
                .find_map(|line| line.strip_prefix("PRISM_VERIFIED_TREE="))
        })
        .filter(|tree| !tree.trim().is_empty())
        .ok_or_else(|| "local verification did not report the verified Git tree".to_string())?;
    Ok(json!({"result":{
        "tree":tree,
        "status":"satisfied",
        "quality":"current",
        "subject":{"worktree":worktree,"head":candidate.get("head")},
        "revision":format!("verification:{}", candidate.get("head").and_then(Value::as_str).unwrap_or("unknown")),
        "process":process
    }}))
}

fn semantic_gate(input: Value) -> Result<Value, String> {
    let evidence = input
        .get("observation")
        .or_else(|| input.get("candidate"))
        .or_else(|| input.get("ci_candidate"))
        .unwrap_or(&input);
    if let Some(facts) = evidence.get("facts").and_then(Value::as_object) {
        let satisfied = ["ci", "review", "policy", "mergeability", "merge_relation"]
            .into_iter()
            .all(|name| {
                facts.get(name).is_some_and(|fact| {
                    fact.get("quality").and_then(Value::as_str) == Some("current")
                        && fact.get("satisfied").and_then(Value::as_bool) == Some(true)
                })
            });
        let gate_revisions = facts
            .iter()
            .filter_map(|(name, fact)| {
                fact.get("revision")
                    .and_then(Value::as_str)
                    .map(|revision| (name.clone(), Value::String(revision.to_string())))
            })
            .collect::<serde_json::Map<_, _>>();
        return Ok(json!({"result":{
            "status":if satisfied { "satisfied" } else { "unsatisfied" },
            "subject":evidence.get("candidate"),
            "evidence_revision":facts.values().filter_map(|fact| fact.get("revision")).collect::<Vec<_>>(),
            "gate_revisions":gate_revisions,
            "policy_revision":facts.get("policy").and_then(|fact| fact.get("policy_revision").or_else(|| fact.get("revision")))
        }}));
    }
    let evidence = evidence
        .get("gate")
        .or_else(|| evidence.get("verification"))
        .unwrap_or(evidence);
    Ok(json!({"result":evaluate_gate(evidence.clone())?}))
}

async fn semantic_effect(
    context: &ExecuteContext,
    implementation: &str,
    input: Value,
) -> Result<Value, String> {
    let effect_name = match implementation {
        "prism.standard/commit-candidate" | "prism.standard/commit-repair" => "commit",
        "prism.standard/push-candidate" | "prism.standard/push-successor" => "push",
        "prism.standard/create-change-request" => "create_change_request",
        "prism.standard/squash-merge-exact-head" => "squash_merge",
        "prism.standard/delete-exact-worktree" => "delete_worktree",
        _ => return Err(format!("unknown semantic effect {implementation}")),
    };
    let source = input
        .get("candidate")
        .or_else(|| input.get("merged"))
        .or_else(|| input.get("ci_candidate"))
        .or_else(|| input.get("review_candidate"))
        .unwrap_or(&input);
    let request = source
        .get("effects")
        .and_then(|effects| effects.get(effect_name))
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| inferred_effect_request(effect_name, &input, source))?;
    if effect_name == "squash_merge" {
        if input["gates"].get("status").and_then(Value::as_str) != Some("satisfied") {
            return Err("exact-head Gates must be revalidated before squash merge".into());
        }
        if input["candidate"].get("head").and_then(Value::as_str)
            != request["preconditions"]["expected_head"].as_str()
        {
            return Err("merge intent is not bound to the approved exact head".into());
        }
    }
    if effect_name == "delete_worktree" {
        if !matches!(
            input["merged"].get("status").and_then(Value::as_str),
            Some("succeeded" | "merged" | "proven")
        ) {
            return Err("cleanup requires an authoritatively proven merge".into());
        }
        let requested_worktree = &request["preconditions"]["worktree_session"];
        if input["worktree"].get("id") != requested_worktree.get("id")
            || input["worktree"].get("revision") != requested_worktree.get("revision")
        {
            return Err(
                "cleanup intent belongs to a different Worktree Session incarnation".into(),
            );
        }
    }
    let result = match effect_name {
        "commit" => {
            protected(context, request, |request| HostOperation::Commit {
                request,
            })
            .await?
        }
        "push" => protected(context, request, |request| HostOperation::Push { request }).await?,
        "create_change_request" => {
            protected(context, request, |request| {
                HostOperation::CreateChangeRequest { request }
            })
            .await?
        }
        "squash_merge" => {
            protected(context, request, |request| HostOperation::SquashMerge {
                request,
            })
            .await?
        }
        "delete_worktree" => {
            protected(context, request, |request| HostOperation::DeleteWorktree {
                request,
            })
            .await?
        }
        _ => unreachable!(),
    };
    let mut successor = merge_objects(source, &result);
    if effect_name == "push"
        && let Some(head) = successor
            .get("head")
            .and_then(Value::as_str)
            .map(str::to_owned)
        && let Some(object) = successor.as_object_mut()
    {
        for field in ["change_request", "subject"] {
            if let Some(subject) = object.get_mut(field).and_then(Value::as_object_mut) {
                subject.insert("revision".into(), Value::String(head.clone()));
            }
        }
    }
    Ok(match effect_name {
        "create_change_request" => json!({"candidate":successor}),
        "squash_merge" => json!({"merged":successor}),
        "delete_worktree" => {
            json!({"notification":{"body":"Merged change and deleted exact Worktree Session"}})
        }
        _ => json!({"candidate":successor}),
    })
}

fn inferred_effect_request(
    effect_name: &str,
    input: &Value,
    source: &Value,
) -> Result<Value, String> {
    match effect_name {
        "squash_merge" => {
            let change_request = source
                .get("change_request")
                .or_else(|| source.get("subject"))
                .ok_or_else(|| "candidate has no exact Change Request identity".to_string())?;
            let subject_id = change_request
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Change Request identity has no ID".to_string())?;
            let head = source
                .get("head")
                .and_then(Value::as_str)
                .ok_or_else(|| "candidate has no exact head".to_string())?;
            let repository_id = subject_id
                .rsplit_once(":change_request:")
                .map(|(repository, _)| repository)
                .ok_or_else(|| "Change Request identity is not canonical".to_string())?;
            let gates = input
                .get("gates")
                .ok_or_else(|| "squash merge has no revalidated Gates".to_string())?;
            let policy_revision = gates
                .get("policy_revision")
                .and_then(Value::as_str)
                .ok_or_else(|| "squash merge has no repository policy revision".to_string())?;
            let gate_revisions = gates
                .get("gate_revisions")
                .cloned()
                .ok_or_else(|| "squash merge has no exact Gate revisions".to_string())?;
            Ok(json!({
                "effect_id":format!("squash-merge:{subject_id}:{head}"),
                "idempotency_key":format!("squash-merge:{subject_id}:{head}"),
                "authority_scope":"provider:write",
                "preconditions":{
                    "repository":{"id":repository_id,"revision":head},
                    "worktree_session":null,
                    "expected_head":head,
                    "target_repository":{"id":repository_id,"revision":policy_revision},
                    "policy_revision":policy_revision,
                    "gate_revisions":gate_revisions
                },
                "parameters":{"method":"squash","change_request":change_request}
            }))
        }
        "create_change_request" => {
            let worktree = source.get("worktree").ok_or_else(|| {
                "Change Request creation requires an exact Worktree Session".to_string()
            })?;
            let repository = worktree
                .get("repository")
                .and_then(Value::as_str)
                .ok_or_else(|| "Worktree Session has no Repository".to_string())?;
            let id = worktree
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Worktree Session has no ID".to_string())?;
            let revision = worktree
                .get("revision")
                .and_then(Value::as_str)
                .ok_or_else(|| "Worktree Session has no revision".to_string())?;
            let head = source
                .get("head")
                .and_then(Value::as_str)
                .ok_or_else(|| "Change Request candidate has no exact head".to_string())?;
            let branch = worktree
                .get("branch")
                .and_then(Value::as_str)
                .ok_or_else(|| "Worktree Session has no branch".to_string())?;
            let target = source
                .get("target_repository")
                .cloned()
                .unwrap_or_else(|| json!({"id":repository,"revision":head}));
            let target_id = target
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "target Repository has no ID".to_string())?;
            Ok(json!({
                "effect_id":format!("create-change-request:{id}:{revision}:{head}"),
                "idempotency_key":format!("create-change-request:{id}:{revision}:{head}"),
                "authority_scope":"provider:write",
                "preconditions":{
                    "repository":{"id":repository,"revision":revision},
                    "worktree_session":{"id":id,"revision":revision},
                    "expected_head":head,
                    "target_repository":{"id":target_id,"revision":target.get("revision").and_then(Value::as_str).unwrap_or(head)},
                    "policy_revision":null,
                    "gate_revisions":{}
                },
                "parameters":{
                    "branch":branch,
                    "body":source.get("description").or_else(|| source.get("body")).and_then(Value::as_str).unwrap_or("Created by Prism workflow")
                }
            }))
        }
        "delete_worktree" => {
            let worktree = input
                .get("worktree")
                .ok_or_else(|| "cleanup requires an exact Worktree Session".to_string())?;
            let repository = worktree
                .get("repository")
                .and_then(Value::as_str)
                .ok_or_else(|| "Worktree Session has no Repository".to_string())?;
            let revision = worktree
                .get("revision")
                .and_then(Value::as_str)
                .ok_or_else(|| "Worktree Session has no revision".to_string())?;
            let path = worktree
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| "Worktree Session has no path".to_string())?;
            let branch = worktree
                .get("branch")
                .and_then(Value::as_str)
                .ok_or_else(|| "Worktree Session has no branch".to_string())?;
            let id = worktree
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Worktree Session has no ID".to_string())?;
            Ok(json!({
                "effect_id":format!("delete-worktree:{id}:{revision}"),
                "idempotency_key":format!("delete-worktree:{id}:{revision}"),
                "authority_scope":"worktrunk:write",
                "preconditions":{
                    "repository":{"id":repository,"revision":revision},
                    "worktree_session":{"id":id,"revision":revision},
                    "expected_head":null,
                    "target_repository":null,
                    "policy_revision":null,
                    "gate_revisions":{}
                },
                "parameters":{"expected_path":path,"branch":branch}
            }))
        }
        "commit" | "push" => {
            let worktree = source
                .get("worktree")
                .ok_or_else(|| format!("{effect_name} requires an exact Worktree Session"))?;
            let repository = worktree
                .get("repository")
                .and_then(Value::as_str)
                .ok_or_else(|| "Worktree Session has no Repository".to_string())?;
            let id = worktree
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Worktree Session has no ID".to_string())?;
            let revision = worktree
                .get("revision")
                .and_then(Value::as_str)
                .ok_or_else(|| "Worktree Session has no revision".to_string())?;
            let head = source
                .get("head")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{effect_name} candidate has no exact head"))?;
            let branch = worktree
                .get("branch")
                .and_then(Value::as_str)
                .ok_or_else(|| "Worktree Session has no branch".to_string())?;
            let (expected_head, parameters) = if effect_name == "commit" {
                let expected_tree = input
                    .get("verification")
                    .and_then(|verification| verification.get("tree"))
                    .or_else(|| source.get("verified_tree"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        "commit requires the exact locally verified Git tree".to_string()
                    })?;
                (
                    head,
                    json!({"message":"Apply Prism workflow repair","expected_tree":expected_tree}),
                )
            } else {
                let expected_remote = source
                    .get("previous_head")
                    .or_else(|| source.get("remote_head"));
                (
                    head,
                    json!({"branch":branch,"remote":"origin","expected_remote_head":expected_remote}),
                )
            };
            Ok(json!({
                "effect_id":format!("{effect_name}:{id}:{revision}:{expected_head}"),
                "idempotency_key":format!("{effect_name}:{id}:{revision}:{expected_head}"),
                "authority_scope":"git:write",
                "preconditions":{
                    "repository":{"id":repository,"revision":revision},
                    "worktree_session":{"id":id,"revision":revision},
                    "expected_head":expected_head,
                    "target_repository":null,
                    "policy_revision":null,
                    "gate_revisions":{}
                },
                "parameters":parameters
            }))
        }
        _ => Err(format!(
            "candidate has no exact brokered {effect_name} intent"
        )),
    }
}

fn merge_objects(base: &Value, update: &Value) -> Value {
    let mut merged = base.as_object().cloned().unwrap_or_default();
    if let Some(update) = update.as_object() {
        merged.extend(update.clone());
    }
    Value::Object(merged)
}

async fn resolve_addressed_threads(
    context: &ExecuteContext,
    input: Value,
) -> Result<Value, String> {
    if input["report"].get("source_observation_revision") != input["observation"].get("revision") {
        return Err("repair report does not belong to the supplied review observation".into());
    }
    let subject = input["candidate"]
        .get("change_request")
        .or_else(|| input["candidate"].get("subject"))
        .ok_or_else(|| "successor candidate has no Change Request identity".to_string())?;
    let current = context
        .host_operation(HostOperation::ObserveProvider {
            request: ProviderObservationRequest {
                subject: serde_json::from_value(subject.clone())
                    .map_err(|error| format!("invalid Change Request subject: {error}"))?,
                operation: "review".into(),
            },
        })
        .await
        .map_err(|error| format!("review reobservation failed: {}", error.message))?;
    if current.get("quality").and_then(Value::as_str) != Some("current") {
        return Err("review threads must be current before resolution".into());
    }
    let threads = addressed_threads(&input["observation"], &current, &input["report"])?;
    if threads.is_empty() {
        return Ok(json!({"candidate":input["candidate"]}));
    }
    let mut request = match input["candidate"]
        .get("effects")
        .and_then(|effects| effects.get("resolve_review_threads"))
        .cloned()
    {
        Some(request) => Ok(request),
        None => inferred_thread_resolution(&input["candidate"]),
    }?;
    request["parameters"]["threads"] = Value::Array(threads);
    let result = protected(context, request, |request| {
        HostOperation::ResolveReviewThreads { request }
    })
    .await?;
    Ok(json!({"candidate":merge_objects(&input["candidate"], &result)}))
}

fn inferred_thread_resolution(candidate: &Value) -> Result<Value, String> {
    let subject = candidate
        .get("change_request")
        .or_else(|| candidate.get("subject"))
        .ok_or_else(|| "candidate has no Change Request identity".to_string())?;
    let subject_id = subject
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "Change Request identity has no ID".to_string())?;
    let repository = subject_id
        .rsplit_once(":change_request:")
        .map(|(repository, _)| repository)
        .ok_or_else(|| "Change Request identity is not canonical".to_string())?;
    let head = candidate
        .get("head")
        .and_then(Value::as_str)
        .ok_or_else(|| "candidate has no exact successor head".to_string())?;
    Ok(json!({
        "effect_id":format!("resolve-review-threads:{subject_id}:{head}"),
        "idempotency_key":format!("resolve-review-threads:{subject_id}:{head}"),
        "authority_scope":"provider:write",
        "preconditions":{
            "repository":{"id":repository,"revision":head},
            "worktree_session":null,
            "expected_head":head,
            "target_repository":{"id":repository,"revision":head},
            "policy_revision":null,
            "gate_revisions":{}
        },
        "parameters":{"change_request":subject,"threads":[]}
    }))
}

fn addressed_threads(
    consumed: &Value,
    observation: &Value,
    report: &Value,
) -> Result<Vec<Value>, String> {
    let addressed = report
        .get("addressed_thread_ids")
        .and_then(Value::as_array)
        .ok_or_else(|| "repair report has no addressed thread IDs".to_string())?;
    let mut addressed: std::collections::BTreeSet<_> =
        addressed.iter().filter_map(Value::as_str).collect();
    let consumed_ids = consumed
        .get("threads")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|thread| thread.get("resolved").and_then(Value::as_bool) == Some(false))
        .filter_map(|thread| thread.get("id").and_then(Value::as_str))
        .collect::<std::collections::BTreeSet<_>>();
    addressed.retain(|id| consumed_ids.contains(id));
    let artifact = report
        .get("artifact_id")
        .and_then(Value::as_str)
        .unwrap_or("repair-report");
    let mut threads = observation
        .get("threads")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|thread| thread.get("resolved").and_then(Value::as_bool) == Some(false))
        .filter(|thread| {
            thread
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| addressed.contains(id))
        })
        .map(|thread| {
            json!({
                "id":thread["id"],
                "observed_revision":thread["revision"],
                "addressed_by_artifact":artifact
            })
        })
        .collect::<Vec<_>>();
    threads.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    Ok(threads)
}

fn filter_review(input: Value) -> Result<Value, String> {
    if input.get("quality").and_then(Value::as_str) != Some("current") {
        return Err("review filtering requires a current complete observation".into());
    }
    let observation_revision = input
        .get("revision")
        .and_then(Value::as_str)
        .filter(|revision| !revision.trim().is_empty())
        .ok_or_else(|| "review observation requires an exact revision".to_string())?;
    let after = input.get("after").and_then(Value::as_str);
    let authors = input
        .get("authors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let mut threads = input
        .get("threads")
        .and_then(Value::as_array)
        .ok_or_else(|| "review observation requires threads".to_string())?
        .iter()
        .filter(|thread| thread.get("resolved").and_then(Value::as_bool) == Some(false))
        .filter(|thread| non_empty_string(thread, "id") && non_empty_string(thread, "body"))
        .filter(|thread| {
            authors.is_empty()
                || thread
                    .get("author")
                    .and_then(Value::as_str)
                    .is_some_and(|author| authors.contains(&author))
        })
        .filter(|thread| {
            after.is_none_or(|after| {
                thread
                    .get("created_at")
                    .and_then(Value::as_str)
                    .is_some_and(|created| created > after)
            })
        })
        .map(|thread| {
            json!({
                "id": thread["id"],
                "observed_revision": thread.get("revision").unwrap_or(&Value::Null),
                "body": thread["body"],
                "author": thread.get("author"),
                "path": thread.get("path"),
                "line": thread.get("line")
            })
        })
        .collect::<Vec<_>>();
    threads.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    Ok(json!({
        "observation_revision": observation_revision,
        "actionable": !threads.is_empty(),
        "threads": threads
    }))
}

fn render_prompt(input: Value) -> Result<Value, String> {
    let template = input
        .get("template")
        .and_then(Value::as_str)
        .ok_or_else(|| "prompt rendering requires a template".to_string())?;
    let values = input
        .get("values")
        .and_then(Value::as_object)
        .ok_or_else(|| "prompt rendering requires an object of values".to_string())?;
    let mut rendered = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        rendered.push_str(&rest[..start]);
        let Some(end) = rest[start + 1..].find('}') else {
            rendered.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let placeholder_end = start + end + 2;
        let key = &rest[start + 1..placeholder_end - 1];
        match values.get(key) {
            Some(Value::String(value)) => rendered.push_str(value),
            Some(value) => rendered.push_str(&value.to_string()),
            None => rendered.push_str(&rest[start..placeholder_end]),
        }
        rest = &rest[placeholder_end..];
    }
    rendered.push_str(rest);
    Ok(json!({"text":rendered}))
}

fn evaluate_gate(input: Value) -> Result<Value, String> {
    let quality = input
        .get("quality")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let status = match quality {
        "current" => {
            input
                .get("satisfied")
                .and_then(Value::as_bool)
                .map_or("unknown", |satisfied| {
                    if satisfied {
                        "satisfied"
                    } else {
                        "unsatisfied"
                    }
                })
        }
        "unsupported" => "unavailable",
        "stale" | "partial" | "failed" | "unknown" | "unavailable" => "unknown",
        _ => return Err(format!("unknown evidence quality '{quality}'")),
    };
    Ok(json!({
        "status": status,
        "subject": input.get("subject"),
        "evidence_revision": input.get("revision"),
        "policy_revision": input.get("policy_revision"),
        "quality": quality
    }))
}

fn implementations() -> Vec<ImplementationDescriptor> {
    let mut descriptors = vec![
        implementation(
            "prism.standard/echo",
            StepClass::Action,
            EffectBoundary::None,
            &[],
            &[],
        ),
        implementation(
            "prism.standard/plan-parse",
            StepClass::Action,
            EffectBoundary::None,
            &[PLAN_SOURCE],
            &[PLAN],
        ),
        implementation(
            "prism.standard/agent",
            StepClass::Action,
            // The host records and bounds the harness process, but an agent can still mutate the
            // workspace directly with the user's OS authority.
            EffectBoundary::Unbrokered,
            &[AGENT_REQUEST],
            &[AGENT_RESULT],
        ),
        implementation(
            "prism.standard/command",
            StepClass::Action,
            // Arbitrary commands can bypass protected effect APIs and must not claim their
            // reconciliation guarantees.
            EffectBoundary::Unbrokered,
            &[PROCESS_REQUEST],
            &[PROCESS_RESULT],
        ),
        implementation(
            "prism.standard/verify",
            StepClass::Action,
            EffectBoundary::None,
            &[PROCESS_REQUEST],
            &[PROCESS_RESULT],
        ),
        implementation(
            "prism.standard/provider-observe",
            StepClass::Action,
            EffectBoundary::None,
            &[PROVIDER_OBSERVATION_REQUEST],
            &[PROVIDER_OBSERVATION],
        ),
        implementation(
            "prism.standard/review-filter",
            StepClass::Action,
            EffectBoundary::None,
            &[REVIEW_OBSERVATION],
            &[REVIEW_REPORT],
        ),
        implementation(
            "prism.standard/render-prompt",
            StepClass::Action,
            EffectBoundary::None,
            &[PROMPT_TEMPLATE],
            &[PROMPT],
        ),
        implementation(
            "prism.standard/git-commit",
            StepClass::Action,
            EffectBoundary::Brokered,
            &[EFFECT_REQUEST],
            &[EFFECT_RESULT],
        ),
        implementation(
            "prism.standard/git-push",
            StepClass::Action,
            EffectBoundary::Brokered,
            &[EFFECT_REQUEST],
            &[EFFECT_RESULT],
        ),
        implementation(
            "prism.standard/squash-merge",
            StepClass::Action,
            EffectBoundary::Brokered,
            &[EFFECT_REQUEST],
            &[EFFECT_RESULT],
        ),
        implementation(
            "prism.standard/worktrunk-delete",
            StepClass::Action,
            EffectBoundary::Brokered,
            &[EFFECT_REQUEST],
            &[EFFECT_RESULT],
        ),
        implementation(
            "prism.standard/gate",
            StepClass::Gate,
            EffectBoundary::None,
            &[PROVIDER_OBSERVATION],
            &[GATE_RESULT],
        ),
        implementation(
            "prism.standard/approval-presentation",
            StepClass::Approval,
            EffectBoundary::None,
            &[GATE_RESULT],
            &[APPROVAL_PRESENTATION],
        ),
        implementation(
            "prism.standard/notification",
            StepClass::Notification,
            EffectBoundary::None,
            &[NOTIFICATION],
            &[],
        ),
    ];
    descriptors.extend(semantic_implementations());
    descriptors
}

fn semantic_implementations() -> Vec<ImplementationDescriptor> {
    let action = StepClass::Action;
    vec![
        named_implementation(
            "prism.standard/plan-approval",
            StepClass::Approval,
            EffectBoundary::None,
            &[("plan", PLAN)],
            &[],
            &[],
        ),
        named_implementation(
            "prism.standard/plan-phase-task",
            action,
            EffectBoundary::None,
            &[("plan", PLAN)],
            &[("task", TASK)],
            &[],
        ),
        named_implementation(
            "prism.standard/agent-implement",
            action,
            EffectBoundary::Unbrokered,
            &[("task", TASK), ("worktree", WORKTREE)],
            &[("candidate", CANDIDATE_CHANGE)],
            &["agent:run", "workspace:write"],
        ),
        named_implementation(
            "prism.standard/local-verification",
            StepClass::Gate,
            EffectBoundary::None,
            &[("candidate", CANDIDATE_CHANGE)],
            &[("result", GATE_RESULT)],
            &["process:run"],
        ),
        named_implementation(
            "prism.standard/self-review",
            StepClass::Gate,
            EffectBoundary::None,
            &[("candidate", CANDIDATE_CHANGE)],
            &[("result", GATE_RESULT)],
            &["agent:run"],
        ),
        named_implementation(
            "prism.standard/verified-candidate",
            action,
            EffectBoundary::None,
            &[
                ("candidate", CANDIDATE_CHANGE),
                ("local", GATE_RESULT),
                ("review", GATE_RESULT),
            ],
            &[
                ("candidate", CANDIDATE_CHANGE),
                ("next_task", TASK),
                ("plan_complete", BOOLEAN),
            ],
            &[],
        ),
        named_implementation(
            "prism.standard/commit-candidate",
            action,
            EffectBoundary::Brokered,
            &[("candidate", CANDIDATE_CHANGE)],
            &[("candidate", CANDIDATE_CHANGE)],
            &["git:write"],
        ),
        named_implementation(
            "prism.standard/push-candidate",
            action,
            EffectBoundary::Brokered,
            &[("candidate", CANDIDATE_CHANGE)],
            &[("candidate", CANDIDATE_CHANGE)],
            &["git:write"],
        ),
        named_implementation(
            "prism.standard/create-change-request",
            action,
            EffectBoundary::Brokered,
            &[("candidate", CANDIDATE_CHANGE)],
            &[("candidate", CHANGE_REQUEST)],
            &["provider:write"],
        ),
        named_implementation(
            "prism.standard/observe-ci",
            action,
            EffectBoundary::None,
            &[("candidate", CHANGE_REQUEST)],
            &[("evidence", PROVIDER_OBSERVATION)],
            &["provider:read"],
        ),
        named_implementation(
            "prism.standard/observe-review",
            action,
            EffectBoundary::None,
            &[("candidate", CHANGE_REQUEST)],
            &[("evidence", PROVIDER_OBSERVATION)],
            &["provider:read"],
        ),
        named_implementation(
            "prism.standard/observe-policy",
            action,
            EffectBoundary::None,
            &[("candidate", CHANGE_REQUEST)],
            &[("evidence", PROVIDER_OBSERVATION)],
            &["provider:read"],
        ),
        named_implementation(
            "prism.standard/observe-mergeability",
            action,
            EffectBoundary::None,
            &[("candidate", CHANGE_REQUEST)],
            &[("evidence", PROVIDER_OBSERVATION)],
            &["provider:read"],
        ),
        named_implementation(
            "prism.standard/observe-merge-relation",
            action,
            EffectBoundary::None,
            &[("candidate", CHANGE_REQUEST)],
            &[("evidence", PROVIDER_OBSERVATION)],
            &["provider:read"],
        ),
        named_implementation(
            "prism.standard/choose-stabilization-repair",
            action,
            EffectBoundary::None,
            &[
                ("candidate", CHANGE_REQUEST),
                ("ci", PROVIDER_OBSERVATION),
                ("review", PROVIDER_OBSERVATION),
                ("policy", PROVIDER_OBSERVATION),
                ("mergeability", PROVIDER_OBSERVATION),
                ("merge_relation", PROVIDER_OBSERVATION),
            ],
            &[("needs_repair", BOOLEAN), ("repair_kind", REPAIR_KIND)],
            &[],
        ),
        named_implementation(
            "prism.standard/agent-repair-ci",
            action,
            EffectBoundary::Unbrokered,
            &[
                ("candidate", CHANGE_REQUEST),
                ("evidence", PROVIDER_OBSERVATION),
                ("worktree", WORKTREE),
            ],
            &[("candidate", CANDIDATE_CHANGE)],
            &["agent:run", "workspace:write"],
        ),
        named_implementation(
            "prism.standard/agent-repair-review",
            action,
            EffectBoundary::Unbrokered,
            &[
                ("candidate", CHANGE_REQUEST),
                ("evidence", PROVIDER_OBSERVATION),
                ("worktree", WORKTREE),
            ],
            &[("candidate", CANDIDATE_CHANGE), ("report", REPAIR_REPORT)],
            &["agent:run", "workspace:write"],
        ),
        named_implementation(
            "prism.standard/verify-repair",
            StepClass::Gate,
            EffectBoundary::None,
            &[
                ("ci_candidate", CANDIDATE_CHANGE),
                ("review_candidate", CANDIDATE_CHANGE),
            ],
            &[("result", GATE_RESULT)],
            &["process:run"],
        ),
        named_implementation(
            "prism.standard/commit-repair",
            action,
            EffectBoundary::Brokered,
            &[
                ("verification", GATE_RESULT),
                ("ci_candidate", CANDIDATE_CHANGE),
                ("review_candidate", CANDIDATE_CHANGE),
            ],
            &[("candidate", CANDIDATE_CHANGE)],
            &["git:write"],
        ),
        named_implementation(
            "prism.standard/push-successor",
            action,
            EffectBoundary::Brokered,
            &[("candidate", CANDIDATE_CHANGE)],
            &[("candidate", CHANGE_REQUEST)],
            &["git:write"],
        ),
        named_implementation(
            "prism.standard/resolve-addressed-threads",
            action,
            EffectBoundary::Brokered,
            &[
                ("candidate", CHANGE_REQUEST),
                ("report", REPAIR_REPORT),
                ("observation", PROVIDER_OBSERVATION),
            ],
            &[("candidate", CHANGE_REQUEST)],
            &["provider:write"],
        ),
        named_implementation(
            "prism.standard/reobserve-change-request",
            action,
            EffectBoundary::None,
            &[("original", CHANGE_REQUEST), ("successor", CHANGE_REQUEST)],
            &[("observation", REOBSERVATION)],
            &["provider:read"],
        ),
        named_implementation(
            "prism.standard/summarize-stabilization",
            action,
            EffectBoundary::None,
            &[("observation", REOBSERVATION)],
            &[
                ("candidate", CHANGE_REQUEST),
                ("ready", BOOLEAN),
                ("ci", PROVIDER_OBSERVATION),
                ("review", PROVIDER_OBSERVATION),
                ("policy", PROVIDER_OBSERVATION),
                ("mergeability", PROVIDER_OBSERVATION),
                ("merge_relation", PROVIDER_OBSERVATION),
            ],
            &[],
        ),
        named_implementation(
            "prism.standard/merge-approval",
            StepClass::Approval,
            EffectBoundary::None,
            &[
                ("candidate", CHANGE_REQUEST),
                ("ci", GATE_RESULT),
                ("review", GATE_RESULT),
                ("policy", GATE_RESULT),
                ("mergeability", GATE_RESULT),
                ("merge_relation", GATE_RESULT),
            ],
            &[],
            &[],
        ),
        named_implementation(
            "prism.standard/revalidate-merge",
            StepClass::Gate,
            EffectBoundary::None,
            &[("observation", REOBSERVATION)],
            &[("result", GATE_RESULT)],
            &["provider:read"],
        ),
        named_implementation(
            "prism.standard/squash-merge-exact-head",
            action,
            EffectBoundary::Brokered,
            &[("candidate", CHANGE_REQUEST), ("gates", GATE_RESULT)],
            &[("merged", MERGED_CHANGE)],
            &["provider:write"],
        ),
        named_implementation(
            "prism.standard/delete-exact-worktree",
            action,
            EffectBoundary::Brokered,
            &[("worktree", WORKTREE), ("merged", MERGED_CHANGE)],
            &[("notification", NOTIFICATION)],
            &["worktrunk:write"],
        ),
        named_implementation(
            "prism.standard/classify-issue",
            action,
            EffectBoundary::None,
            &[("intake", ISSUE_INTAKE)],
            &[("classification", ISSUE_CLASSIFICATION)],
            &["provider:read"],
        ),
        named_implementation(
            "prism.standard/issue-admission",
            StepClass::Approval,
            EffectBoundary::None,
            &[("classification", ISSUE_CLASSIFICATION)],
            &[],
            &[],
        ),
    ]
}

fn named_implementation(
    id: &str,
    class: StepClass,
    effect_boundary: EffectBoundary,
    inputs: &[(&str, &str)],
    outputs: &[(&str, &str)],
    capabilities: &[&str],
) -> ImplementationDescriptor {
    ImplementationDescriptor {
        id: id.into(),
        class,
        inputs: inputs
            .iter()
            .map(|(name, schema)| PortDescriptor {
                name: (*name).into(),
                schema: (*schema).into(),
                required: !matches!(*name, "ci_candidate" | "review_candidate" | "successor"),
            })
            .collect(),
        outputs: outputs
            .iter()
            .map(|(name, schema)| PortDescriptor {
                name: (*name).into(),
                schema: (*schema).into(),
                required: true,
            })
            .collect(),
        capabilities: capabilities.iter().map(|value| (*value).into()).collect(),
        targets: vec!["local".into()],
        effect_boundary,
    }
}

fn implementation(
    id: &str,
    class: StepClass,
    effect_boundary: EffectBoundary,
    inputs: &[&str],
    outputs: &[&str],
) -> ImplementationDescriptor {
    ImplementationDescriptor {
        id: id.into(),
        class,
        inputs: inputs
            .iter()
            .map(|schema| PortDescriptor {
                name: "request".into(),
                schema: (*schema).into(),
                required: true,
            })
            .collect(),
        outputs: outputs
            .iter()
            .map(|schema| PortDescriptor {
                name: "result".into(),
                schema: (*schema).into(),
                required: true,
            })
            .collect(),
        capabilities: capabilities(id),
        targets: vec!["local".into()],
        effect_boundary,
    }
}

fn capabilities(id: &str) -> Vec<String> {
    match id {
        "prism.standard/agent" => vec!["agent:run".into(), "workspace:write".into()],
        "prism.standard/command" => vec!["process:run".into(), "workspace:write".into()],
        "prism.standard/verify" => vec!["process:run".into()],
        "prism.standard/provider-observe" | "prism.standard/gate" => vec!["provider:read".into()],
        "prism.standard/git-commit" | "prism.standard/git-push" => vec!["git:write".into()],
        "prism.standard/create-change-request"
        | "prism.standard/resolve-addressed-threads"
        | "prism.standard/squash-merge" => vec!["provider:write".into()],
        "prism.standard/worktrunk-delete" => vec!["worktrunk:write".into()],
        _ => Vec::new(),
    }
}

fn artifact_schemas() -> Vec<ArtifactSchemaDescriptor> {
    [
        (
            TASK,
            json!({
                "type":"object",
                "additionalProperties":true,
                "properties":{
                    "title":{"type":"string", "minLength":1},
                    "body":{"type":"string", "minLength":1}
                },
                "anyOf":[{"required":["title"]},{"required":["body"]}]
            }),
        ),
        (
            PLAN_SOURCE,
            json!({
                "type":"object",
                "additionalProperties":true,
                "properties":{
                    "source":{"type":"string", "minLength":1},
                    "steps":{"type":"array", "minItems":1, "items":{"type":"object"}}
                },
                "anyOf":[{"required":["source"]},{"required":["steps"]}]
            }),
        ),
        (PLAN, object_schema(&["steps"], &[])),
        (WORKTREE, object_schema(&["id", "revision"], &[])),
        (
            PROCESS_REQUEST,
            object_schema(
                &[
                    "executable",
                    "working_scope",
                    "timeout_ms",
                    "max_output_bytes",
                ],
                &[],
            ),
        ),
        (PROCESS_RESULT, object_schema(&["status"], &[])),
        (
            AGENT_REQUEST,
            object_schema(
                &[
                    "harness",
                    "prompt",
                    "working_scope",
                    "tool_policy",
                    "timeout_ms",
                    "max_output_bytes",
                ],
                &[],
            ),
        ),
        (AGENT_RESULT, object_schema(&["status"], &[])),
        (
            PROVIDER_OBSERVATION_REQUEST,
            object_schema(&["subject", "operation"], &[]),
        ),
        (
            PROVIDER_OBSERVATION,
            object_schema(&["quality", "revision"], &[]),
        ),
        (
            REVIEW_OBSERVATION,
            object_schema(&["quality", "revision", "threads"], &[]),
        ),
        (
            REVIEW_REPORT,
            object_schema(&["observation_revision", "threads"], &[]),
        ),
        (PROMPT_TEMPLATE, object_schema(&["template", "values"], &[])),
        (PROMPT, object_schema(&["text"], &[])),
        (
            EFFECT_REQUEST,
            object_schema(
                &[
                    "effect_id",
                    "idempotency_key",
                    "authority_scope",
                    "preconditions",
                    "parameters",
                ],
                &[],
            ),
        ),
        (EFFECT_RESULT, object_schema(&["status"], &[])),
        (
            GATE_EVIDENCE,
            object_schema(&["quality", "revision", "policy_revision"], &[]),
        ),
        (GATE_RESULT, object_schema(&["status"], &[])),
        (APPROVAL_PRESENTATION, object_schema(&["summary"], &[])),
        (NOTIFICATION, object_schema(&["body"], &[])),
        (
            CANDIDATE_CHANGE,
            object_schema(&[], &["head", "agent_result"]),
        ),
        (
            CHANGE_REQUEST,
            object_schema(&[], &["head", "change_request"]),
        ),
        (BOOLEAN, json!({"type":"boolean"})),
        (
            REPAIR_KIND,
            json!({"type":"string", "enum":["none", "ci", "review", "blocked"]}),
        ),
        (REPAIR_REPORT, object_schema(&["addressed_thread_ids"], &[])),
        (REOBSERVATION, object_schema(&["candidate", "facts"], &[])),
        (MERGED_CHANGE, object_schema(&["status"], &[])),
        (
            ISSUE_INTAKE,
            json!({
                "type":"object",
                "additionalProperties":true,
                "required":["issue"],
                "properties":{"issue":{"type":"object"}}
            }),
        ),
        (
            ISSUE_CLASSIFICATION,
            object_schema(&["issue", "quarantined", "admission_required"], &[]),
        ),
    ]
    .into_iter()
    .map(|(id, schema)| ArtifactSchemaDescriptor {
        id: id.into(),
        schema,
    })
    .collect()
}

fn object_schema(required: &[&str], any_of: &[&str]) -> Value {
    let mut schema = json!({"type":"object", "additionalProperties":true});
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    if !any_of.is_empty() {
        schema["anyOf"] = Value::Array(
            any_of
                .iter()
                .map(|field| json!({"required":[field]}))
                .collect(),
        );
    }
    schema
}

fn non_empty_string(value: &Value, field: &str) -> bool {
    value
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
}

fn artifact_summary(schema_id: &str, value: &Value) -> String {
    let label = schema_id
        .strip_prefix("prism.")
        .unwrap_or(schema_id)
        .replace(['/', '-'], " ");
    let detail = value
        .get("summary")
        .or_else(|| value.get("title"))
        .or_else(|| value.get("status"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string());
    format!("{label}: {detail}")
}

fn render_style(schema_id: &str) -> &'static str {
    if schema_id == GATE_RESULT || schema_id == EFFECT_RESULT {
        "status"
    } else {
        "plain"
    }
}

fn truncate(value: &str, maximum: usize) -> String {
    if value.chars().count() <= maximum {
        return value.into();
    }
    let mut truncated = value
        .chars()
        .take(maximum.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

#[tokio::main]
async fn main() {
    if let Err(error) = prism_extension_sdk::serve(StandardExtension).await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_protected_implementations_are_disclosed_as_brokered() {
        let descriptors = implementations();
        for id in [
            "prism.standard/git-commit",
            "prism.standard/git-push",
            "prism.standard/create-change-request",
            "prism.standard/resolve-addressed-threads",
            "prism.standard/squash-merge",
            "prism.standard/worktrunk-delete",
        ] {
            assert_eq!(
                descriptors
                    .iter()
                    .find(|item| item.id == id)
                    .unwrap()
                    .effect_boundary,
                EffectBoundary::Brokered
            );
        }
    }

    #[test]
    fn plan_parser_extracts_ordered_markdown_steps() {
        assert_eq!(
            parse_plan(json!({"source":"# Plan\n- inspect\n- implement\n"})).unwrap()["steps"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn stale_gate_evidence_cannot_satisfy_a_gate() {
        let result = evaluate_gate(json!({"quality":"stale", "satisfied":true})).unwrap();
        assert_eq!(result["status"], "unknown");
    }

    #[test]
    fn review_filter_preserves_exact_thread_provenance() {
        let report = filter_review(json!({
            "quality":"current",
            "revision":"review-observation-7",
            "threads":[
                {"id":"thread-b","revision":"thread-b-2","resolved":false,"body":"fix b","author":"reviewer","created_at":"2026-08-02"},
                {"id":"thread-a","revision":"thread-a-1","resolved":false,"body":"fix a","author":"reviewer","created_at":"2026-08-01"},
                {"id":"thread-c","revision":"thread-c-1","resolved":true,"body":"done","author":"reviewer","created_at":"2026-08-01"}
            ]
        }))
        .unwrap();
        assert_eq!(report["threads"][0]["id"], "thread-a");
        assert_eq!(report["threads"][1]["observed_revision"], "thread-b-2");
        assert_eq!(report["observation_revision"], "review-observation-7");
    }

    #[test]
    fn prompt_rendering_does_not_expand_inserted_placeholders() {
        let prompt = render_prompt(json!({
            "template":"Review {title} at {url}",
            "values":{"title":"a {url}","url":"https://example.test/1"}
        }))
        .unwrap();
        assert_eq!(prompt["text"], "Review a {url} at https://example.test/1");
    }

    #[test]
    fn stabilization_selects_at_most_one_repair_and_stops_on_capability_gaps() {
        let current = |satisfied| json!({"quality":"current","revision":"r1","head":"abc1234","satisfied":satisfied});
        let decision = choose_stabilization_repair(json!({
            "candidate":{"head":"abc1234"},
            "ci":current(false), "review":current(false), "policy":current(true),
            "mergeability":current(true), "merge_relation":current(true)
        }))
        .unwrap();
        assert_eq!(decision["repair_kind"], "ci");
        assert_eq!(decision["needs_repair"], true);

        let error = choose_stabilization_repair(json!({
            "candidate":{"head":"abc1234"},
            "ci":current(true), "review":current(true), "policy":current(true),
            "mergeability":{"quality":"unsupported","head":"abc1234","reason":"Forgejo cannot report mergeability"},
            "merge_relation":current(true)
        }))
        .unwrap_err();
        assert!(error.contains("unsupported"));
        assert!(error.contains("Forgejo"));
    }

    #[test]
    fn thread_resolution_is_the_exact_addressed_still_unresolved_intersection() {
        let consumed = json!({"threads":[
            {"id":"T1","revision":"T1-r1","resolved":false},
            {"id":"T2","revision":"T2-r1","resolved":false},
            {"id":"T3","revision":"T3-r1","resolved":true}
        ]});
        let current = json!({"threads":[
            {"id":"T1","revision":"T1-r1","resolved":false},
            {"id":"T2","revision":"T2-r2","resolved":false},
            {"id":"T3","revision":"T3-r1","resolved":true},
            {"id":"T4","revision":"T4-r1","resolved":false}
        ]});
        let threads = addressed_threads(
            &consumed,
            &current,
            &json!({"artifact_id":"repair-a7","addressed_thread_ids":["T2","T3","T4","unknown"]}),
        )
        .unwrap();
        assert_eq!(
            threads,
            vec![json!({
                "id":"T2", "observed_revision":"T2-r2", "addressed_by_artifact":"repair-a7"
            })]
        );
    }

    #[test]
    fn stabilization_infers_exact_brokered_merge_and_cleanup_intents() {
        let candidate = json!({
            "change_request":{"id":"github:github.com:acme/prism:change_request:PR_42","revision":"abc1234"},
            "head":"abc1234"
        });
        let merge = inferred_effect_request(
            "squash_merge",
            &json!({"gates":{
                "policy_revision":"policy-7",
                "gate_revisions":{"ci":"ci-7","review":"review-7"}
            }}),
            &candidate,
        )
        .unwrap();
        assert_eq!(merge["preconditions"]["expected_head"], "abc1234");
        assert_eq!(merge["parameters"]["method"], "squash");
        assert_eq!(merge["authority_scope"], "provider:write");

        let worktree = json!({
            "id":"/repo:/repo.fix",
            "revision":"incarnation-7",
            "repository":"/repo",
            "path":"/repo.fix",
            "branch":"fix/thing"
        });
        let cleanup = inferred_effect_request(
            "delete_worktree",
            &json!({"worktree":worktree}),
            &json!({"status":"merged"}),
        )
        .unwrap();
        assert_eq!(cleanup["parameters"]["expected_path"], "/repo.fix");
        assert_eq!(
            cleanup["preconditions"]["worktree_session"]["revision"],
            "incarnation-7"
        );
        assert_eq!(cleanup["authority_scope"], "worktrunk:write");
    }

    #[test]
    fn every_exact_head_fact_must_be_current_before_stabilization_is_ready() {
        let facts = json!({
            "ci":{"quality":"current","head":"abc1234","satisfied":true},
            "review":{"quality":"current","head":"abc1234","satisfied":true},
            "policy":{"quality":"current","head":"abc1234","satisfied":true},
            "mergeability":{"quality":"current","head":"abc1234","satisfied":true},
            "merge_relation":{"quality":"stale","head":"abc1234","satisfied":true}
        });
        let result = summarize_stabilization(json!({
            "observation":{"candidate":{"head":"abc1234"},"facts":facts}
        }))
        .unwrap();
        assert_eq!(result["ready"], false);
        assert_eq!(result["candidate"]["head"], "abc1234");
    }
}
