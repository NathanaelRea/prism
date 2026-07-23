use super::*;

pub(super) fn emit_auto_run_log(run: &AutoRun) {
    observability::emit(observability::EventInput {
        level: LogLevel::Debug,
        target: "auto_flow",
        action: "run_state",
        operation_id: None,
        parent_operation_id: None,
        branch: Some(run.branch.clone()),
        session: None,
        message: format!("Auto Flow run {} is {}", run.id, run.status.as_str()),
        data_json: Some(format!(
            "{{\"run_id\":{},\"status\":{},\"mode\":{},\"pause_requested\":{},\"pr_number\":{},\"current_head_sha\":{}}}",
            json_string(&run.id),
            json_string(run.status.as_str()),
            json_string(run.mode.as_str()),
            run.pause_requested,
            run.pr_number
                .map(|number| number.to_string())
                .unwrap_or_else(|| "null".to_string()),
            run.current_head_sha
                .as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".to_string())
        )),
    });
}

pub(super) fn emit_auto_step_log(step: &AutoStepRun) {
    observability::emit(observability::EventInput {
        level: LogLevel::Debug,
        target: "auto_flow",
        action: "step_state",
        operation_id: None,
        parent_operation_id: None,
        branch: None,
        session: step.session.id.clone(),
        message: format!(
            "Auto Flow step {} attempt {} is {}",
            step.step_key.as_str(),
            step.attempt,
            step.status.as_str()
        ),
        data_json: Some(format!(
            "{{\"run_id\":{},\"step_run_id\":{},\"sequence\":{},\"step_key\":{},\"attempt\":{},\"status\":{},\"process_id\":{},\"commit_sha\":{},\"head_sha\":{}}}",
            json_string(&step.run_id),
            step.id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "null".to_string()),
            step.sequence,
            json_string(step.step_key.as_str()),
            step.attempt,
            json_string(step.status.as_str()),
            step.execution
                .process_id
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "null".to_string()),
            step.commit_sha
                .as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".to_string()),
            step.head_sha
                .as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".to_string())
        )),
    });
}

pub(super) fn emit_auto_event_log(event: &AutoEvent) {
    observability::emit(observability::EventInput {
        level: LogLevel::Info,
        target: "auto_flow",
        action: event.kind.as_str(),
        operation_id: None,
        parent_operation_id: None,
        branch: None,
        session: None,
        message: format!("Auto Flow event {}", event.kind),
        data_json: Some(format!(
            "{{\"run_id\":{},\"step_run_id\":{},\"kind\":{}}}",
            json_string(&event.run_id),
            event
                .step_run_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "null".to_string()),
            json_string(&event.kind)
        )),
    });
}

pub(super) fn summarize_prompt(prompt: &str) -> String {
    let collapsed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= 96 {
        return collapsed;
    }
    let mut summary = collapsed.chars().take(93).collect::<String>();
    summary.push_str("...");
    summary
}

pub(super) fn stable_string_hash(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

pub(super) fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}
