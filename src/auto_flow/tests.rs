use super::*;
use std::collections::BTreeMap;
use std::fs;
use std::process::Command;

use crate::config::{Checks, Config, EscapeKey, MergeMethod};

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let unique = unix_ms();
        let path = std::env::temp_dir().join(format!(
            "prism-auto-flow-test-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn launch_creates_persistable_auto_run() {
    let repo = PathBuf::from("/repo/prism");
    let launch = AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
        .expect("launch");

    let persisted = launch.create_run();

    assert_eq!(persisted.run.status, AutoRunStatus::Queued);
    assert_eq!(persisted.run.branch, "feat/auto");
    assert_eq!(persisted.run.prompt_summary, "Implement auto");
    assert_eq!(persisted.steps.len(), 1);
    assert_eq!(persisted.steps[0].sequence, 1);
    assert_eq!(persisted.steps[0].step_key, AutoStepKey::Prepare);
}

#[test]
fn plan_first_prompts_create_and_review_plan_file() {
    let repo = PathBuf::from("/repo/prism");
    let persisted = AutoLaunch::with_options(
        &repo,
        &repo.join("feature"),
        AutoLaunchOptions {
            branch: "feat/auto".to_string(),
            mode: AutoRunMode::PlanFirst,
            implementation_source: AutoImplementationSource::DraftPlan,
            plan_path: Some(repo.join("feature/plan.md")),
            plan_run_mode: PlanRunMode::Sequential,
            variant: "intensive".to_string(),
            agent_profile: Some("planner".to_string()),
            initial_prompt: "Implement auto".to_string(),
        },
    )
    .unwrap()
    .create_run();

    let create_prompt = prompt_for_step(
        &persisted.run,
        &AutoStepRun::queued(&persisted.run.id, 2, AutoStepKey::CreatePlan, 1, None),
    );
    let review_prompt = prompt_for_step(
        &persisted.run,
        &AutoStepRun::queued(&persisted.run.id, 3, AutoStepKey::ReviewPlan, 1, None),
    );
    assert!(create_prompt.contains("/repo/prism/feature/plan.md"));
    assert!(create_prompt.contains("Do not implement"));
    assert!(create_prompt.contains("Variant: intensive"));
    assert!(create_prompt.contains("Agent profile: planner"));
    assert!(review_prompt.contains("missing phases"));
    assert!(review_prompt.contains("Edit the plan in place"));
}

#[test]
fn plan_first_queues_prelude_before_run_plan() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = AutoLaunch::with_options(
        &repo,
        &repo.join("feature"),
        AutoLaunchOptions {
            branch: "feat/auto".to_string(),
            mode: AutoRunMode::PlanFirst,
            implementation_source: AutoImplementationSource::DraftPlan,
            plan_path: Some(repo.join("feature/plan.md")),
            plan_run_mode: PlanRunMode::Sequential,
            variant: "intensive".to_string(),
            agent_profile: None,
            initial_prompt: "Implement auto".to_string(),
        },
    )
    .unwrap()
    .create_run();
    persisted.steps[0].status = AutoStepStatus::Done;
    save_auto_run(&conn, &mut persisted).unwrap();

    assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());
    assert_eq!(persisted.steps[1].step_key, AutoStepKey::CreatePlan);
    persisted.steps[1].status = AutoStepStatus::Done;
    save_auto_run(&conn, &mut persisted).unwrap();

    assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());
    assert_eq!(persisted.steps[2].step_key, AutoStepKey::ReviewPlan);
    persisted.steps[2].status = AutoStepStatus::Done;
    save_auto_run(&conn, &mut persisted).unwrap();

    assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());
    assert_eq!(persisted.steps[3].step_key, AutoStepKey::ApprovePlan);
    assert!(
        !persisted
            .steps
            .iter()
            .any(|step| step.step_key == AutoStepKey::Implement)
    );
    persisted.steps[3].status = AutoStepStatus::Done;
    save_auto_run(&conn, &mut persisted).unwrap();

    assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());
    assert_eq!(persisted.steps[4].step_key, AutoStepKey::RunPlan);
}

#[test]
fn plan_approval_pauses_and_resume_queues_run_plan() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = AutoLaunch::with_options(
        &repo,
        &repo.join("feature"),
        AutoLaunchOptions {
            branch: "feat/auto".to_string(),
            mode: AutoRunMode::PlanFirst,
            implementation_source: AutoImplementationSource::DraftPlan,
            plan_path: Some(repo.join("feature/plan.md")),
            plan_run_mode: PlanRunMode::Sequential,
            variant: "intensive".to_string(),
            agent_profile: None,
            initial_prompt: "Implement auto".to_string(),
        },
    )
    .unwrap()
    .create_run();
    persisted.steps.clear();
    push_test_step(
        &mut persisted,
        1,
        AutoStepKey::CreatePlan,
        AutoStepStatus::Done,
    );
    push_test_step(
        &mut persisted,
        2,
        AutoStepKey::ReviewPlan,
        AutoStepStatus::Done,
    );
    persisted.steps.push(AutoStepRun::queued(
        &persisted.run.id,
        3,
        AutoStepKey::ApprovePlan,
        1,
        Some("approve".to_string()),
    ));
    save_auto_run(&conn, &mut persisted).unwrap();
    start_non_agent_step(&conn, &mut persisted, 2).unwrap();

    execute_approve_plan_step(&conn, &mut persisted, 2, 100).unwrap();

    assert_eq!(persisted.run.status, AutoRunStatus::Paused);
    assert!(persisted.run.pause_requested);
    assert_eq!(persisted.steps[2].status, AutoStepStatus::Done);

    resume_paused_auto_run(&conn, &mut persisted).unwrap();
    assert!(prepare_auto_run_for_resume(&conn, &mut persisted, 100).unwrap());
    assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());
    assert!(
        persisted
            .steps
            .iter()
            .any(|step| step.step_key == AutoStepKey::RunPlan)
    );
}

#[test]
fn existing_plan_queues_run_plan() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = AutoLaunch::with_options(
        &repo,
        &repo.join("feature"),
        AutoLaunchOptions {
            branch: "feat/auto".to_string(),
            mode: AutoRunMode::Standard,
            implementation_source: AutoImplementationSource::ExistingPlan,
            plan_path: Some(repo.join("feature/plan.md")),
            plan_run_mode: PlanRunMode::Sequential,
            variant: "default".to_string(),
            agent_profile: None,
            initial_prompt: "Implement existing plan".to_string(),
        },
    )
    .unwrap()
    .create_run();
    persisted.steps[0].status = AutoStepStatus::Done;
    save_auto_run(&conn, &mut persisted).unwrap();

    assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());

    assert_eq!(persisted.steps[1].step_key, AutoStepKey::RunPlan);
}

