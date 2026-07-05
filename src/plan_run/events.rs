use super::*;

pub fn parse_plan_agent_events(raw: &str) -> Vec<PlanAgentEvent> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return vec![PlanAgentEvent::AssistantText {
            text: trimmed.to_string(),
        }];
    };
    let event_type = string_field_deep(&value, &["type", "event", "name"])
        .unwrap_or_else(|| "event".to_string());
    let lower_type = event_type.to_ascii_lowercase();
    let mut events = Vec::new();

    if lower_type == "server.connected" {
        events.push(PlanAgentEvent::StateChanged {
            state: "connected".to_string(),
        });
    }

    if let Some(session_id) = string_field_deep(&value, &["session_id", "sessionID", "sessionId"])
        .or_else(|| nested_string(&value, &["session", "id"]))
    {
        events.push(PlanAgentEvent::SessionIdentified {
            session_id,
            title: string_field_deep(&value, &["title"]),
        });
    }

    if let Some(state) = string_field_deep(&value, &["status", "state"])
        && (lower_type.contains("status") || lower_type.contains("state"))
    {
        events.push(PlanAgentEvent::StateChanged { state });
    } else if lower_type == "session.idle" {
        events.push(PlanAgentEvent::StateChanged {
            state: "idle".to_string(),
        });
    } else if lower_type == "session.error" {
        events.push(PlanAgentEvent::StateChanged {
            state: "error".to_string(),
        });
    }

    if let Some(todos) = todos_from_value(&value)
        && !todos.is_empty()
    {
        events.push(PlanAgentEvent::TodoUpdated { todos });
    }

    if lower_type.contains("error")
        && let Some(message) = string_field_deep(&value, &["error", "message"])
    {
        events.push(PlanAgentEvent::Error { message });
    }

    if (lower_type.contains("diff") || string_field_deep(&value, &["patch", "path"]).is_some())
        && let Some(summary) =
            string_field_deep(&value, &["summary", "path"]).or_else(|| Some("diff updated".into()))
    {
        events.push(PlanAgentEvent::DiffUpdated {
            summary,
            patch: string_field_deep(&value, &["patch"]),
        });
    }

    if lower_type.contains("tool") {
        let id = string_field_deep(&value, &["id", "tool_call_id", "call_id"]);
        let name = string_field_deep(&value, &["tool", "name"]).unwrap_or_else(|| "tool".into());
        let status = string_field_deep(&value, &["status", "state"]);
        let args_summary = string_field_deep(&value, &["command", "description", "input"]);
        let output = string_field_deep(&value, &["output", "stdout", "stderr"]);
        if let Some(text) = output {
            events.push(PlanAgentEvent::ToolOutput {
                id: id.clone(),
                text,
            });
        } else if lower_type.contains(".after")
            || lower_type.contains("after")
            || matches!(
                status.as_deref(),
                Some("done" | "failed" | "error" | "completed" | "complete" | "success")
            )
        {
            events.push(PlanAgentEvent::ToolFinished {
                id: id.clone(),
                status: status.unwrap_or_else(|| "done".into()),
            });
        } else {
            events.push(PlanAgentEvent::ToolStarted {
                id: id.clone(),
                name,
                args_summary,
            });
        }
    }

    if !lower_type.contains("tool")
        && !lower_type.contains("status")
        && !lower_type.contains("diff")
        && !lower_type.contains("todo")
        && let Some(text) = string_field_deep(&value, &["text", "content", "message", "summary"])
    {
        events.push(PlanAgentEvent::AssistantText { text });
    }

    if events.is_empty() || should_keep_raw(&event_type, &value) {
        events.push(PlanAgentEvent::Raw {
            event_type,
            json: trimmed.to_string(),
        });
    }
    events
}

pub fn ingest_plan_sse_payload(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
    raw: &str,
    max_output_lines_per_step: usize,
) -> Result<bool, String> {
    if serde_json::from_str::<Value>(raw.trim()).is_err() {
        return Ok(false);
    }
    let events = parse_plan_agent_events(raw);
    if events.is_empty() || !events_match_step_session(step, &events) {
        return Ok(false);
    }
    ingest_plan_agent_events(conn, step, events, max_output_lines_per_step)?;
    Ok(true)
}

pub fn reconcile_plan_step_from_server(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
    max_output_lines_per_step: usize,
) -> Result<bool, String> {
    let (Some(server_url), Some(session_id)) = (
        step.opencode_server_url.as_deref(),
        step.opencode_session_id.as_deref(),
    ) else {
        return Ok(false);
    };
    let status = crate::opencode::poll_session_status(server_url, session_id)?;
    reconcile_plan_step_from_opencode_status(conn, step, &status, max_output_lines_per_step)
}

