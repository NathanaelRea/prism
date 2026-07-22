use super::*;

pub(super) fn execute_one_agent_step(
    conn: &rusqlite::Connection,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    executor: &AutoExecutorConfig,
    output: &mut dyn Write,
) -> Result<(), String> {
    {
        let step = &mut persisted.steps[step_index];
        step.status = AutoStepStatus::Starting;
        step.started_unix_ms = Some(unix_ms());
        step.finished_unix_ms = None;
        step.opencode_server_url = executor.server_url.clone();
        step.opencode_session_id = None;
        step.process_id = None;
        step.error = None;
        persisted.run.selected_step_run_id = step.id;
        persisted.run.status = AutoRunStatus::Running;
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        save_step_with_conn(conn, step)?;
    }

    let prompt = prompt_for_step(config, &persisted.run, &persisted.steps[step_index]);
    let label = persisted.steps[step_index].step_key.as_str().to_string();
    writeln!(
        output,
        "\n==> Auto Flow {label} attempt {}\n",
        persisted.steps[step_index].attempt
    )
    .map_err(|error| format!("write auto output: {error}"))?;

    let (mut command, mut invocation) =
        harness_run_command(executor, &persisted.steps[step_index], &prompt, true)?;
    let spawn_result = spawn_harness(&mut command, &invocation);
    let (mut child, used_attach) = match spawn_result {
        Ok(child) => (child, invocation.attach),
        Err(error) if executor.server_url.is_some() => {
            if let Some(step_id) = persisted.steps[step_index].id {
                append_system_output(
                    conn,
                    step_id,
                    AutoOutputKind::Error,
                    &format!("attach launch failed, retrying without --attach: {error}"),
                    None,
                    executor.max_output_lines_per_step,
                )?;
            }
            invocation.cleanup();
            let (mut fallback, fallback_invocation) =
                harness_run_command(executor, &persisted.steps[step_index], &prompt, false)?;
            match spawn_harness(&mut fallback, &fallback_invocation) {
                Ok(child) => {
                    invocation = fallback_invocation;
                    (child, false)
                }
                Err(error) => {
                    mark_spawn_failure(
                        conn,
                        &mut persisted.steps[step_index],
                        &error,
                        executor.max_output_lines_per_step,
                    )?;
                    return Err(error);
                }
            }
        }
        Err(error) => {
            invocation.cleanup();
            mark_spawn_failure(
                conn,
                &mut persisted.steps[step_index],
                &error,
                executor.max_output_lines_per_step,
            )?;
            return Err(error);
        }
    };

    {
        let step = &mut persisted.steps[step_index];
        step.status = AutoStepStatus::Running;
        step.process_id = Some(child.id());
        save_step_with_conn(conn, step)?;
    }

    let exit_code = collect_child_output(
        conn,
        &mut persisted.steps[step_index],
        &mut child,
        executor.max_output_lines_per_step,
        invocation.structured_events,
        output,
    );
    invocation.cleanup();
    let exit_code = exit_code?;
    finish_step_after_exit(
        conn,
        &mut persisted.steps[step_index],
        exit_code,
        used_attach,
    )?;
    if exit_code == 0 {
        Ok(())
    } else {
        let step = &persisted.steps[step_index];
        Err(format!(
            "auto flow step {} attempt {} failed: {}",
            step.step_key.as_str(),
            step.attempt,
            step.error.as_deref().unwrap_or("opencode run failed")
        ))
    }
}

fn harness_run_command(
    executor: &AutoExecutorConfig,
    step: &AutoStepRun,
    prompt: &str,
    attach: bool,
) -> Result<(Command, crate::harness::Invocation), String> {
    let invocation = crate::harness::Harness::new(&executor.harness_id, &executor.harness_config)
        .headless(
        prompt,
        &executor.worktree_path,
        &format!(
            "{} {} attempt {}",
            executor.title_prefix,
            step.step_key.as_str(),
            step.attempt
        ),
        executor.server_url.as_deref(),
        None,
        attach,
    )?;
    let command = invocation.command(&executor.worktree_path)?;
    Ok((command, invocation))
}

fn spawn_harness(
    command: &mut Command,
    invocation: &crate::harness::Invocation,
) -> Result<Child, String> {
    let mut child = command
        .spawn()
        .map_err(|error| format!("harness '{}': {error}", invocation.argv[0]))?;
    if let Some(input) = invocation.stdin.as_deref() {
        use std::io::Write as _;
        let result = child
            .stdin
            .take()
            .ok_or_else(|| "open harness stdin".to_string())
            .and_then(|mut stdin| {
                stdin
                    .write_all(input.as_bytes())
                    .map_err(|error| format!("write harness prompt to stdin: {error}"))
            });
        if let Err(error) = result {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    }
    Ok(child)
}

pub(super) fn mark_spawn_failure(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    error: &str,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    step.status = AutoStepStatus::Failed;
    step.finished_unix_ms = Some(unix_ms());
    step.error = Some(error.to_string());
    let step_id = save_step_with_conn(conn, step)?;
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Error,
        error,
        None,
        max_output_lines_per_step,
    )
}

pub(super) fn finish_step_after_exit(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    exit_code: i32,
    used_attach: bool,
) -> Result<(), String> {
    step.process_id = None;
    step.finished_unix_ms = Some(unix_ms());
    if exit_code == 0 {
        step.status = AutoStepStatus::Done;
        step.error = None;
    } else {
        step.status = AutoStepStatus::Failed;
        let attach_note = if used_attach { " with --attach" } else { "" };
        step.error = Some(format!("opencode run{attach_note} exited with {exit_code}"));
    }
    save_step_with_conn(conn, step)?;
    Ok(())
}