#[test]
#[cfg(unix)]
fn prompt_implementation_pr_delegates_to_stabilization_ready_state() {
    let temp = TempDir::new("stabilization-ready-delegation");
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    setup_git_worktree(&origin, &work);
    let head = git_output(&work, &["rev-parse", "HEAD"]);
    let repo = Repository::with_config_dir_for_test(work.clone(), temp.path().join("prism-config"));
    let config = Config::load(&repo);
    seed_pr_cache(&repo, "feat/auto", &head);
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let mut persisted = AutoLaunch::new(&work, &work, "feat/auto", "Implement auto")
        .unwrap()
        .create_run();
    persisted.steps.clear();
    push_test_step(
        &mut persisted,
        1,
        AutoStepKey::Prepare,
        AutoStepStatus::Done,
    );
    push_test_step(
        &mut persisted,
        2,
        AutoStepKey::Implement,
        AutoStepStatus::Done,
    );
    push_test_step(
        &mut persisted,
        3,
        AutoStepKey::LocalVerify,
        AutoStepStatus::Done,
    );
    push_test_step(
        &mut persisted,
        4,
        AutoStepKey::CommitImpl,
        AutoStepStatus::Done,
    );
    push_test_step(&mut persisted, 5, AutoStepKey::PushPr, AutoStepStatus::Done);
    persisted.run.pr_number = Some(42);
    persisted.run.pr_url = Some("https://example.com/pr/42".to_string());
    persisted.run.current_head_sha = Some(head.clone());
    save_auto_run(&conn, &mut persisted).unwrap();

    assert!(ensure_next_auto_step_with_context(&conn, &repo, &config, &mut persisted).unwrap());

    let step = persisted.steps.last().unwrap();
    assert_eq!(step.step_key, AutoStepKey::Merge);
    assert_eq!(
        persisted.run.stabilization_status,
        Some(stabilization_model::StabilizationStatus::Ready)
    );
    assert_eq!(
        persisted.run.stabilization_blocker,
        Some(stabilization_model::StabilizationBlocker::ReadyForManualMerge)
    );
    assert_eq!(
        persisted.run.stabilization_next_work,
        Some(stabilization_model::StabilizationWorkKind::MarkReadyForManualMerge)
    );
    assert_eq!(
        step.blocker,
        Some(stabilization_model::StabilizationBlocker::ReadyForManualMerge)
    );
    assert_eq!(
        step.work_guard.as_ref().unwrap().pr_head_sha.as_deref(),
        Some(head.as_str())
    );
}

#[test]
#[cfg(unix)]
fn run_plan_success_queues_local_verify() {
    let temp = TempDir::new("run-plan-success");
    let work = temp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join("plan.md"), "# Phase 1\n\nImplement it.\n").unwrap();
    let repo = Repository::with_config_dir_for_test(work.clone(), temp.path().join("prism-config"));
    let mut config = Config::load(&repo);
    let opencode = temp.path().join("opencode");
    let opencode_log = temp.path().join("opencode.log");
    write_executable(
        &opencode,
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
printf '%s\n' '{{"type":"message","text":"phase done"}}'
"#,
            opencode_log.display()
        ),
    );
    config
        .tools
        .insert("opencode".to_string(), opencode.display().to_string());
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    let mut persisted = AutoLaunch::with_options(
        &work,
        &work,
        AutoLaunchOptions {
            branch: "feat/auto".to_string(),
            mode: AutoRunMode::Standard,
            implementation_source: AutoImplementationSource::ExistingPlan,
            plan_path: Some(work.join("plan.md")),
            plan_run_mode: PlanRunMode::Sequential,
            variant: "default".to_string(),
            agent_profile: None,
            initial_prompt: "Implement existing plan".to_string(),
        },
    )
    .unwrap()
    .create_run();
    persisted.steps.clear();
    persisted.steps.push(AutoStepRun::queued(
        &persisted.run.id,
        1,
        AutoStepKey::RunPlan,
        1,
        Some("run plan".to_string()),
    ));
    save_auto_run(&conn, &mut persisted).unwrap();
    start_non_agent_step(&conn, &mut persisted, 0).unwrap();

    execute_run_plan_step(
        &conn,
        &repo,
        &config,
        &mut persisted,
        0,
        Some("http://127.0.0.1:41234".to_string()),
        100,
    )
    .unwrap();
    assert_eq!(persisted.steps[0].status, AutoStepStatus::Done);
    assert!(persisted.steps[0].plan_run_id.is_some());
    let command = fs::read_to_string(opencode_log).unwrap();
    assert!(command.contains("--attach http://127.0.0.1:41234"));

    assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());
    assert!(
        persisted
            .steps
            .iter()
            .any(|step| step.step_key == AutoStepKey::LocalVerify)
    );
}

#[test]
#[cfg(unix)]
fn run_plan_failure_marks_auto_step_failed() {
    let temp = TempDir::new("run-plan-failure");
    let work = temp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join("plan.md"), "# Phase 1\n\nImplement it.\n").unwrap();
    let repo = Repository::with_config_dir_for_test(work.clone(), temp.path().join("prism-config"));
    let mut config = Config::load(&repo);
    let opencode = temp.path().join("opencode");
    write_executable(
        &opencode,
        r#"#!/bin/sh
printf '%s\n' '{"type":"message","text":"phase failed"}'
exit 7
"#,
    );
    config
        .tools
        .insert("opencode".to_string(), opencode.display().to_string());
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    let mut persisted = AutoLaunch::with_options(
        &work,
        &work,
        AutoLaunchOptions {
            branch: "feat/auto".to_string(),
            mode: AutoRunMode::Standard,
            implementation_source: AutoImplementationSource::ExistingPlan,
            plan_path: Some(work.join("plan.md")),
            plan_run_mode: PlanRunMode::Sequential,
            variant: "default".to_string(),
            agent_profile: None,
            initial_prompt: "Implement existing plan".to_string(),
        },
    )
    .unwrap()
    .create_run();
    persisted.steps.clear();
    persisted.steps.push(AutoStepRun::queued(
        &persisted.run.id,
        1,
        AutoStepKey::RunPlan,
        1,
        Some("run plan".to_string()),
    ));
    save_auto_run(&conn, &mut persisted).unwrap();
    start_non_agent_step(&conn, &mut persisted, 0).unwrap();

    let error = execute_run_plan_step(&conn, &repo, &config, &mut persisted, 0, None, 100)
        .expect_err("run-plan should fail when linked phase fails");

    assert!(error.contains("inspect linked plan dashboard"));
    assert_eq!(persisted.steps[0].status, AutoStepStatus::Failed);
    assert_eq!(
        persisted.steps[0].summary.as_deref(),
        Some("plan run failed")
    );
    assert!(
        persisted.steps[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("ended with status failed"))
    );
    let plan_run_id = persisted.steps[0].plan_run_id.as_deref().unwrap();
    let linked_plan = load_plan_run(&conn, plan_run_id).unwrap().unwrap();
    assert_eq!(linked_plan.run.status, PlanRunStatus::Failed);
}

#[test]
fn resume_reconciles_interrupted_linked_plan_before_auto_stale_failure() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = linked_run_plan_auto_run(&conn, &repo);
    let plan_run_id = persisted.steps[0].plan_run_id.clone().unwrap();
    let mut plan_run = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
    plan_run.run.status = PlanRunStatus::Running;
    plan_run.steps[0].status = crate::plan_run::PlanStepStatus::Running;
    crate::plan_run::save_plan_run(&conn, &plan_run).unwrap();

    assert!(prepare_auto_run_for_resume(&conn, &mut persisted, 100).unwrap());

    assert_eq!(persisted.steps[0].status, AutoStepStatus::Queued);
    assert_eq!(persisted.run.status, AutoRunStatus::Queued);
    let loaded_plan = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
    assert_eq!(
        loaded_plan.steps[0].status,
        crate::plan_run::PlanStepStatus::Queued
    );
}

