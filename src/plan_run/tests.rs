use super::*;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[test]
fn launch_creates_queued_steps_with_prompts() {
    let repo = PathBuf::from("/repo/prism");
    let plan = repo.join("plan-plan.md");
    let launch = PlanLaunch::new(&repo, &repo, &plan, "phase", 2, 4, PlanRunMode::Sequential)
        .expect("launch");

    let persisted = launch.create_run();

    assert_eq!(persisted.run.plan_display, "plan-plan.md");
    assert_eq!(persisted.run.selected_step, 2);
    assert_eq!(persisted.run.status, PlanRunStatus::Queued);
    assert_eq!(
        persisted
            .steps
            .iter()
            .map(|step| step.prompt.as_str())
            .collect::<Vec<_>>(),
        vec![
            "Implement plan-plan.md phase 2",
            "Implement plan-plan.md phase 3",
            "Implement plan-plan.md phase 4",
        ]
    );
}

#[test]
fn opencode_run_command_passes_prompt_as_single_raw_argument() {
    let scope_path = PathBuf::from("/repo/prism");
    let executor = PlanExecutorConfig::new(
        "opencode".to_string(),
        Some("http://127.0.0.1:41234".to_string()),
        scope_path.clone(),
        "plan with spaces.md",
    );
    let prompt = "  Implement plan phase 3\n\"quotes\" and $PATH && true\n--leading-dash";

    let command = opencode_run_command(&executor, 3, prompt, true);
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect::<Vec<_>>();

    assert_eq!(args.last().map(String::as_str), Some(prompt));
    assert!(args.windows(2).any(|args| args == ["--variant", "medium"]));
    assert!(!args.iter().any(|arg| arg == &format!("'{prompt}'")));
    assert_eq!(args.iter().filter(|arg| arg.as_str() == prompt).count(), 1);
    assert_eq!(command.get_current_dir(), Some(scope_path.as_path()));
}

#[test]
fn aggregate_status_prioritizes_failure_and_running_state() {
    assert_eq!(
        aggregate_step_status([
            PlanStepStatus::Done,
            PlanStepStatus::Queued,
            PlanStepStatus::Done
        ]),
        PlanRunStatus::Queued
    );
    assert_eq!(
        aggregate_step_status([PlanStepStatus::Done, PlanStepStatus::Running]),
        PlanRunStatus::Running
    );
    assert_eq!(
        aggregate_step_status([PlanStepStatus::Running, PlanStepStatus::Failed]),
        PlanRunStatus::Failed
    );
    assert_eq!(
        aggregate_step_status([PlanStepStatus::Done, PlanStepStatus::Skipped]),
        PlanRunStatus::Done
    );
}

#[test]
fn schema_round_trips_plan_run_steps_and_output() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();

    let repo = PathBuf::from("/repo/prism");
    let plan = repo.join("plans/plan-one.md");
    let mut persisted = PlanLaunch::new(&repo, &repo, &plan, "phase", 1, 2, PlanRunMode::Parallel)
        .unwrap()
        .create_run();
    persisted.run.status = PlanRunStatus::Running;
    persisted.steps[0].status = PlanStepStatus::Done;
    persisted.steps[0].latest_message = Some("finished phase 1".to_string());
    persisted.steps[0].todos = vec![PlanTodo::new("write tests", "done")];
    persisted.steps[1].status = PlanStepStatus::Running;
    persisted.steps[1].active_tool = Some("bash running: cargo test".to_string());

    save_plan_run(&conn, &persisted).unwrap();

    let loaded = load_plan_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("run");
    assert_eq!(loaded.run, persisted.run);
    assert_eq!(loaded.steps, persisted.steps);
    assert_eq!(loaded.status_counts().done, 1);
    assert_eq!(loaded.status_counts().running, 1);
    assert_eq!(loaded.aggregate_status(), PlanRunStatus::Running);

    for line_number in 1..=3 {
        append_output_line(
            &conn,
            &PlanOutputLine {
                run_id: persisted.run.id.clone(),
                step: 1,
                line_number,
                time_unix_ms: 100 + line_number,
                kind: PlanOutputKind::Assistant,
                text: format!("line {line_number}"),
                block_id: None,
            },
            2,
        )
        .unwrap();
    }

    let output = load_output_lines(&conn, &persisted.run.id, 1).unwrap();
    assert_eq!(
        output
            .iter()
            .map(|line| (line.line_number, line.text.as_str()))
            .collect::<Vec<_>>(),
        vec![(2, "[... omitted 2 older output lines ...]"), (3, "line 3")]
    );
}

