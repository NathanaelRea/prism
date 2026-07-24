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
        step.session.endpoint = executor.server_url.clone();
        step.session.adapter_id = Some(executor.harness_config.adapter.clone());
        step.session.id = None;
        step.execution = crate::harness::ExecutionRef::default();
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
        match harness_run_command(executor, &persisted.steps[step_index], &prompt, true) {
            Ok(command) => command,
            Err(error) => {
                mark_spawn_failure(
                    conn,
                    &mut persisted.steps[step_index],
                    &error,
                    executor.max_output_lines_per_step,
                )?;
                return Err(error);
            }
        };
    crate::execution::validate_installed_claim(conn)?;
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
            crate::execution::validate_installed_claim(conn)?;
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
        if !claim_spawned_process(conn, step, &mut child)? {
            invocation.cleanup();
            return Err(format!(
                "auto flow step {} was aborted while starting",
                step.step_key.as_str()
            ));
        }
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
        &executor.harness_id,
    )?;
    if persisted.steps[step_index].status == AutoStepStatus::Aborted {
        Err(format!(
            "auto flow step {} attempt {} was aborted",
            persisted.steps[step_index].step_key.as_str(),
            persisted.steps[step_index].attempt
        ))
    } else if exit_code == 0 && persisted.steps[step_index].status == AutoStepStatus::Done {
        Ok(())
    } else {
        let step = &persisted.steps[step_index];
        Err(format!(
            "auto flow step {} attempt {} failed: {}",
            step.step_key.as_str(),
            step.attempt,
            step.error.as_deref().unwrap_or("harness run failed")
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
    let cwd = command
        .get_current_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    invocation.spawn(&cwd)
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
    harness_id: &str,
) -> Result<(), String> {
    step.execution.process_id = None;
    step.execution.process_start_time_ticks = None;
    step.finished_unix_ms = Some(unix_ms());
    if exit_code == 0 && step.error.is_none() {
        step.status = AutoStepStatus::Done;
        step.error = None;
    } else {
        step.status = AutoStepStatus::Failed;
        if step.error.is_none() {
            let attach_note = if used_attach { " while attached" } else { "" };
            step.error = Some(format!(
                "harness '{harness_id}'{attach_note} exited with {exit_code}"
            ));
        }
    }
    let step_id = step
        .id
        .ok_or_else(|| "auto step must be saved before completion".to_string())?;
    let changed = conn
        .execute(
            "update auto_step_run
             set status = ?1, execution_process_id = null,
                 execution_process_start_time_ticks = null, finished_unix_ms = ?2, error = ?3
             where id = ?4 and status != 'aborted'",
            params![
                step.status.as_str(),
                step.finished_unix_ms.map(u64_to_i64),
                step.error,
                step_id,
            ],
        )
        .map_err(|error| format!("finish auto step: {error}"))?;
    if changed == 0 {
        let status = conn
            .query_row(
                "select status from auto_step_run where id = ?1",
                params![step_id],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| format!("reload auto step status: {error}"))?;
        step.status = AutoStepStatus::parse(&status)?;
        step.execution.process_id = None;
        step.execution.process_start_time_ticks = None;
        if step.status == AutoStepStatus::Aborted {
            step.error = Some("aborted".to_string());
        }
    }
    Ok(())
}

pub(super) fn claim_spawned_process(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    child: &mut Child,
) -> Result<bool, String> {
    let step_id = step
        .id
        .ok_or_else(|| "auto step must be saved before process spawn".to_string())?;
    let process_id = child.id();
    let start_time_ticks = crate::harness::process_start_time_ticks(process_id);
    let changed = conn
        .execute(
            "update auto_step_run
             set status = 'running', execution_process_id = ?1,
                 execution_process_start_time_ticks = ?2
             where id = ?3 and status = 'starting'",
            params![
                i64::from(process_id),
                start_time_ticks.map(u64_to_i64),
                step_id,
            ],
        )
        .map_err(|error| format!("claim auto harness process: {error}"))?;
    if changed == 0 {
        let _ = crate::harness::terminate_process(process_id, start_time_ticks);
        let _ = child.wait();
        step.status = AutoStepStatus::Aborted;
        step.execution = crate::harness::ExecutionRef::default();
        return Ok(false);
    }
    step.status = AutoStepStatus::Running;
    step.execution.process_id = Some(process_id);
    step.execution.process_start_time_ticks = start_time_ticks;
    Ok(true)
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
        .ok_or_else(|| "open harness stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "open harness stderr".to_string())?;
    let (tx, rx) = mpsc::channel::<Result<ChildLine, String>>();
    spawn_reader_thread(StreamKind::Stdout, stdout, tx.clone());
    spawn_reader_thread(StreamKind::Stderr, stderr, tx);

    let mut readers_open = 2;
    while readers_open > 0 {
        match rx.recv_timeout(std::time::Duration::from_millis(250)) {
            Ok(Ok(ChildLine::Line { stream, text })) => {
                if let Err(error) = ingest_child_line(
                    conn,
                    step,
                    stream,
                    &text,
                    max_output_lines_per_step,
                    structured_events,
                    output,
                ) {
                    terminate_auto_child(step, child);
                    return Err(error);
                }
            }
            Ok(Ok(ChildLine::End)) => readers_open -= 1,
            Ok(Err(error)) => {
                terminate_auto_child(step, child);
                return Err(error);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Err(error) = crate::execution::validate_installed_claim(conn) {
                    terminate_auto_child(step, child);
                    return Err(error);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let status = child
        .wait()
        .map_err(|error| format!("wait for harness: {error}"))?;
    Ok(status.code().unwrap_or(1))
}

fn terminate_auto_child(step: &AutoStepRun, child: &mut Child) {
    let process_id = step.execution.process_id.unwrap_or_else(|| child.id());
    let identity = step.execution.process_start_time_ticks;
    let _ = crate::harness::terminate_process(process_id, identity);
    let _ = child.wait();
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
        let result = crate::harness::read_bounded_lines(reader, |text| {
            tx.send(Ok(ChildLine::Line { stream, text })).is_ok()
        });
        if let Err(error) = result {
            let _ = tx.send(Err(error));
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
            step.session.id = Some(session_id.clone());
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