#[test]
fn resume_marks_run_plan_done_when_linked_plan_finished() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = linked_run_plan_auto_run(&conn, &repo);
    let plan_run_id = persisted.steps[0].plan_run_id.clone().unwrap();
    let mut plan_run = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
    plan_run.run.status = PlanRunStatus::Done;
    plan_run.steps[0].status = crate::plan_run::PlanStepStatus::Done;
    crate::plan_run::save_plan_run(&conn, &plan_run).unwrap();

    assert!(prepare_auto_run_for_resume(&conn, &mut persisted, 100).unwrap());

    assert_eq!(persisted.steps[0].status, AutoStepStatus::Done);
    assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());
    assert_eq!(persisted.steps[1].step_key, AutoStepKey::LocalVerify);
}

#[test]
fn retry_failed_run_plan_requeues_linked_failed_phase() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = linked_run_plan_auto_run(&conn, &repo);
    let plan_run_id = persisted.steps[0].plan_run_id.clone().unwrap();
    persisted.steps[0].status = AutoStepStatus::Failed;
    save_auto_run(&conn, &mut persisted).unwrap();
    let mut plan_run = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
    plan_run.run.status = PlanRunStatus::Failed;
    plan_run.steps[0].status = crate::plan_run::PlanStepStatus::Failed;
    crate::plan_run::save_plan_run(&conn, &plan_run).unwrap();

    retry_failed_auto_step(&conn, &mut persisted).unwrap();

    assert_eq!(persisted.steps.len(), 1);
    assert_eq!(persisted.steps[0].status, AutoStepStatus::Queued);
    let loaded_plan = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
    assert_eq!(
        loaded_plan.steps[0].status,
        crate::plan_run::PlanStepStatus::Queued
    );
}

#[test]
fn retry_failed_run_plan_continues_when_linked_plan_finished() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = linked_run_plan_auto_run(&conn, &repo);
    let plan_run_id = persisted.steps[0].plan_run_id.clone().unwrap();
    persisted.steps[0].status = AutoStepStatus::Failed;
    save_auto_run(&conn, &mut persisted).unwrap();
    let mut plan_run = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
    plan_run.run.status = PlanRunStatus::Done;
    plan_run.steps[0].status = crate::plan_run::PlanStepStatus::Done;
    crate::plan_run::save_plan_run(&conn, &plan_run).unwrap();

    retry_failed_auto_step(&conn, &mut persisted).unwrap();

    assert_eq!(persisted.steps[0].status, AutoStepStatus::Done);
    assert_eq!(persisted.run.status, AutoRunStatus::Done);
    assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());
    assert_eq!(persisted.steps[1].step_key, AutoStepKey::LocalVerify);
}

#[test]
fn retry_from_run_plan_resets_later_auto_steps() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = linked_run_plan_auto_run(&conn, &repo);
    persisted.steps[0].status = AutoStepStatus::Done;
    save_auto_run(&conn, &mut persisted).unwrap();
    append_step_run(
        &conn,
        &mut persisted,
        AutoStepKey::LocalVerify,
        Some("verify".to_string()),
    )
    .unwrap();
    persisted.steps[1].status = AutoStepStatus::Done;
    save_auto_run(&conn, &mut persisted).unwrap();
    let selected = persisted.steps[0].id.unwrap();

    retry_auto_from_step(&conn, &mut persisted, selected).unwrap();

    assert_eq!(persisted.steps[0].status, AutoStepStatus::Queued);
    assert_eq!(persisted.steps[1].status, AutoStepStatus::Queued);
}

#[test]
fn pause_auto_run_requests_linked_plan_pause() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = linked_run_plan_auto_run(&conn, &repo);
    let plan_run_id = persisted.steps[0].plan_run_id.clone().unwrap();

    request_auto_run_pause(&conn, &mut persisted).unwrap();

    let loaded_plan = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
    assert!(loaded_plan.run.pause_requested);
}

#[test]
fn aggregate_status_handles_waiting_and_failures() {
    assert_eq!(
        aggregate_step_status([AutoStepStatus::Done, AutoStepStatus::Waiting]),
        AutoRunStatus::Running
    );
    assert_eq!(
        aggregate_step_status([AutoStepStatus::Done, AutoStepStatus::Queued]),
        AutoRunStatus::Queued
    );
    assert_eq!(
        aggregate_step_status([AutoStepStatus::Running, AutoStepStatus::Failed]),
        AutoRunStatus::Failed
    );
    assert_eq!(
        aggregate_step_status([AutoStepStatus::Done, AutoStepStatus::Skipped]),
        AutoRunStatus::Done
    );
}

#[test]
fn schema_round_trips_run_steps_and_output() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted =
        AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
            .unwrap()
            .create_run();
    persisted.run.status = AutoRunStatus::Running;
    persisted.run.pr_number = Some(42);
    persisted.steps[0].status = AutoStepStatus::Done;
    persisted.steps[0].summary = Some("prepared".to_string());
    persisted.steps.push(AutoStepRun::running(
        &persisted.run.id,
        2,
        AutoStepKey::Implement,
        1,
    ));
    persisted.steps[1].plan_run_id = Some("plan-linked".to_string());
    persisted.run.selected_step_run_id = Some(2);

    save_auto_run(&conn, &mut persisted).unwrap();
    let implement_id = persisted.steps[1].id.expect("step id");
    append_output_line(
        &conn,
        &AutoOutputLine {
            step_run_id: implement_id,
            line_number: 1,
            time_unix_ms: 100,
            kind: AutoOutputKind::Assistant,
            text: "working".to_string(),
            block_id: None,
        },
    )
    .unwrap();

    let loaded = load_auto_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("run");

    assert_eq!(loaded.run, persisted.run);
    assert_eq!(loaded.steps, persisted.steps);
    assert_eq!(loaded.status_counts().done, 1);
    assert_eq!(loaded.status_counts().running, 1);
    assert_eq!(
        load_output_lines(&conn, implement_id).unwrap()[0].text,
        "working"
    );
}

#[test]
fn schema_round_trips_stabilization_guards_and_planner_state() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted =
        AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
            .unwrap()
            .create_run();
    persisted.run.stabilization_status = Some(stabilization_model::StabilizationStatus::Blocked);
    persisted.run.stabilization_blocker =
        Some(stabilization_model::StabilizationBlocker::PendingPush);
    persisted.run.stabilization_next_work =
        Some(stabilization_model::StabilizationWorkKind::PushPendingRepair);
    persisted.run.pending_push = Some(stabilization_model::PendingPushGuard {
        repair_kind: stabilization_model::RepairKind::Review,
        commit_sha: "repair-sha".to_string(),
        expected_local_head_sha: "repair-sha".to_string(),
        expected_remote_head_sha: Some("remote-sha".to_string()),
        pr_number: Some(42),
        expected_pr_head_sha: Some("remote-sha".to_string()),
        expected_base_sha: Some("base-sha".to_string()),
        guarded_review_thread_ids: vec!["thread-1".to_string(), "thread-2".to_string()],
    });
    persisted.steps[0].work_guard = Some(stabilization_model::WorkGuard {
        local_head_sha: Some("local-sha".to_string()),
        remote_head_sha: Some("remote-sha".to_string()),
        pr_head_sha: Some("pr-sha".to_string()),
        base_sha: Some("base-sha".to_string()),
        review_thread_ids: vec!["thread-1".to_string()],
    });
    persisted.steps[0].blocker =
        Some(stabilization_model::StabilizationBlocker::ReviewFeedbackFound);

    save_auto_run(&conn, &mut persisted).unwrap();

    let loaded = load_auto_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("run");
    assert_eq!(
        loaded.run.stabilization_status,
        persisted.run.stabilization_status
    );
    assert_eq!(
        loaded.run.stabilization_blocker,
        persisted.run.stabilization_blocker
    );
    assert_eq!(
        loaded.run.stabilization_next_work,
        persisted.run.stabilization_next_work
    );
    assert_eq!(loaded.run.pending_push, persisted.run.pending_push);
    assert_eq!(loaded.steps[0].work_guard, persisted.steps[0].work_guard);
    assert_eq!(loaded.steps[0].blocker, persisted.steps[0].blocker);
}