#[test]
fn plan_plugin_config_is_generated_under_prism_state() {
    let temp = unique_temp_dir("prism-plan-plugin-config");
    let repo_root = temp.join("repo");
    let prism_dir = temp.join("config/prism/repos/repo-1234");
    std::fs::create_dir_all(&repo_root).unwrap();

    let plugin = prepare_plan_plugin_config(&prism_dir).unwrap();

    assert!(plugin.config_dir.starts_with(&prism_dir));
    assert!(!plugin.config_dir.starts_with(&repo_root));
    assert!(plugin.plugin_path.is_file());
    assert!(plugin.config_dir.join("opencode.json").is_file());
    assert_eq!(
        plugin.event_log_path,
        plugin.config_dir.join("events.jsonl")
    );
    let config = std::fs::read_to_string(plugin.config_dir.join("opencode.json")).unwrap();
    assert!(config.contains("prism-plan-plugin.js"));
    let plugin_source = std::fs::read_to_string(plugin.plugin_path).unwrap();
    assert!(plugin_source.contains("tool.execute.before"));
    assert!(plugin_source.contains("session.compacted"));

    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn executor_passes_plugin_environment_only_when_enabled() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let temp = unique_temp_dir("prism-plan-plugin-env");
    let observed_config_dir = PathBuf::from("observed-config-dir");
    let observed_hook_log = PathBuf::from("observed-hook-log");
    let opencode = fake_opencode(
        &temp,
        r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s' "${OPENCODE_CONFIG_DIR:-}" > observed-config-dir
printf '%s' "${PRISM_PLAN_HOOK_LOG:-}" > observed-hook-log
echo '{"type":"message","text":"plugin env observed"}'
"#,
    );
    let plugin = prepare_plan_plugin_config(&temp.join("prism-state")).unwrap();
    let mut persisted = PlanLaunch::new(
        &temp,
        &temp,
        &temp.join("plan.md"),
        "phase",
        1,
        1,
        PlanRunMode::Sequential,
    )
    .unwrap()
    .create_run();
    save_plan_run(&conn, &persisted).unwrap();
    let executor = PlanExecutorConfig::new(
        opencode.display().to_string(),
        None,
        temp.clone(),
        "plan.md",
    )
    .with_plugin_config(plugin.clone());
    let mut output = Vec::new();

    execute_plan_sequential(&conn, &mut persisted, &executor, &mut output).unwrap();

    assert_eq!(
        std::fs::read_to_string(temp.join(observed_config_dir)).unwrap(),
        plugin.config_dir.display().to_string()
    );
    assert_eq!(
        std::fs::read_to_string(temp.join(observed_hook_log)).unwrap(),
        plugin.event_log_path.display().to_string()
    );

    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn sequential_executor_updates_steps_from_fake_opencode() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let temp = unique_temp_dir("prism-plan-executor-success");
    let opencode = fake_opencode(
        &temp,
        r#"#!/usr/bin/env bash
set -euo pipefail
echo '{"type":"session","session_id":"ses_test","title":"phase"}'
echo '{"type":"message","text":"working"}'
echo '{"type":"tool.call","id":"tool_1","name":"bash","input":{"command":"cargo test"}}'
echo '{"type":"todo.updated","todos":[{"title":"write tests","status":"done"}]}'
"#,
    );
    let mut persisted = PlanLaunch::new(
        &temp,
        &temp,
        &temp.join("plan.md"),
        "phase",
        1,
        2,
        PlanRunMode::Sequential,
    )
    .unwrap()
    .create_run();
    save_plan_run(&conn, &persisted).unwrap();

    let executor = PlanExecutorConfig::new(
        opencode.display().to_string(),
        Some("http://127.0.0.1:41234".to_string()),
        temp.clone(),
        "plan.md",
    );
    let mut output = Vec::new();

    execute_plan_sequential(&conn, &mut persisted, &executor, &mut output).unwrap();

    let loaded = load_plan_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("persisted run");
    assert_eq!(loaded.run.status, PlanRunStatus::Done);
    assert!(
        loaded
            .steps
            .iter()
            .all(|step| step.status == PlanStepStatus::Done)
    );
    assert_eq!(
        loaded.steps[0].opencode_server_url.as_deref(),
        Some("http://127.0.0.1:41234")
    );
    assert_eq!(
        loaded.steps[0].opencode_session_id.as_deref(),
        Some("ses_test")
    );
    assert_eq!(loaded.steps[0].latest_message.as_deref(), Some("working"));
    assert_eq!(
        loaded.steps[0].todos,
        vec![PlanTodo::new("write tests", "done")]
    );
    assert!(
        load_output_lines(&conn, &persisted.run.id, 1)
            .unwrap()
            .iter()
            .any(|line| line.kind == PlanOutputKind::Tool)
    );

    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn sequential_executor_stops_on_failed_step() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let temp = unique_temp_dir("prism-plan-executor-failure");
    let opencode = fake_opencode(
        &temp,
        r#"#!/usr/bin/env bash
set -euo pipefail
echo '{"type":"message","text":"started"}'
if [[ "$*" == *"phase 2"* ]]; then
  echo 'phase 2 failed' >&2
  exit 7
fi
"#,
    );
    let mut persisted = PlanLaunch::new(
        &temp,
        &temp,
        &temp.join("plan.md"),
        "phase",
        1,
        3,
        PlanRunMode::Sequential,
    )
    .unwrap()
    .create_run();
    save_plan_run(&conn, &persisted).unwrap();
    let executor = PlanExecutorConfig::new(
        opencode.display().to_string(),
        None,
        temp.clone(),
        "plan.md",
    );
    let mut output = Vec::new();

    let error = execute_plan_sequential(&conn, &mut persisted, &executor, &mut output)
        .expect_err("phase 2 should fail");

    assert!(error.contains("plan step 2 failed"));
    let loaded = load_plan_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("persisted run");
    assert_eq!(loaded.run.status, PlanRunStatus::Failed);
    assert_eq!(loaded.steps[0].status, PlanStepStatus::Done);
    assert_eq!(loaded.steps[1].status, PlanStepStatus::Failed);
    assert_eq!(loaded.steps[1].exit_code, Some(7));
    assert_eq!(loaded.steps[2].status, PlanStepStatus::Queued);
    assert!(
        load_output_lines(&conn, &persisted.run.id, 2)
            .unwrap()
            .iter()
            .any(|line| line.kind == PlanOutputKind::Error && line.text == "phase 2 failed")
    );

    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn parallel_executor_runs_all_steps_and_waits_for_failures() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let temp = unique_temp_dir("prism-plan-executor-parallel-failure");
    let marker = temp.join("phase-3-finished");
    let opencode = fake_opencode(
        &temp,
        r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "$*" == *"phase 1"* ]]; then
  echo '{"type":"message","text":"phase 1 done"}'
  exit 0