pub(super) fn collect_child_output(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    child: &mut Child,
    max_output_lines_per_step: usize,
    structured_events: bool,
    output: &mut dyn Write,
) -> Result<i32, String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "open opencode stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "open opencode stderr".to_string())?;
    let (tx, rx) = mpsc::channel::<Result<ChildLine, String>>();
    spawn_reader_thread(StreamKind::Stdout, stdout, tx.clone());
    spawn_reader_thread(StreamKind::Stderr, stderr, tx);

    let mut readers_open = 2;
    while readers_open > 0 {
        match rx.recv() {
            Ok(Ok(ChildLine::Line { stream, text })) => {
                ingest_child_line(
                    conn,
                    step,
                    stream,
                    &text,
                    max_output_lines_per_step,
                    structured_events,
                    output,
                )?;
            }
            Ok(Ok(ChildLine::End)) => readers_open -= 1,
            Ok(Err(error)) => return Err(error),
            Err(_) => break,
        }
    }

    let status = child
        .wait()
        .map_err(|error| format!("wait for opencode: {error}"))?;
    Ok(status.code().unwrap_or(1))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StreamKind {
    Stdout,
    Stderr,
}

#[derive(Debug)]
pub(super) enum ChildLine {
    Line { stream: StreamKind, text: String },
    End,
}

pub(super) fn spawn_reader_thread(
    stream: StreamKind,
    reader: impl std::io::Read + Send + 'static,
    tx: mpsc::Sender<Result<ChildLine, String>>,
) {
    thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            match line {
                Ok(text) => {
                    if tx.send(Ok(ChildLine::Line { stream, text })).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = tx.send(Err(format!("read opencode output: {error}")));
                    return;
                }
            }
        }
        let _ = tx.send(Ok(ChildLine::End));
    });
}

pub(super) fn ingest_child_line(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    stream: StreamKind,
    raw: &str,
    max_output_lines_per_step: usize,
    structured_events: bool,
    output: &mut dyn Write,
) -> Result<(), String> {
    if stream == StreamKind::Stderr {
        let step_id = step
            .id
            .ok_or_else(|| "auto step must be saved before output".to_string())?;
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Error,
            raw,
            None,
            max_output_lines_per_step,
        )?;
        step.error = Some(raw.to_string());
        save_step_with_conn(conn, step)?;
        writeln!(output, "[stderr] {raw}")
            .map_err(|error| format!("write auto output: {error}"))?;
        return Ok(());
    }

    if !structured_events {
        let step_id = step
            .id
            .ok_or_else(|| "auto step must be saved before output".to_string())?;
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Assistant,
            raw,
            None,
            max_output_lines_per_step,
        )?;
        step.summary = Some(raw.to_string());
        save_step_with_conn(conn, step)?;
        writeln!(output, "{raw}").map_err(|error| format!("write auto output: {error}"))?;
        return Ok(());
    }

    let events = crate::plan_run::parse_plan_agent_events(raw);
    for event in events {
        let text = ingest_single_agent_event(conn, step, event, max_output_lines_per_step)?;
        writeln!(output, "{text}").map_err(|error| format!("write auto output: {error}"))?;
    }
    save_step_with_conn(conn, step)?;
    Ok(())
}

pub(super) fn ingest_single_agent_event(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    event: PlanAgentEvent,
    max_output_lines_per_step: usize,
) -> Result<String, String> {
    let (kind, text, block_id) = apply_agent_event(step, event);
    let step_id = step
        .id
        .ok_or_else(|| "auto step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        kind,
        &text,
        block_id.as_deref(),
        max_output_lines_per_step,
    )?;
    Ok(text)
}

pub(super) fn apply_agent_event(
    step: &mut AutoStepRun,
    event: PlanAgentEvent,
) -> (AutoOutputKind, String, Option<String>) {
    match event {
        PlanAgentEvent::SessionIdentified { session_id, title } => {
            step.opencode_session_id = Some(session_id.clone());
            let title = title
                .map(|title| format!(" title: {title}"))
                .unwrap_or_default();
            (
                AutoOutputKind::Status,
                format!("session {session_id}{title}"),
                None,
            )
        }
        PlanAgentEvent::StateChanged { state } => {
            (AutoOutputKind::Status, format!("status: {state}"), None)
        }
        PlanAgentEvent::AssistantText { text } => {
            step.summary = Some(text.clone());
            (AutoOutputKind::Assistant, text, None)
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
            (AutoOutputKind::Tool, text, id)
        }
        PlanAgentEvent::ToolOutput { id, text } => (AutoOutputKind::ToolOutput, text, id),
        PlanAgentEvent::ToolFinished { id, status } => {
            (AutoOutputKind::Tool, format!("tool finished: {status}"), id)
        }
        PlanAgentEvent::TodoUpdated { todos } => (
            AutoOutputKind::Status,
            format!("todos updated: {}", todos.len()),
            None,
        ),
        PlanAgentEvent::DiffUpdated { summary, patch } => {
            let text = patch
                .map(|patch| format!("{summary}\n{patch}"))
                .unwrap_or(summary);
            (AutoOutputKind::Diff, text, None)
        }
        PlanAgentEvent::Error { message } => {
            step.error = Some(message.clone());
            (AutoOutputKind::Error, message, None)
        }
        PlanAgentEvent::Raw { event_type, json } => (
            AutoOutputKind::RawJson,
            format!("[{event_type}] {json}"),
            None,
        ),
    }
}