#[cfg(unix)]
#[test]
fn review_repair_commit_enters_pending_push_with_guard_data() {
    let temp = TempDir::new("review-repair-pending-push");
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    let config_dir = temp.path().join("config");
    setup_git_worktree(&origin, &work);
    let repo = Repository::with_config_dir_for_test(work.clone(), config_dir);
    let remote_head = git_output(&work, &["rev-parse", "origin/main"]);
    seed_pr_cache(&repo, "feat/auto", &remote_head);

    fs::write(work.join("tracked.txt"), "review fix\n").unwrap();
    let mut config = test_config();
    config.prompt_templates.insert(
        "repair_commit_review".to_string(),
        "fix: review template".to_string(),
    );
    let mut persisted = AutoLaunch::new(&repo.root, &work, "feat/auto", "Implement auto")
        .unwrap()
        .create_run();
    persisted.run.pr_number = Some(42);
    persisted.steps.clear();
    persisted.steps.push(AutoStepRun::queued(
        &persisted.run.id,
        1,
        AutoStepKey::CommitReviewFix,
        1,
        Some("commit review repair".to_string()),
    ));
    persisted.steps[0].work_guard = Some(stabilization_model::WorkGuard {
        review_thread_ids: vec!["thread-1".to_string()],
        ..stabilization_model::WorkGuard::default()
    });
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    save_auto_run(&conn, &mut persisted).unwrap();

    execute_commit_review_fix_step(&conn, &repo, &config, &mut persisted, 0, 100).unwrap();

    let guard = persisted.run.pending_push.as_ref().expect("pending push");
    let commit = git_output(&work, &["rev-parse", "HEAD"]);
    assert_eq!(guard.repair_kind, stabilization_model::RepairKind::Review);
    assert_eq!(guard.commit_sha, commit);
    assert_eq!(guard.expected_local_head_sha, commit);
    assert_eq!(guard.expected_remote_head_sha, None);
    assert_eq!(
        guard.expected_pr_head_sha.as_deref(),
        Some(remote_head.as_str())
    );
    assert_eq!(guard.guarded_review_thread_ids, vec!["thread-1"]);
    assert_eq!(
        git_output(&work, &["log", "-1", "--pretty=%s"]),
        "fix: review template"
    );
}

#[cfg(unix)]
#[test]
fn ci_repair_commit_enters_pending_push_with_guard_data() {
    let temp = TempDir::new("ci-repair-pending-push");
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    let config_dir = temp.path().join("config");
    setup_git_worktree(&origin, &work);
    run_git(&work, &["push", "-u", "origin", "feat/auto"]);
    let repo = Repository::with_config_dir_for_test(work.clone(), config_dir);
    let remote_head = git_output(&work, &["rev-parse", "origin/feat/auto"]);
    seed_pr_cache(&repo, "feat/auto", &remote_head);

    fs::write(work.join("ci.txt"), "ci fix\n").unwrap();
    let mut config = test_config();
    config.prompt_templates.insert(
        "repair_commit_ci".to_string(),
        "fix: ci template".to_string(),
    );
    let mut persisted = AutoLaunch::new(&repo.root, &work, "feat/auto", "Implement auto")
        .unwrap()
        .create_run();
    persisted.run.pr_number = Some(42);
    persisted.steps.clear();
    persisted.steps.push(AutoStepRun::queued(
        &persisted.run.id,
        1,
        AutoStepKey::CommitCiFix,
        1,
        Some("commit CI repair".to_string()),
    ));
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    save_auto_run(&conn, &mut persisted).unwrap();

    execute_commit_ci_fix_step(&conn, &repo, &config, &mut persisted, 0, 100).unwrap();

    let guard = persisted.run.pending_push.as_ref().expect("pending push");
    let commit = git_output(&work, &["rev-parse", "HEAD"]);
    assert_eq!(guard.repair_kind, stabilization_model::RepairKind::Ci);
    assert_eq!(guard.commit_sha, commit);
    assert_eq!(guard.expected_local_head_sha, commit);
    assert_eq!(
        guard.expected_remote_head_sha.as_deref(),
        Some(remote_head.as_str())
    );
    assert_eq!(
        guard.expected_pr_head_sha.as_deref(),
        Some(remote_head.as_str())
    );
    assert!(guard.guarded_review_thread_ids.is_empty());
    assert_eq!(
        git_output(&work, &["log", "-1", "--pretty=%s"]),
        "fix: ci template"
    );
    assert_eq!(
        git_output(&work, &["rev-parse", "origin/feat/auto"]),
        remote_head
    );
}

#[test]
fn schema_migration_archives_old_active_auto_runs_once() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "
        create table auto_run (
          id text primary key,
          repo_root text not null,
          worktree_path text not null,
          branch text not null,
          mode text not null,
          implementation_source text not null default 'prompt',
          plan_path text,
          plan_run_mode text not null default 'sequential',
          variant text not null,
          agent_profile text,
          prompt_summary text not null,
          initial_prompt text not null,
          status text not null,
          pause_requested integer not null default 0,
          selected_step_run_id integer,
          pr_number integer,
          pr_url text,
          current_head_sha text,
          review_baseline_json text,
          created_unix_ms integer not null,
          updated_unix_ms integer not null,
          archived_unix_ms integer
        );
        create table auto_step_run (
          id integer primary key autoincrement,
          run_id text not null references auto_run(id) on delete cascade,
          sequence integer not null,
          step_key text not null,
          reason text,
          status text not null,
          attempt integer not null,
          started_unix_ms integer,
          finished_unix_ms integer,
          opencode_server_url text,
          opencode_session_id text,
          process_id integer,
          plan_run_id text,
          commit_sha text,
          head_sha text,
          summary text,
          error text,
          unique(run_id, sequence)
        );
        insert into auto_run (
          id, repo_root, worktree_path, branch, mode, implementation_source, plan_run_mode,
          variant, prompt_summary, initial_prompt, status, created_unix_ms, updated_unix_ms
        ) values ('old', '/repo', '/repo/feature', 'feature', 'standard', 'prompt', 'sequential',
          'default', 'old', 'old', 'running', 1, 1);
        insert into auto_step_run (run_id, sequence, step_key, status, attempt)
        values ('old', 1, 'wait_ci', 'running', 1);
        ",
    )
    .unwrap();

    migrate_schema(&conn).unwrap();
    let loaded = load_auto_run(&conn, "old").unwrap().expect("run");

    assert_eq!(loaded.run.status, AutoRunStatus::Aborted);
    assert!(loaded.run.archived_unix_ms.is_some());
    assert_eq!(loaded.steps[0].status, AutoStepStatus::Aborted);
    assert!(
        loaded.steps[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("PR Stabilization"))
    );

    migrate_schema(&conn).unwrap();
    let loaded = load_auto_run(&conn, "old").unwrap().expect("run");
    assert_eq!(loaded.run.status, AutoRunStatus::Aborted);
}