fi
if [[ "$*" == *"phase 2"* ]]; then
  echo '{"type":"message","text":"phase 2 failed"}'
  sleep 0.1
  exit 9
fi
if [[ "$*" == *"phase 3"* ]]; then
  sleep 0.3
  echo '{"type":"message","text":"phase 3 done"}'
  touch phase-3-finished
  exit 0
fi
"#,
    );
    let mut persisted = PlanLaunch::new(
        &temp,
        &temp,
        &temp.join("plan.md"),
        "phase",
        1,
        3,
        PlanRunMode::Parallel,
    )
    .unwrap()
    .create_run();
    save_plan_run(&conn, &persisted).unwrap();
    let executor = PlanExecutorConfig::new(
        opencode.display().to_string(),
        None,
        temp.clone(),
        "plan.md",
    );
    let mut output = Vec::new();

    let error = execute_plan_parallel(&conn, &mut persisted, &executor, &mut output)
        .expect_err("phase 2 should fail");

    assert!(error.contains("parallel plan failed"));
    assert!(
        marker.exists(),
        "phase 3 should continue after phase 2 fails"
    );
    let loaded = load_plan_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("persisted run");
    assert_eq!(loaded.run.status, PlanRunStatus::Failed);
    assert_eq!(loaded.steps[0].status, PlanStepStatus::Done);
    assert_eq!(loaded.steps[1].status, PlanStepStatus::Failed);
    assert_eq!(loaded.steps[1].exit_code, Some(9));
    assert_eq!(loaded.steps[2].status, PlanStepStatus::Done);
    assert_eq!(
        loaded.steps[2].latest_message.as_deref(),
        Some("phase 3 done")
    );

    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn parallel_executor_marks_success_after_all_steps_finish() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let temp = unique_temp_dir("prism-plan-executor-parallel-success");
    let opencode = fake_opencode(
        &temp,
        r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "$*" == *"phase 1"* ]]; then
  sleep 0.2
  echo '{"type":"message","text":"phase 1 done"}'