pub fn reconcile_plan_step_from_opencode_status(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
    status: &OpencodeStatus,
    max_output_lines_per_step: usize,
) -> Result<bool, String> {
    let mut changed = false;
    if let Some(status_session_id) = status.session_id.as_deref() {
        if let Some(step_session_id) = step.opencode_session_id.as_deref()
            && step_session_id != status_session_id
        {
            return Ok(false);
        }
        if step.opencode_session_id.is_none() {
            step.opencode_session_id = Some(status_session_id.to_string());
            changed = true;
        }
    }
    if step.opencode_server_url.is_none() {
        changed |= status.server_url.is_some();
        step.opencode_server_url = status.server_url.clone();
    }
    changed |= step.opencode_state != Some(status.state);
    step.opencode_state = Some(status.state);

    let mut events = Vec::new();
    if let Some(session_id) = status.session_id.clone() {
        events.push(PlanAgentEvent::SessionIdentified {
            session_id,
            title: status.title.clone(),
        });
    }
    events.push(PlanAgentEvent::StateChanged {
        state: status.state.label().to_string(),
    });
    if let Some(text) = status.latest_message.as_ref()
        && step.latest_message.as_deref() != Some(text.as_str())
    {
        changed = true;
        events.push(PlanAgentEvent::AssistantText { text: text.clone() });
    }
    if let Some(tool) = status.active_tool.as_ref()
        && step.active_tool.as_deref() != Some(tool.as_str())
    {
        changed = true;
        events.push(PlanAgentEvent::ToolStarted {
            id: None,
            name: tool.clone(),
            args_summary: None,
        });
    } else if status.active_tool.is_none() && step.active_tool.is_some() {
        changed = true;
        events.push(PlanAgentEvent::ToolFinished {
            id: None,
            status: "idle".to_string(),
        });
    }
    let todos = status
        .todos
        .iter()
        .map(|todo| PlanTodo::new(&todo.text, &todo.status))
        .collect::<Vec<_>>();
    if step.todos != todos {
        changed = true;
        events.push(PlanAgentEvent::TodoUpdated { todos });
    }

    ingest_plan_agent_events(conn, step, events, max_output_lines_per_step)?;
    Ok(changed)
}

pub(super) fn ingest_plan_agent_events(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
    events: Vec<PlanAgentEvent>,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    for event in events {
        ingest_single_plan_agent_event(conn, step, event, max_output_lines_per_step)?;
    }
    save_step_with_conn(conn, step)
}

pub(super) fn ingest_single_plan_agent_event(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
    event: PlanAgentEvent,
    max_output_lines_per_step: usize,
) -> Result<String, String> {
    let (kind, text, block_id) = apply_agent_event(step, event);
    append_system_output_with_block(
        conn,
        step,
        kind,
        &text,
        block_id.as_deref(),
        max_output_lines_per_step,
    )?;
    Ok(text)
}

pub(super) fn events_match_step_session(step: &PlanStepRun, events: &[PlanAgentEvent]) -> bool {
    let event_session_ids = events
        .iter()
        .filter_map(|event| match event {
            PlanAgentEvent::SessionIdentified { session_id, .. } => Some(session_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if let Some(step_session_id) = step.opencode_session_id.as_deref() {
        return event_session_ids
            .iter()
            .all(|event_session_id| *event_session_id == step_session_id);
    }
    event_session_ids.len() == 1
}

pub(super) fn apply_agent_event(
    step: &mut PlanStepRun,
    event: PlanAgentEvent,
) -> (PlanOutputKind, String, Option<String>) {
    match event {
        PlanAgentEvent::SessionIdentified { session_id, title } => {
            step.opencode_session_id = Some(session_id.clone());
            let title = title
                .map(|title| format!(" title: {title}"))
                .unwrap_or_default();
            (
                PlanOutputKind::Status,
                format!("session {session_id}{title}"),
                None,
            )
        }
        PlanAgentEvent::StateChanged { state } => {
            step.opencode_state = OpencodeState::parse(&state);
            if state == OpencodeState::Idle.label() {
                step.active_tool = None;
            }
            (PlanOutputKind::Status, format!("status: {state}"), None)
        }
        PlanAgentEvent::AssistantText { text } => {
            step.latest_message = Some(text.clone());
            (PlanOutputKind::Assistant, text, None)
        }
        PlanAgentEvent::ToolStarted {
            id,
            name,
            args_summary,
        } => {
            let mut text = format!("tool {name} running");
            if let Some(args) = args_summary {
                text.push_str(": ");
                text.push_str(&args);
            }
            step.active_tool = Some(text.clone());
            (PlanOutputKind::Tool, text, id)
        }
        PlanAgentEvent::ToolOutput { id, text } => (PlanOutputKind::ToolOutput, text, id),
        PlanAgentEvent::ToolFinished { id, status } => {
            step.active_tool = None;
            (PlanOutputKind::Tool, format!("tool finished: {status}"), id)
        }
        PlanAgentEvent::TodoUpdated { todos } => {
            let text = format!("todos updated: {}", todos.len());
            step.todos = todos;
            (PlanOutputKind::Todo, text, None)
        }
        PlanAgentEvent::DiffUpdated { summary, patch } => {
            let text = patch
                .map(|patch| format!("{summary}\n{patch}"))
                .unwrap_or(summary);
            (PlanOutputKind::Diff, text, None)
        }
        PlanAgentEvent::Error { message } => {
            step.error = Some(message.clone());
            (PlanOutputKind::Error, message, None)
        }
        PlanAgentEvent::Raw { event_type, json } => (
            PlanOutputKind::RawJson,
            format!("[{event_type}] {json}"),
            None,
        ),
    }
}

pub(super) fn todos_from_value(value: &Value) -> Option<Vec<PlanTodo>> {
    find_key_deep(value, "todos")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let title = string_field_deep(item, &["title", "text", "content"])?;
                    let status = string_field_deep(item, &["status", "state"])
                        .unwrap_or_else(|| "pending".into());
                    Some(PlanTodo::new(title, status))
                })
                .collect::<Vec<_>>()
        })
}

pub(super) fn should_keep_raw(event_type: &str, value: &Value) -> bool {
    let lower_type = event_type.to_ascii_lowercase();
    lower_type.contains("tool")
        || lower_type.contains("diff")
        || lower_type.contains("error")
        || find_key_deep(value, "input").is_some()
        || find_key_deep(value, "arguments").is_some()
}

pub(super) fn string_field_deep(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| find_key_deep(value, key).and_then(value_to_string))
}

pub(super) fn nested_string(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    value_to_string(current)
}

pub(super) fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

pub(super) fn find_key_deep<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|v| find_key_deep(v, key))),
        Value::Array(items) => items.iter().find_map(|item| find_key_deep(item, key)),
        _ => None,
    }
}