#[test]
fn schema_round_trips_prompt_existing_plan_and_draft_plan_sources() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");

    let mut prompt = AutoLaunch::with_options(
        &repo,
        &repo.join("prompt"),
        AutoLaunchOptions {
            branch: "feat/prompt".to_string(),
            mode: AutoRunMode::Standard,
            implementation_source: AutoImplementationSource::Prompt,
            plan_path: None,
            plan_run_mode: PlanRunMode::Sequential,
            variant: "default".to_string(),
            agent_profile: None,
            initial_prompt: "Implement prompt task".to_string(),
        },
    )
    .unwrap()
    .create_run();
    let mut existing_plan = AutoLaunch::with_options(
        &repo,
        &repo.join("existing"),
        AutoLaunchOptions {
            branch: "feat/existing".to_string(),
            mode: AutoRunMode::Standard,
            implementation_source: AutoImplementationSource::ExistingPlan,
            plan_path: Some(repo.join("existing/plan.md")),
            plan_run_mode: PlanRunMode::Parallel,
            variant: "default".to_string(),
            agent_profile: None,
            initial_prompt: "Implement existing plan".to_string(),
        },
    )
    .unwrap()
    .create_run();
    let mut draft_plan = AutoLaunch::with_options(
        &repo,
        &repo.join("draft"),
        AutoLaunchOptions {
            branch: "feat/draft".to_string(),
            mode: AutoRunMode::PlanFirst,
            implementation_source: AutoImplementationSource::DraftPlan,
            plan_path: Some(repo.join("draft/plan.md")),
            plan_run_mode: PlanRunMode::Sequential,
            variant: "intensive".to_string(),
            agent_profile: None,
            initial_prompt: "Draft then implement plan".to_string(),
        },
    )
    .unwrap()
    .create_run();

    save_auto_run(&conn, &mut prompt).unwrap();
    save_auto_run(&conn, &mut existing_plan).unwrap();
    save_auto_run(&conn, &mut draft_plan).unwrap();

    let prompt = load_auto_run(&conn, &prompt.run.id).unwrap().unwrap();
    let existing_plan = load_auto_run(&conn, &existing_plan.run.id)
        .unwrap()
        .unwrap();
    let draft_plan = load_auto_run(&conn, &draft_plan.run.id).unwrap().unwrap();

    assert_eq!(
        prompt.run.implementation_source,
        AutoImplementationSource::Prompt
    );
    assert_eq!(prompt.run.plan_path, None);
    assert_eq!(
        existing_plan.run.implementation_source,
        AutoImplementationSource::ExistingPlan
    );
    assert_eq!(
        existing_plan.run.plan_path,
        Some(repo.join("existing/plan.md"))
    );
    assert_eq!(existing_plan.run.plan_run_mode, PlanRunMode::Parallel);
    assert_eq!(
        draft_plan.run.implementation_source,
        AutoImplementationSource::DraftPlan
    );
    assert_eq!(draft_plan.run.plan_path, Some(repo.join("draft/plan.md")));
}

#[test]
fn repeated_attempts_retain_distinct_output() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted =
        AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
            .unwrap()
            .create_run();
    save_auto_run(&conn, &mut persisted).unwrap();

    let first_id = append_step_run(
        &conn,
        &mut persisted,
        AutoStepKey::FixReview,
        Some("first review fix".to_string()),
    )
    .unwrap();
    persisted.steps[1].status = AutoStepStatus::Failed;
    save_auto_run(&conn, &mut persisted).unwrap();
    append_output_line(
        &conn,
        &AutoOutputLine {
            step_run_id: first_id,
            line_number: 1,
            time_unix_ms: 101,
            kind: AutoOutputKind::Error,
            text: "first failed".to_string(),
            block_id: None,
        },
    )
    .unwrap();

    let second_id = append_step_run(
        &conn,
        &mut persisted,
        AutoStepKey::FixReview,
        Some("second review fix".to_string()),
    )
    .unwrap();
    append_output_line(
        &conn,
        &AutoOutputLine {
            step_run_id: second_id,
            line_number: 1,
            time_unix_ms: 102,
            kind: AutoOutputKind::Assistant,
            text: "second running".to_string(),
            block_id: None,
        },
    )
    .unwrap();

    let loaded = load_auto_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("run");
    let fix_attempts = loaded
        .steps
        .iter()
        .filter(|step| step.step_key == AutoStepKey::FixReview)
        .collect::<Vec<_>>();

    assert_eq!(fix_attempts.len(), 2);
    assert_eq!(fix_attempts[0].attempt, 1);
    assert_eq!(fix_attempts[1].attempt, 2);
    assert_eq!(
        load_output_lines(&conn, first_id).unwrap()[0].text,
        "first failed"
    );
    assert_eq!(
        load_output_lines(&conn, second_id).unwrap()[0].text,
        "second running"
    );
}

#[test]
fn pause_resume_fail_and_archive_round_trip() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted =
        AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
            .unwrap()
            .create_run();
    save_auto_run(&conn, &mut persisted).unwrap();

    request_auto_run_pause(&conn, &mut persisted).unwrap();
    let loaded = load_auto_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("paused");
    assert_eq!(loaded.run.status, AutoRunStatus::Paused);

    resume_paused_auto_run(&conn, &mut persisted).unwrap();
    assert_eq!(persisted.run.status, AutoRunStatus::Queued);

    fail_auto_run(&conn, &mut persisted, "verification failed").unwrap();
    archive_auto_run(&conn, &mut persisted).unwrap();
    let loaded = load_auto_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("archived");
    assert_eq!(loaded.run.status, AutoRunStatus::Failed);
    assert!(loaded.run.archived_unix_ms.is_some());
}

#[test]
fn stale_reconciliation_marks_active_steps_failed() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted =
        AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
            .unwrap()
            .create_run();
    persisted.run.status = AutoRunStatus::Running;
    persisted.steps[0].status = AutoStepStatus::Running;
    save_auto_run(&conn, &mut persisted).unwrap();

    let changed = reconcile_stale_auto_run(&conn, &mut persisted).unwrap();

    assert!(changed);
    let loaded = load_auto_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("run");
    assert_eq!(loaded.run.status, AutoRunStatus::Failed);
    assert_eq!(loaded.steps[0].status, AutoStepStatus::Failed);
    assert!(
        loaded.steps[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Prism restarted"))
    );
    let output = load_output_lines(&conn, loaded.steps[0].id.unwrap()).unwrap();
    assert!(output.iter().any(|line| {
        line.kind == AutoOutputKind::Error && line.text.contains("Prism restarted")
    }));
}