else
  echo '{"type":"message","text":"phase 2 done"}'
fi
"#,
    );
    let mut persisted = PlanLaunch::new(
        &temp,
        &temp,
        &temp.join("plan.md"),
        "phase",
        1,
        2,
        PlanRunMode::Parallel,
    )
    .unwrap()
    .create_run();
    save_plan_run(&conn, &persisted).unwrap();
    let executor = PlanExecutorConfig::new(
        opencode.display().to_string(),
        None,
        temp.clone(),
        "plan.md",
    );
    let mut output = Vec::new();

    execute_plan_parallel(&conn, &mut persisted, &executor, &mut output).unwrap();

    let loaded = load_plan_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("persisted run");
    assert_eq!(loaded.run.status, PlanRunStatus::Done);
    assert!(
        loaded
            .steps
            .iter()
            .all(|step| step.status == PlanStepStatus::Done)
    );

    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn parses_known_plan_agent_events_without_panics() {
    let events = parse_plan_agent_events(
        r#"{"type":"tool.execute.before","session":{"id":"ses_1"},"name":"bash","input":{"command":"cargo test"}}"#,
    );

    assert!(events.iter().any(|event| matches!(
        event,
        PlanAgentEvent::SessionIdentified { session_id, .. } if session_id == "ses_1"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        PlanAgentEvent::ToolStarted { name, args_summary, .. }
            if name == "bash" && args_summary.as_deref() == Some("cargo test")
    )));
}

#[test]
fn sse_payload_ingestion_updates_matching_step_and_ignores_other_sessions() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = PlanLaunch::new(
        &repo,
        &repo,
        &repo.join("plan.md"),
        "phase",
        1,
        1,
        PlanRunMode::Sequential,
    )
    .unwrap()
    .create_run();
    persisted.steps[0].opencode_session_id = Some("ses_plan".to_string());
    save_plan_run(&conn, &persisted).unwrap();

    let matched = ingest_plan_sse_payload(
        &conn,
        &mut persisted.steps[0],
        r#"{"type":"message.part.updated","properties":{"sessionID":"ses_plan","role":"assistant","text":"live update"}}"#,
        DEFAULT_OUTPUT_LINES_PER_STEP,
    )
    .unwrap();
    let ignored = ingest_plan_sse_payload(
        &conn,
        &mut persisted.steps[0],
        r#"{"type":"message.part.updated","properties":{"sessionID":"ses_other","role":"assistant","text":"wrong run"}}"#,
        DEFAULT_OUTPUT_LINES_PER_STEP,
    )
    .unwrap();

    assert!(matched);
    assert!(!ignored);
    assert_eq!(
        persisted.steps[0].latest_message.as_deref(),
        Some("live update")
    );
    let output = load_output_lines(&conn, &persisted.run.id, 1).unwrap();
    assert!(output.iter().any(|line| line.text == "live update"));
    assert!(!output.iter().any(|line| line.text.contains("wrong run")));
}

#[test]
fn sse_payload_ingestion_tracks_session_and_raw_relevant_unknown_events() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = PlanLaunch::new(
        &repo,
        &repo,
        &repo.join("plan.md"),
        "phase",
        1,
        1,
        PlanRunMode::Sequential,
    )
    .unwrap()
    .create_run();
    save_plan_run(&conn, &persisted).unwrap();

    let matched = ingest_plan_sse_payload(
        &conn,
        &mut persisted.steps[0],
        r#"{"type":"session.status","properties":{"sessionID":"ses_new","status":"busy","title":"plan phase 1"}}"#,
        DEFAULT_OUTPUT_LINES_PER_STEP,
    )
    .unwrap();
    ingest_plan_sse_payload(
        &conn,
        &mut persisted.steps[0],
        r#"{"type":"tool.execute.after","properties":{"sessionID":"ses_new","id":"tool_1","name":"bash","status":"success","arguments":{"command":"cargo test"}}}"#,
        DEFAULT_OUTPUT_LINES_PER_STEP,
    )
    .unwrap();
    let malformed = ingest_plan_sse_payload(
        &conn,
        &mut persisted.steps[0],
        "not json",
        DEFAULT_OUTPUT_LINES_PER_STEP,
    )
    .unwrap();

    assert!(matched);
    assert!(!malformed);
    assert_eq!(
        persisted.steps[0].opencode_session_id.as_deref(),
        Some("ses_new")
    );
    assert_eq!(persisted.steps[0].active_tool, None);
    let output = load_output_lines(&conn, &persisted.run.id, 1).unwrap();
    assert!(output.iter().any(
        |line| line.kind == PlanOutputKind::RawJson && line.text.contains("tool.execute.after")
    ));
}

