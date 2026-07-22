use super::*;

pub fn execute_plan_sequential(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
    executor: &PlanExecutorConfig,
    output: &mut dyn Write,
) -> Result<(), String> {
    persisted.run.status = PlanRunStatus::Running;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)?;

    let mut failure: Option<String> = None;
    let mut paused = false;
    for index in 0..persisted.steps.len() {
        if failure.is_some() {
            break;
        }
        if persisted.steps[index].status != PlanStepStatus::Queued {
            continue;
        }
        if reload_pause_request(conn, persisted)? {
            paused = true;
            break;
        }
        let result = execute_one_step(conn, persisted, index, executor, output);
        if let Err(error) = result {
            failure = Some(error);
        }
    }

    if paused {
        persisted.run.status = PlanRunStatus::Paused;
    } else {
        persisted.run.status = persisted.aggregate_status();
    }
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)?;

    if let Some(error) = failure {
        Err(error)
    } else {
        Ok(())
    }
}

pub fn execute_plan_parallel(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
    executor: &PlanExecutorConfig,
    output: &mut dyn Write,
) -> Result<(), String> {
    persisted.run.status = PlanRunStatus::Running;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)?;

    let (tx, rx) = mpsc::channel::<Result<ParallelChildEvent, String>>();
    let mut running = 0usize;
    let mut spawn_errors = Vec::new();

    for index in 0..persisted.steps.len() {
        if persisted.steps[index].status != PlanStepStatus::Queued {
            continue;
        }
        let step_number = persisted.steps[index].step;
        let prompt = persisted.steps[index].prompt.clone();
        {
            let step = &mut persisted.steps[index];
            step.status = PlanStepStatus::Starting;
            step.started_unix_ms = Some(unix_ms());
            step.opencode_server_url = executor.server_url.clone();
            step.agent_variant = executor.agent_variant.clone();
            step.error = None;
            save_step_with_conn(conn, step)?;
        }
        persisted.run.selected_step = step_number;
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        writeln!(output, "\n==> {prompt}\n")
            .map_err(|error| format!("write plan output: {error}"))?;

        let (mut command, invocation) = harness_run_command(executor, step_number, &prompt, true)?;
        let spawn_result = spawn_harness(&mut command, &invocation);
        let (child, used_attach) = match spawn_result {
            Ok(child) => (child, invocation.attach),
            Err(error) if executor.server_url.is_some() => {
                append_system_output(
                    conn,
                    &persisted.steps[index],
                    PlanOutputKind::Error,
                    &format!("attach launch failed, retrying without --attach: {error}"),
                    executor.max_output_lines_per_step,
                )?;
                let (mut fallback, fallback_invocation) =
                    harness_run_command(executor, step_number, &prompt, false)?;
                match spawn_harness(&mut fallback, &fallback_invocation) {
                    Ok(child) => (child, false),
                    Err(error) => {
                        mark_spawn_failure(
                            conn,
                            &mut persisted.steps[index],
                            &error,
                            executor.max_output_lines_per_step,
                        )?;
                        spawn_errors.push(error);
                        continue;
                    }
                }
            }
            Err(error) => {
                invocation.cleanup();
                mark_spawn_failure(
                    conn,
                    &mut persisted.steps[index],
                    &error,
                    executor.max_output_lines_per_step,
                )?;
                spawn_errors.push(error);
                continue;
            }
        };

        persisted.steps[index].status = PlanStepStatus::Running;
        persisted.steps[index].process_id = Some(child.id());
        identify_attached_plan_session(executor, &mut persisted.steps[index]);
        save_step_with_conn(conn, &persisted.steps[index])?;
        spawn_parallel_child(index, child, used_attach, invocation, tx.clone())?;
        running += 1;
    }
    drop(tx);

    let mut finished = 0usize;
    while finished < running {
        match rx.recv() {
            Ok(Ok(ParallelChildEvent::Line {
                step_index,
                stream,
                text,
            })) => {
                if let Some(step) = persisted.steps.get_mut(step_index) {
                    ingest_child_line(
                        conn,
                        step,
                        stream,
                        &text,
                        executor.max_output_lines_per_step,
                        executor.harness_config.output_format
                            == crate::harness::OutputFormat::JsonLines,
                        output,
                    )?;
                }
            }
            Ok(Ok(ParallelChildEvent::Exit {
                step_index,
                exit_code,
                used_attach,
            })) => {
                if let Some(step) = persisted.steps.get_mut(step_index) {
                    finish_step_after_exit(conn, step, exit_code, used_attach)?;
                    persisted.run.selected_step = step.step;
                    persisted.run.status = persisted.aggregate_status();
                    persisted.run.updated_unix_ms = unix_ms();
                    save_run_with_conn(conn, &persisted.run)?;
                }
                finished += 1;
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => break,
        }
    }

    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)?;

    if persisted
        .steps
        .iter()
        .any(|step| step.status == PlanStepStatus::Failed)
    {
        let failures = persisted
            .steps
            .iter()
            .filter(|step| step.status == PlanStepStatus::Failed)
            .map(|step| {
                format!(
                    "step {}: {}",
                    step.step,
                    step.error.as_deref().unwrap_or("failed")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        Err(format!("parallel plan failed: {failures}"))
    } else if !spawn_errors.is_empty() {
        Err(format!("parallel plan failed: {}", spawn_errors.join("; ")))
    } else {
        Ok(())
    }
}

pub(super) fn execute_one_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
    step_index: usize,
    executor: &PlanExecutorConfig,
    output: &mut dyn Write,
) -> Result<(), String> {
    {
        let step = &mut persisted.steps[step_index];
        step.status = PlanStepStatus::Starting;
        step.started_unix_ms = Some(unix_ms());
        step.opencode_server_url = executor.server_url.clone();
        step.agent_variant = executor.agent_variant.clone();
        step.error = None;
        persisted.run.selected_step = step.step;
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        save_step_with_conn(conn, step)?;
    }

    let step_number = persisted.steps[step_index].step;
    let prompt = persisted.steps[step_index].prompt.clone();
    writeln!(output, "\n==> {prompt}\n").map_err(|error| format!("write plan output: {error}"))?;

    let (mut command, mut invocation) = harness_run_command(executor, step_number, &prompt, true)?;
    let spawn_result = spawn_harness(&mut command, &invocation);
    let (mut child, used_attach) = match spawn_result {
        Ok(child) => (child, invocation.attach),
        Err(error) if executor.server_url.is_some() => {
            append_system_output(
                conn,
                &persisted.steps[step_index],
                PlanOutputKind::Error,
                &format!("attach launch failed, retrying without --attach: {error}"),
                executor.max_output_lines_per_step,
            )?;
            invocation.cleanup();
            let (mut fallback, fallback_invocation) =
                harness_run_command(executor, step_number, &prompt, false)?;
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
        step.status = PlanStepStatus::Running;
        step.process_id = Some(child.id());
        identify_attached_plan_session(executor, step);
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

    let step = &mut persisted.steps[step_index];
    finish_step_after_exit(conn, step, exit_code, used_attach)?;
    if exit_code == 0 {
        Ok(())
    } else {
        Err(format!(
            "plan step {} failed: {}",
            step.step,
            step.error.as_deref().unwrap_or("opencode run failed")
        ))
    }
}

pub(super) fn finish_step_after_exit(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
    exit_code: i32,
    used_attach: bool,
) -> Result<(), String> {
    step.process_id = None;
    step.finished_unix_ms = Some(unix_ms());
    step.exit_code = Some(exit_code);
    if exit_code == 0 {
        step.status = PlanStepStatus::Done;
        step.active_tool = None;
    } else {
        step.status = PlanStepStatus::Failed;
        let attach_note = if used_attach { " with --attach" } else { "" };
        step.error = Some(format!("opencode run{attach_note} exited with {exit_code}"));
    }
    save_step_with_conn(conn, step)
}

pub(super) fn mark_spawn_failure(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
    error: &str,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    step.status = PlanStepStatus::Failed;
    step.finished_unix_ms = Some(unix_ms());
    step.error = Some(error.to_string());
    append_system_output(
        conn,
        step,
        PlanOutputKind::Error,
        error,
        max_output_lines_per_step,
    )?;
    save_step_with_conn(conn, step)
}

#[cfg(test)]
pub(super) fn opencode_run_command(
    executor: &PlanExecutorConfig,
    step: usize,
    prompt: &str,
    attach: bool,
) -> Command {
    harness_run_command(executor, step, prompt, attach)
        .expect("valid OpenCode harness invocation")
        .0
}

fn harness_run_command(
    executor: &PlanExecutorConfig,
    step: usize,
    prompt: &str,
    attach: bool,
) -> Result<(Command, crate::harness::Invocation), String> {
    let harness = crate::harness::Harness::new(&executor.harness_id, &executor.harness_config);
    let mut invocation = harness.headless(
        prompt,
        &executor.scope_path,
        &format!("{} phase {}", executor.title_prefix, step),
        executor.server_url.as_deref(),
        executor.agent_variant.as_deref(),
        attach,
    )?;
    if let Some(config_dir) = executor.plugin_config_dir.as_deref() {
        invocation.environment.insert(
            "OPENCODE_CONFIG_DIR".to_string(),
            config_dir.display().to_string(),
        );
    }
    if let Some(event_log_path) = executor.plugin_event_log_path.as_deref() {
        invocation.environment.insert(
            "PRISM_PLAN_HOOK_LOG".to_string(),
            event_log_path.display().to_string(),
        );
    }
    let command = invocation.command(&executor.scope_path)?;
    Ok((command, invocation))
}

pub(super) fn identify_attached_plan_session(
    executor: &PlanExecutorConfig,
    step: &mut PlanStepRun,
) {
    let Some(server_url) = executor.server_url.as_deref() else {
        return;
    };
    if step.opencode_session_id.is_some() {
        return;
    }
    let title = format!("{} phase {}", executor.title_prefix, step.step);
    if let Ok(sessions) = crate::opencode::list_sessions(server_url)
        && let Some(session) = sessions
            .iter()
            .filter(|session| session.title.as_deref() == Some(title.as_str()))
            .max_by(|left, right| left.time_updated.cmp(&right.time_updated))
    {
        step.opencode_server_url = Some(server_url.to_string());
        step.opencode_session_id = Some(session.id.clone());
    }
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

pub(super) fn collect_child_output(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
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

#[derive(Debug)]
pub(super) enum ParallelChildEvent {
    Line {
        step_index: usize,
        stream: StreamKind,
        text: String,
    },
    Exit {
        step_index: usize,
        exit_code: i32,
        used_attach: bool,
    },
}

pub(super) fn spawn_parallel_child(
    step_index: usize,
    mut child: Child,
    used_attach: bool,
    invocation: crate::harness::Invocation,
    tx: mpsc::Sender<Result<ParallelChildEvent, String>>,
) -> Result<(), String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "open opencode stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "open opencode stderr".to_string())?;
    spawn_parallel_reader(step_index, StreamKind::Stdout, stdout, tx.clone());
    spawn_parallel_reader(step_index, StreamKind::Stderr, stderr, tx.clone());
    thread::spawn(move || {
        let result = child
            .wait()
            .map_err(|error| format!("wait for opencode: {error}"))
            .map(|status| ParallelChildEvent::Exit {
                step_index,
                exit_code: status.code().unwrap_or(1),
                used_attach,
            });
        invocation.cleanup();
        let _ = tx.send(result);
    });
    Ok(())
}

pub(super) fn spawn_parallel_reader(
    step_index: usize,
    stream: StreamKind,
    reader: impl std::io::Read + Send + 'static,
    tx: mpsc::Sender<Result<ParallelChildEvent, String>>,
) {
    thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            match line {
                Ok(text) => {
                    let event = ParallelChildEvent::Line {
                        step_index,
                        stream,
                        text,
                    };
                    if tx.send(Ok(event)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = tx.send(Err(format!("read opencode output: {error}")));
                    return;
                }
            }
        }
    });
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
    step: &mut PlanStepRun,
    stream: StreamKind,
    raw: &str,
    max_output_lines_per_step: usize,
    structured_events: bool,
    output: &mut dyn Write,
) -> Result<(), String> {
    if stream == StreamKind::Stderr {
        append_system_output(
            conn,
            step,
            PlanOutputKind::Error,
            raw,
            max_output_lines_per_step,
        )?;
        step.error = Some(raw.to_string());
        save_step_with_conn(conn, step)?;
        writeln!(output, "[stderr] {raw}")
            .map_err(|error| format!("write plan output: {error}"))?;
        return Ok(());
    }

    if !structured_events {
        append_system_output(
            conn,
            step,
            PlanOutputKind::Assistant,
            raw,
            max_output_lines_per_step,
        )?;
        step.latest_message = Some(raw.to_string());
        save_step_with_conn(conn, step)?;
        writeln!(output, "{raw}").map_err(|error| format!("write plan output: {error}"))?;
        return Ok(());
    }

    let events = parse_plan_agent_events(raw);
    for event in events {
        let text = ingest_single_plan_agent_event(conn, step, event, max_output_lines_per_step)?;
        writeln!(output, "{text}").map_err(|error| format!("write plan output: {error}"))?;
    }
    save_step_with_conn(conn, step)?;
    Ok(())
}