#[test]
fn recent_active_runs_excludes_archived_and_done_runs() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");

    let mut active = AutoLaunch::new(&repo, &repo.join("feature-a"), "feat/a", "Implement a")
        .unwrap()
        .create_run();
    let mut done = AutoLaunch::new(&repo, &repo.join("feature-b"), "feat/b", "Implement b")
        .unwrap()
        .create_run();
    done.run.status = AutoRunStatus::Done;
    let mut archived = AutoLaunch::new(&repo, &repo.join("feature-c"), "feat/c", "Implement c")
        .unwrap()
        .create_run();
    archived.run.status = AutoRunStatus::Failed;
    save_auto_run(&conn, &mut active).unwrap();
    save_auto_run(&conn, &mut done).unwrap();
    save_auto_run(&conn, &mut archived).unwrap();
    archive_auto_run(&conn, &mut archived).unwrap();

    let recent = load_recent_active_runs_for_repo(&conn, &repo, 10).unwrap();

    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].run.id, active.run.id);
}

#[test]
#[cfg(unix)]
fn executor_runs_fake_opencode_pauses_before_next_step_and_persists_events() {
    let temp = TempDir::new("executor-success");
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    setup_git_worktree(&origin, &work);
    let repo = Repository::with_config_dir_for_test(work.clone(), temp.path().join("prism-config"));
    let mut config = Config::load(&repo);
    config.default_base = None;
    seed_pr_cache(&repo, "feat/auto", "abc123");
    let opencode = temp.path().join("opencode");
    write_executable(
        &opencode,
        r#"#!/bin/sh
printf '%s\n' '{"type":"session","session_id":"ses_auto","title":"Auto Test"}'
printf '%s\n' '{"type":"message","text":"working on it"}'
printf '%s\n' '{"type":"tool.execute.before","id":"tool_1","name":"bash","command":"cargo test"}'
printf '%s\n' '{"type":"tool.execute.after","id":"tool_1","status":"success","output":"ok"}'
"#,
    );
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let mut persisted = AutoLaunch::new(&work, &work, "feat/auto", "Implement auto")
        .unwrap()
        .create_run();
    save_auto_run(&conn, &mut persisted).unwrap();
    let executor =
        AutoExecutorConfig::new(opencode.display().to_string(), None, work.clone(), "Auto");

    execute_auto_initial_step(
        &conn,
        &repo,
        &config,
        &mut persisted,
        &executor,
        &mut Vec::new(),
    )
    .unwrap();

    let loaded = load_auto_run(&conn, &persisted.run.id).unwrap().unwrap();
    assert_eq!(loaded.run.status, AutoRunStatus::Paused);
    assert!(loaded.run.pause_requested);
    assert_eq!(loaded.steps[0].status, AutoStepStatus::Done);
    let implement = loaded
        .steps
        .iter()
        .find(|step| step.step_key == AutoStepKey::Implement)
        .unwrap();
    assert_eq!(implement.status, AutoStepStatus::Done);
    assert_eq!(implement.opencode_session_id.as_deref(), Some("ses_auto"));
    assert_eq!(implement.summary.as_deref(), Some("working on it"));
    let verify = loaded
        .steps
        .iter()
        .find(|step| step.step_key == AutoStepKey::LocalVerify)
        .unwrap();
    assert_eq!(verify.status, AutoStepStatus::Queued);
    let lines = load_output_lines(&conn, implement.id.unwrap()).unwrap();
    assert!(
        lines
            .iter()
            .any(|line| { line.kind == AutoOutputKind::Tool && line.text.contains("cargo test") })
    );
    assert!(
        lines
            .iter()
            .any(|line| { line.kind == AutoOutputKind::ToolOutput && line.text == "ok" })
    );
}

#[test]
#[cfg(unix)]
fn executor_marks_failed_opencode_exit() {
    let temp = TempDir::new("executor-failed");
    let repo = Repository {
        root: temp.path().to_path_buf(),
    };
    let config = Config::load(&repo);
    let opencode = temp.path().join("opencode");
    write_executable(
        &opencode,
        r#"#!/bin/sh
printf '%s\n' '{"type":"message","text":"starting"}'
printf '%s\n' 'boom' >&2
exit 7
"#,
    );
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let mut persisted = AutoLaunch::new(temp.path(), temp.path(), "feat/auto", "Implement auto")
        .unwrap()
        .create_run();
    save_auto_run(&conn, &mut persisted).unwrap();
    let executor =
        AutoExecutorConfig::new(opencode.display().to_string(), None, temp.path(), "Auto");

    let error = execute_auto_initial_step(
        &conn,
        &repo,
        &config,
        &mut persisted,
        &executor,
        &mut Vec::new(),
    )
    .unwrap_err();

    assert!(error.contains("exited with 7"));
    let loaded = load_auto_run(&conn, &persisted.run.id).unwrap().unwrap();
    assert_eq!(loaded.run.status, AutoRunStatus::Failed);
    let implement = loaded
        .steps
        .iter()
        .find(|step| step.step_key == AutoStepKey::Implement)
        .unwrap();
    assert_eq!(implement.status, AutoStepStatus::Failed);
    assert!(
        implement
            .error
            .as_deref()
            .unwrap_or("")
            .contains("exited with 7")
    );
    let lines = load_output_lines(&conn, implement.id.unwrap()).unwrap();
    assert!(lines.iter().any(|line| line.text == "boom"));
}

#[test]
fn output_retention_keeps_marker_and_recent_lines() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted =
        AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
            .unwrap()
            .create_run();
    save_auto_run(&conn, &mut persisted).unwrap();
    let step_id = persisted.steps[0].id.unwrap();

    for line_number in 1..=5 {
        append_output_line_limited(
            &conn,
            &AutoOutputLine {
                step_run_id: step_id,
                line_number,
                time_unix_ms: line_number,
                kind: AutoOutputKind::Assistant,
                text: format!("line {line_number}"),
                block_id: None,
            },
            3,
        )
        .unwrap();
    }

    let lines = load_output_lines(&conn, step_id).unwrap();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].text.contains("omitted"));
    assert_eq!(lines[1].text, "line 4");
    assert_eq!(lines[2].text, "line 5");
}

#[test]
fn review_poll_detects_new_actionable_pr_comments() {
    let temp = TempDir::new("review-poll-actionable");
    let repo = Repository {
        root: temp.path().to_path_buf(),
    };
    let summary = test_pr_summary("feat/auto", "abc123", "2026-01-01T00:00:00Z");
    let config = Config::load(&repo);
    let details = crate::github::PrDetails {
        comments: vec![crate::github::PrComment {
            id: "comment-1".to_string(),
            author: "github-copilot".to_string(),
            body: "Please simplify this branch.".to_string(),
            severity: None,
            created_at: "2026-01-01T00:01:00Z".to_string(),
        }],
        ..crate::github::PrDetails::default()
    };
    let mut persisted = AutoLaunch::new(temp.path(), temp.path(), "feat/auto", "Implement auto")
        .unwrap()
        .create_run();
    persisted.run.review_baseline_json = Some(
        serde_json::to_string(&ReviewBaseline {
            head_sha: "abc123".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        })
        .unwrap(),
    );

    let outcome =
        evaluate_review_feedback(&config, &mut persisted, &summary, Some(&details)).unwrap();

    assert!(outcome.fix_prompt.is_some());
    let prompt = outcome.fix_prompt.unwrap();
    assert!(prompt.contains("PR comments:"));
    assert!(prompt.contains("Please simplify this branch."));
    assert!(!outcome.complete);
}