#[test]
fn poll_reconciliation_recovers_latest_status_message_tool_and_todos() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = PlanLaunch::new(
        &repo,
        &repo,
        &repo.join("plan.md"),
        "phase",
        1,
        1,
        PlanRunMode::Sequential,
    )
    .unwrap()
    .create_run();
    persisted.steps[0].opencode_server_url = Some("http://127.0.0.1:41234".to_string());
    persisted.steps[0].opencode_session_id = Some("ses_plan".to_string());
    save_plan_run(&conn, &persisted).unwrap();

    let status = OpencodeStatus {
        server_url: Some("http://127.0.0.1:41234".to_string()),
        session_id: Some("ses_plan".to_string()),
        title: Some("plan phase 1".to_string()),
        state: OpencodeState::Busy,
        latest_message: Some("recovered message".to_string()),
        latest_user_message: None,
        recent_messages: vec!["recovered message".to_string()],
        active_tool: Some("bash running".to_string()),
        todos: vec![crate::opencode::OpencodeTodo {
            text: "finish phase".to_string(),
            status: "in_progress".to_string(),
        }],
        last_updated_unix_ms: Some(42),
    };

    reconcile_plan_step_from_opencode_status(
        &conn,
        &mut persisted.steps[0],
        &status,
        DEFAULT_OUTPUT_LINES_PER_STEP,
    )
    .unwrap();

    assert_eq!(
        persisted.steps[0].latest_message.as_deref(),
        Some("recovered message")
    );
    assert_eq!(
        persisted.steps[0].todos,
        vec![PlanTodo::new("finish phase", "in_progress")]
    );
    assert!(
        persisted.steps[0]
            .active_tool
            .as_deref()
            .is_some_and(|tool| tool.contains("bash running"))
    );
    let output = load_output_lines(&conn, &persisted.run.id, 1).unwrap();
    assert!(
        output
            .iter()
            .any(|line| line.kind == PlanOutputKind::Assistant && line.text == "recovered message")
    );
}

#[test]
fn abort_plan_step_marks_step_aborted() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = PlanLaunch::new(
        &repo,
        &repo,
        &repo.join("plan.md"),
        "phase",
        1,
        1,
        PlanRunMode::Sequential,
    )
    .unwrap()
    .create_run();
    save_plan_run(&conn, &persisted).unwrap();

    abort_plan_step(&conn, &mut persisted.steps[0]).unwrap();

    let loaded = load_plan_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("persisted run");
    assert_eq!(loaded.steps[0].status, PlanStepStatus::Aborted);
    assert_eq!(loaded.steps[0].error.as_deref(), Some("aborted"));
    assert!(loaded.steps[0].finished_unix_ms.is_some());
}

#[test]
fn abort_plan_run_aborts_queued_steps_and_clears_pause() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = PlanLaunch::new(
        &repo,
        &repo,
        &repo.join("plan.md"),
        "phase",
        1,
        2,
        PlanRunMode::Sequential,
    )
    .unwrap()
    .create_run();
    persisted.run.pause_requested = true;
    save_plan_run(&conn, &persisted).unwrap();

    abort_plan_run(&conn, &mut persisted).unwrap();

    assert_eq!(persisted.run.status, PlanRunStatus::Aborted);
    assert!(!persisted.run.pause_requested);
    assert!(
        persisted
            .steps
            .iter()
            .all(|step| step.status == PlanStepStatus::Aborted)
    );
    let loaded = load_plan_run(&conn, &persisted.run.id).unwrap().unwrap();
    assert_eq!(loaded.run.status, PlanRunStatus::Aborted);
    assert!(
        loaded
            .steps
            .iter()
            .all(|step| step.status == PlanStepStatus::Aborted)
    );
}