#[test]
fn review_poll_skips_feedback_at_or_before_baseline() {
    let temp = TempDir::new("review-poll-old");
    let repo = Repository {
        root: temp.path().to_path_buf(),
    };
    let summary = test_pr_summary("feat/auto", "abc123", "2026-01-01T00:05:00Z");
    let config = Config::load(&repo);
    let details = crate::github::PrDetails {
        comments: vec![crate::github::PrComment {
            id: "comment-1".to_string(),
            author: "github-copilot".to_string(),
            body: "Already handled.".to_string(),
            severity: None,
            created_at: "2026-01-01T00:05:00Z".to_string(),
        }],
        ..crate::github::PrDetails::default()
    };
    let mut persisted = AutoLaunch::new(temp.path(), temp.path(), "feat/auto", "Implement auto")
        .unwrap()
        .create_run();
    persisted.run.review_baseline_json = Some(
        serde_json::to_string(&ReviewBaseline {
            head_sha: "abc123".to_string(),
            updated_at: "2026-01-01T00:05:00Z".to_string(),
        })
        .unwrap(),
    );

    let outcome =
        evaluate_review_feedback(&config, &mut persisted, &summary, Some(&details)).unwrap();

    assert!(outcome.fix_prompt.is_none());
    assert!(outcome.complete);
    assert!(outcome.summary.contains("no actionable review feedback"));
}

#[test]
fn ci_status_waits_while_checks_are_pending() {
    let temp = TempDir::new("ci-pending");
    let repo = Repository {
        root: temp.path().to_path_buf(),
    };
    let config = Config::load(&repo);
    let mut summary = test_pr_summary("feat/auto", "abc123", "2026-01-01T00:00:00Z");
    summary.check_status = "running".to_string();

    let outcome = evaluate_ci_status(&config, "feat/auto", &summary, None).unwrap();

    assert_eq!(outcome.state, PrCheckState::Pending);
    assert!(outcome.summary.contains("still running"));
}

#[test]
fn ci_status_builds_failure_prompt_with_logs() {
    let temp = TempDir::new("ci-failed");
    let repo = Repository {
        root: temp.path().to_path_buf(),
    };
    let config = Config::load(&repo);
    let mut summary = test_pr_summary("feat/auto", "abc123", "2026-01-01T00:00:00Z");
    summary.check_status = "failed".to_string();
    let details = PrDetails {
        failing_checks: vec!["test".to_string()],
        ci_failures: vec![crate::github::CiFailure {
            workflow: "CI".to_string(),
            name: "test".to_string(),
            conclusion: "failure".to_string(),
            url: "https://example.com/actions/runs/1".to_string(),
            run_id: "1".to_string(),
            log_tail: "assertion failed".to_string(),
        }],
        ..PrDetails::default()
    };

    let outcome = evaluate_ci_status(&config, "feat/auto", &summary, Some(&details)).unwrap();

    assert_eq!(outcome.state, PrCheckState::Failed);
    assert!(outcome.summary.contains("CI failed"));
    assert!(outcome.prompt.contains("Head SHA: abc123"));
    assert!(outcome.prompt.contains("assertion failed"));
}

#[test]
fn merge_success_queues_cleanup_separately() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted =
        AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
            .unwrap()
            .create_run();
    persisted.steps.clear();
    push_test_step(&mut persisted, 1, AutoStepKey::WaitCi, AutoStepStatus::Done);
    push_test_step(&mut persisted, 2, AutoStepKey::Merge, AutoStepStatus::Done);
    save_auto_run(&conn, &mut persisted).unwrap();

    assert!(ensure_next_auto_step(&conn, &mut persisted).unwrap());

    assert!(
        persisted
            .steps
            .iter()
            .any(|step| step.step_key == AutoStepKey::Cleanup)
    );
    assert_ne!(persisted.run.status, AutoRunStatus::Done);
}

#[test]
fn manual_merge_skip_completes_run_without_cleanup() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted =
        AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
            .unwrap()
            .create_run();
    persisted.steps.clear();
    push_test_step(&mut persisted, 1, AutoStepKey::WaitCi, AutoStepStatus::Done);
    push_test_step(
        &mut persisted,
        2,
        AutoStepKey::Merge,
        AutoStepStatus::Skipped,
    );
    save_auto_run(&conn, &mut persisted).unwrap();

    assert!(!ensure_next_auto_step(&conn, &mut persisted).unwrap());

    assert_eq!(persisted.run.status, AutoRunStatus::Done);
    assert!(
        !persisted
            .steps
            .iter()
            .any(|step| step.step_key == AutoStepKey::Cleanup)
    );
}

#[test]
fn merge_gate_blocks_dirty_draft_failed_ci_stale_head_and_review_feedback() {
    let temp = TempDir::new("merge-gate-blockers");
    let repo = Repository {
        root: temp.path().to_path_buf(),
    };
    let config = Config::load(&repo);
    let mut summary = test_pr_summary("feat/auto", "remote-head", "2026-01-01T00:00:00Z");
    summary.check_status = "failed".to_string();
    summary.draft = true;
    let details = PrDetails {
        comments: vec![crate::github::PrComment {
            id: "comment-1".to_string(),
            author: "github-copilot".to_string(),
            body: "Please fix this before merging.".to_string(),
            severity: None,
            created_at: "2026-01-01T00:01:00Z".to_string(),
        }],
        ..PrDetails::default()
    };
    let mut persisted = AutoLaunch::new(temp.path(), temp.path(), "feat/auto", "Implement auto")
        .unwrap()
        .create_run();
    persisted.run.review_baseline_json = Some(
        serde_json::to_string(&ReviewBaseline {
            head_sha: "remote-head".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        })
        .unwrap(),
    );
    let verify = verify_result(false);

    let outcome = evaluate_merge_gate(
        &config,
        &persisted,
        &summary,
        Some(&details),
        "local-head",
        true,
        &verify,
    );

    assert!(!outcome.allowed);
    assert!(outcome.summary.contains("worktree is dirty"));
    assert!(outcome.summary.contains("PR is draft"));
    assert!(outcome.summary.contains("CI state is failed"));
    assert!(outcome.summary.contains("does not match local head"));
    assert!(
        outcome
            .summary
            .contains("actionable review feedback remains")
    );
    assert!(outcome.summary.contains("final local verification failed"));
}

#[cfg(unix)]
#[test]
fn merge_step_uses_gh_merge_and_waits_for_merged_status() {
    let temp = TempDir::new("merge-step-success");
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    setup_git_worktree(&origin, &work);
    let repo = Repository::with_config_dir_for_test(work.clone(), temp.path().join("prism-config"));
    let mut config = Config::load(&repo);
    config.auto.merge = true;
    config.auto.review_wait_enabled = false;
    let gh_log = temp.path().join("gh.log");
    let head = crate::git::current_head_sha(&work, &config).unwrap();
    let gh = temp.path().join("gh");
    write_executable(
        &gh,
        &format!(
            r#"#!/bin/sh
printf 'args=%s\n' "$*" >> '{}'
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "feat/auto" ]; then
  cat <<'JSON'
{{"number":42,"title":"Auto","body":"","url":"https://example.com/pr/42","state":"OPEN","reviewDecision":"APPROVED","reviewRequests":[],"headRefName":"feat/auto","baseRefName":"main","headRefOid":"{}","updatedAt":"2026-01-01T00:00:00Z","statusCheckRollup":{{"contexts":{{"nodes":[{{"__typename":"StatusContext","context":"ci","state":"SUCCESS"}}]}}}},"mergedAt":null,"isDraft":false}}
JSON
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "42" ]; then
  printf '%s\n' '{{"state":"MERGED","mergedAt":"2026-01-01T00:02:00Z"}}'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "merge" ]; then
  exit 0
fi
exit 1
"#,
            gh_log.display(),
            head
        ),
    );
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let mut persisted = AutoLaunch::new(&work, &work, "feat/auto", "Implement auto")
        .unwrap()
        .create_run();
    persisted.steps.clear();
    persisted.steps.push(AutoStepRun::queued(
        &persisted.run.id,
        1,
        AutoStepKey::Merge,
        1,
        Some("merge".to_string()),
    ));
    save_auto_run(&conn, &mut persisted).unwrap();
    start_non_agent_step(&conn, &mut persisted, 0).unwrap();

    execute_merge_step(&conn, &repo, &config, &mut persisted, 0, 100).unwrap();

    let loaded = load_auto_run(&conn, &persisted.run.id).unwrap().unwrap();
    assert_eq!(loaded.steps[0].status, AutoStepStatus::Done);
    let commands = fs::read_to_string(gh_log).unwrap();
    assert!(commands.contains("args=pr merge 42 --squash"));
    assert!(commands.contains("args=pr view 42 --json state,mergedAt"));
}

fn push_test_step(
    persisted: &mut PersistedAutoRun,
    sequence: usize,
    step_key: AutoStepKey,
    status: AutoStepStatus,
) {
    persisted.steps.push(AutoStepRun {
        id: None,
        run_id: persisted.run.id.clone(),
        sequence,
        step_key,
        reason: None,
        status,
        attempt: 1,
        started_unix_ms: None,
        finished_unix_ms: None,
        opencode_server_url: None,
        opencode_session_id: None,
        process_id: None,
        plan_run_id: None,
        commit_sha: None,
        head_sha: None,
        work_guard: None,
        blocker: None,
        summary: Some("done".to_string()),
        error: None,
    });
}

fn linked_run_plan_auto_run(conn: &rusqlite::Connection, repo: &Path) -> PersistedAutoRun {
    let mut persisted = AutoLaunch::with_options(
        repo,
        repo,
        AutoLaunchOptions {
            branch: "feat/auto".to_string(),
            mode: AutoRunMode::Standard,
            implementation_source: AutoImplementationSource::ExistingPlan,
            plan_path: Some(repo.join("plan.md")),
            plan_run_mode: PlanRunMode::Sequential,
            variant: "default".to_string(),
            agent_profile: None,
            initial_prompt: "Implement existing plan".to_string(),
        },
    )
    .unwrap()
    .create_run();
    let plan_launch = crate::plan_run::PlanLaunch::new(
        repo,
        repo,
        &repo.join("plan.md"),
        "phase",
        1,
        1,
        PlanRunMode::Sequential,
    )
    .unwrap();
    let plan_run = plan_launch.create_run();
    crate::plan_run::save_plan_run(conn, &plan_run).unwrap();
    persisted.steps.clear();
    persisted.steps.push(AutoStepRun::running(
        &persisted.run.id,
        1,
        AutoStepKey::RunPlan,
        1,
    ));
    persisted.steps[0].plan_run_id = Some(plan_run.run.id);
    persisted.run.status = AutoRunStatus::Running;
    save_auto_run(conn, &mut persisted).unwrap();
    persisted
}

fn verify_result(passed: bool) -> VerifyResult {
    VerifyResult {
        passed,
        checks: vec![crate::verify::VerifyCheckResult {
            kind: crate::verify::VerifyCheckKind::Configured,
            label: "test".to_string(),
            passed,
            message: "test".to_string(),
        }],
    }
}

fn test_config() -> Config {
    Config {
        default_agent: "opencode".to_string(),
        default_base: None,
        plan_dir: "plans".to_string(),
        review_packet_dir: ".agent/review".to_string(),
        worktree_command: "wt".to_string(),
        opencode_port_base: 41_000,
        opencode_port_span: 1_000,
        opencode_shutdown_owned_servers: false,
        opencode_plan_plugin: false,
        escape_key: EscapeKey::EscEsc,
        merge_method: MergeMethod::Squash,
        icon_style: crate::config::IconStyle::Unicode,
        icon_style_configured: false,
        auto: crate::config::AutoConfig::default(),
        layout: crate::config::LayoutConfig::default(),
        checks: Checks::default(),
        worktree_columns: Vec::new(),
        tools: BTreeMap::new(),
        agent_commands: BTreeMap::new(),
        agent_prompt_modes: BTreeMap::new(),
        prompt_templates: BTreeMap::new(),
        user_path: PathBuf::from("/tmp/prism-user-config.toml"),
        repo_config_path: PathBuf::from("/tmp/prism-repo-config.toml"),
    }
}

#[cfg(unix)]
fn write_executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::write(path, text).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[cfg(unix)]
fn setup_git_worktree(origin: &Path, work: &Path) {
    run(Command::new("git").args(["init", "--bare"]).arg(origin));
    run(Command::new("git").arg("--git-dir").arg(origin).args([
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]));
    run(Command::new("git").arg("clone").arg(origin).arg(work));
    run_git(work, &["config", "user.email", "test@example.com"]);
    run_git(work, &["config", "user.name", "Test User"]);
    fs::write(work.join("tracked.txt"), "base\n").unwrap();
    run_git(work, &["add", "tracked.txt"]);
    run_git(work, &["commit", "-m", "initial"]);
    run_git(work, &["push", "-u", "origin", "main"]);
    run_git(work, &["switch", "-c", "feat/auto"]);
}

#[cfg(unix)]
fn seed_pr_cache(repo: &Repository, branch: &str, head_sha: &str) {
    let cache = crate::github::PrCache {
        summary: Some(crate::github::PrSummary {
            number: 42,
            title: "Auto".to_string(),
            body: String::new(),
            url: "https://example.com/pr/42".to_string(),
            state: "OPEN".to_string(),
            review_decision: String::new(),
            requested_reviewers: Vec::new(),
            head_ref: branch.to_string(),
            base_ref: "main".to_string(),
            head_sha: head_sha.to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            check_status: "passed".to_string(),
            merge_state_status: "CLEAN".to_string(),
            comment_count: 0,
            merged: false,
            draft: false,
        }),
        ..crate::github::PrCache::default()
    };
    crate::github::save_pr_cache(repo, branch, &cache).unwrap();
}

fn test_pr_summary(branch: &str, head_sha: &str, updated_at: &str) -> crate::github::PrSummary {
    crate::github::PrSummary {
        number: 42,
        title: "Auto".to_string(),
        body: String::new(),
        url: "https://example.com/pr/42".to_string(),
        state: "OPEN".to_string(),
        review_decision: String::new(),
        requested_reviewers: vec!["github-copilot".to_string()],
        head_ref: branch.to_string(),
        base_ref: "main".to_string(),
        head_sha: head_sha.to_string(),
        updated_at: updated_at.to_string(),
        check_status: "unknown".to_string(),
        merge_state_status: "CLEAN".to_string(),
        comment_count: 1,
        merged: false,
        draft: false,
    }
}

#[cfg(unix)]
fn run_git(path: &Path, args: &[&str]) {
    run(Command::new("git").arg("-C").arg(path).args(args));
}

#[cfg(unix)]
fn git_output(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "command failed: git -C {} {}\nstdout: {}\nstderr: {}",
        path.display(),
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[cfg(unix)]
fn run(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "command failed: {:?}\nstdout: {}\nstderr: {}",
        command,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