#[test]
fn reconcile_marks_running_steps_failed_after_restart() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = PlanLaunch::new(
        &repo,
        &repo,
        &repo.join("plan.md"),
        "phase",
        1,
        2,
        PlanRunMode::Sequential,
    )
    .unwrap()
    .create_run();
    persisted.run.status = PlanRunStatus::Running;
    persisted.steps[0].status = PlanStepStatus::Running;
    persisted.steps[0].process_id = None;
    save_plan_run(&conn, &persisted).unwrap();

    let changed =
        reconcile_stale_plan_run(&conn, &mut persisted, DEFAULT_OUTPUT_LINES_PER_STEP).unwrap();

    assert!(changed);
    let loaded = load_plan_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("persisted run");
    assert_eq!(loaded.run.status, PlanRunStatus::Failed);
    assert_eq!(loaded.steps[0].status, PlanStepStatus::Failed);
    assert!(
        loaded.steps[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Prism restarted"))
    );
    assert!(
        load_output_lines(&conn, &persisted.run.id, 1)
            .unwrap()
            .iter()
            .any(|line| line.kind == PlanOutputKind::Error
                && line.text.contains("Prism restarted"))
    );
}

#[test]
fn reconcile_keeps_running_step_with_live_process() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let temp = unique_temp_dir("prism-plan-live-reconcile");
    let mut persisted = PlanLaunch::new(
        &temp,
        &temp,
        &temp.join("plan.md"),
        "phase",
        1,
        1,
        PlanRunMode::Sequential,
    )
    .unwrap()
    .create_run();
    persisted.run.status = PlanRunStatus::Running;
    persisted.run.selected_step = 1;
    persisted.run.updated_unix_ms = 123;
    persisted.steps[0].status = PlanStepStatus::Running;
    persisted.steps[0].process_id = Some(std::process::id());
    persisted.steps[0].opencode_server_url = Some("http://127.0.0.1:41234".to_string());
    persisted.steps[0].opencode_session_id = Some("ses_live".to_string());
    persisted.steps[0].started_unix_ms = Some(111);
    save_plan_run(&conn, &persisted).unwrap();

    let changed =
        reconcile_stale_plan_run(&conn, &mut persisted, DEFAULT_OUTPUT_LINES_PER_STEP).unwrap();
    let changed_again =
        reconcile_stale_plan_run(&conn, &mut persisted, DEFAULT_OUTPUT_LINES_PER_STEP).unwrap();

    assert!(changed);
    assert!(!changed_again);
    let loaded = load_plan_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("persisted run");
    assert_eq!(loaded.run.status, PlanRunStatus::Running);
    assert_eq!(loaded.run.selected_step, 1);
    assert_eq!(loaded.run.updated_unix_ms, 123);
    assert_eq!(loaded.steps[0].status, PlanStepStatus::Running);
    assert_eq!(loaded.steps[0].process_id, Some(std::process::id()));
    assert_eq!(loaded.steps[0].started_unix_ms, Some(111));
    assert_eq!(loaded.steps[0].finished_unix_ms, None);
    assert_eq!(
        loaded.steps[0].opencode_server_url.as_deref(),
        Some("http://127.0.0.1:41234")
    );
    assert_eq!(
        loaded.steps[0].opencode_session_id.as_deref(),
        Some("ses_live")
    );
    let output = load_output_lines(&conn, &persisted.run.id, 1).unwrap();
    assert_eq!(
        output
            .iter()
            .filter(|line| line.kind == PlanOutputKind::System
                && line.text.contains("stdout cannot be reattached"))
            .count(),
        1
    );

    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn retry_from_step_resets_selected_and_later_steps() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = PlanLaunch::new(
        &repo,
        &repo,
        &repo.join("plan.md"),
        "phase",
        1,
        3,
        PlanRunMode::Sequential,
    )
    .unwrap()
    .create_run();
    persisted.steps[0].status = PlanStepStatus::Done;
    persisted.steps[1].status = PlanStepStatus::Failed;
    persisted.steps[1].error = Some("failed".to_string());
    persisted.steps[2].status = PlanStepStatus::Skipped;
    save_plan_run(&conn, &persisted).unwrap();

    retry_from_step(&conn, &mut persisted, 2).unwrap();

    let loaded = load_plan_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("persisted run");
    assert_eq!(loaded.steps[0].status, PlanStepStatus::Done);
    assert_eq!(loaded.steps[1].status, PlanStepStatus::Queued);
    assert_eq!(loaded.steps[1].error, None);
    assert_eq!(loaded.steps[2].status, PlanStepStatus::Queued);
    assert_eq!(loaded.run.selected_step, 2);
    assert_eq!(loaded.run.status, PlanRunStatus::Queued);
}

#[test]
fn pause_request_stops_sequential_executor_before_next_queued_step() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let temp = unique_temp_dir("prism-plan-pause-before-next");
    let marker = temp.join("should-not-run");
    let opencode = fake_opencode(
        &temp,
        r#"#!/usr/bin/env bash
set -euo pipefail
touch should-not-run
"#,
    );
    let mut persisted = PlanLaunch::new(
        &temp,
        &temp,
        &temp.join("plan.md"),
        "phase",
        1,
        2,
        PlanRunMode::Sequential,
    )
    .unwrap()
    .create_run();
    persisted.steps[0].status = PlanStepStatus::Done;
    save_plan_run(&conn, &persisted).unwrap();
    request_plan_run_pause(&conn, &mut persisted).unwrap();
    let executor = PlanExecutorConfig::new(
        opencode.display().to_string(),
        None,
        temp.clone(),
        "plan.md",
    );
    let mut output = Vec::new();

    execute_plan_sequential(&conn, &mut persisted, &executor, &mut output).unwrap();

    let loaded = load_plan_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("persisted run");
    assert_eq!(loaded.run.status, PlanRunStatus::Paused);
    assert!(loaded.run.pause_requested);
    assert_eq!(loaded.steps[0].status, PlanStepStatus::Done);
    assert_eq!(loaded.steps[1].status, PlanStepStatus::Queued);
    assert!(!marker.exists());

    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn resumable_run_requeues_interrupted_steps_and_preserves_done_steps() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let launch = PlanLaunch::new(
        &repo,
        &repo,
        &repo.join("plan.md"),
        "phase",
        1,
        3,
        PlanRunMode::Sequential,
    )
    .unwrap();
    let mut persisted = launch.create_run();
    persisted.run.status = PlanRunStatus::Running;
    persisted.steps[0].status = PlanStepStatus::Done;
    persisted.steps[1].status = PlanStepStatus::Running;
    persisted.steps[1].process_id = None;
    save_plan_run(&conn, &persisted).unwrap();

    let mut resumed = load_resumable_plan_run(&conn, &launch)
        .unwrap()
        .expect("resumable run");
    let can_execute =
        prepare_plan_run_for_resume(&conn, &mut resumed, DEFAULT_OUTPUT_LINES_PER_STEP).unwrap();

    assert!(can_execute);
    let loaded = load_plan_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("persisted run");
    assert!(!loaded.run.pause_requested);
    assert_eq!(loaded.run.status, PlanRunStatus::Queued);
    assert_eq!(loaded.steps[0].status, PlanStepStatus::Done);
    assert_eq!(loaded.steps[1].status, PlanStepStatus::Queued);
    assert_eq!(loaded.steps[2].status, PlanStepStatus::Queued);
}

#[test]
fn skip_and_archive_plan_run_hide_it_from_recent_runs() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = PlanLaunch::new(
        &repo,
        &repo,
        &repo.join("plan.md"),
        "phase",
        1,
        1,
        PlanRunMode::Sequential,
    )
    .unwrap()
    .create_run();
    save_plan_run(&conn, &persisted).unwrap();

    skip_plan_step(&conn, &mut persisted, 1).unwrap();
    assert_eq!(persisted.run.status, PlanRunStatus::Done);
    archive_plan_run(&conn, &mut persisted).unwrap();

    let recent = load_recent_plan_runs_for_repo(&conn, &repo, 8).unwrap();
    assert!(recent.is_empty());
    let loaded = load_plan_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("archived run remains loadable");
    assert!(loaded.run.archived_unix_ms.is_some());
    let removed = cleanup_stale_archived_plan_runs(&conn, 0).unwrap();
    assert_eq!(removed, 1);
    assert!(load_plan_run(&conn, &persisted.run.id).unwrap().is_none());
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "{prefix}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn fake_opencode(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("opencode-shim");
    std::fs::write(&path, body).unwrap();
    make_executable(&path);
    path
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}
