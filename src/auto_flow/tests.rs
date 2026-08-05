use super::*;
use std::fs;
use std::process::Command;

use crate::config::Config;
use crate::test_support::write_executable;

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
fn worktree_incarnation_round_trips_and_legacy_rows_remain_unknown() {
    let temp = TempDir::new("worktree-incarnation");
    let worktree = temp.path().join("feature");
    fs::create_dir_all(&worktree).unwrap();
    fs::write(
        worktree.join(".git"),
        "gitdir: /repo/.git/worktrees/feature\n",
    )
    .unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let mut persisted = AutoLaunch::new(temp.path(), &worktree, "feat/auto", "Implement auto")
        .unwrap()
        .create_run();
    let incarnation = persisted
        .run
        .worktree_incarnation
        .clone()
        .expect("new run incarnation");

    save_auto_run(&conn, &mut persisted).unwrap();
    let loaded = load_auto_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("saved run");

    assert_eq!(
        loaded.run.worktree_incarnation.as_deref(),
        Some(incarnation.as_str())
    );
    conn.execute(
        "update auto_run set worktree_incarnation = null where id = ?1",
        params![persisted.run.id],
    )
    .unwrap();
    let legacy = load_auto_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("legacy run");
    assert_eq!(legacy.run.worktree_incarnation, None);
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

    let config = test_config();
    let create_prompt = prompt_for_step(
        &config,
        &persisted.run,
        &AutoStepRun::queued(&persisted.run.id, 2, AutoStepKey::CreatePlan, 1, None),
    );
    let review_prompt = prompt_for_step(
        &config,
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
fn auto_prompt_template_overrides_default_and_renders_context() {
    let repo = PathBuf::from("/repo/prism");
    let persisted = AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
        .unwrap()
        .create_run();
    let mut config = test_config();
    config.prompt_templates.insert(
        "auto_implement".to_string(),
        "Task={{task}} Branch={{branch}} Literal={{missing}}".to_string(),
    );
    let mut persisted = persisted;
    persisted.run.initial_prompt = "Keep {{branch}} literal".to_string();

    let prompt = prompt_for_step(
        &config,
        &persisted.run,
        &AutoStepRun::queued(&persisted.run.id, 2, AutoStepKey::Implement, 1, None),
    );

    assert_eq!(
        prompt,
        "Task=Keep {{branch}} literal Branch=feat/auto Literal={{missing}}"
    );
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

    let outcome =
        apply_auto_run_control(&conn, &persisted.run.id, AutoRunControlIntent::Resume).unwrap();
    assert_eq!(outcome.executor, AutoExecutorDecision::Start);
    persisted = outcome.run;
    assert!(ensure_next_test_step(&conn, &mut persisted).unwrap());
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

    assert!(ensure_next_test_step(&conn, &mut persisted).unwrap());

    assert_eq!(persisted.steps[1].step_key, AutoStepKey::RunPlan);
}

#[test]
fn existing_pull_request_source_skips_implementation_pipeline() {
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = AutoLaunch::with_options(
        &repo,
        &repo.join("feature"),
        AutoLaunchOptions {
            branch: "feat/auto".to_string(),
            mode: AutoRunMode::Standard,
            implementation_source: AutoImplementationSource::ExistingPullRequest,
            plan_path: None,
            plan_run_mode: PlanRunMode::Sequential,
            variant: "existing-pr".to_string(),
            agent_profile: None,
            initial_prompt: "Stabilize existing pull request".to_string(),
        },
    )
    .unwrap()
    .create_run();
    persisted.steps[0].status = AutoStepStatus::Done;

    assert!(next_state_machine_step_needed(&persisted));
    assert!(!implementation_follow_up_step_needed(&persisted));
    assert!(persisted.steps.iter().all(|step| !matches!(
        step.step_key,
        AutoStepKey::Implement
            | AutoStepKey::RunPlan
            | AutoStepKey::LocalVerify
            | AutoStepKey::CommitImpl
            | AutoStepKey::PushPr
    )));
}

#[test]
fn auto_submission_rejects_a_second_active_run_for_the_worktree() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    crate::execution::migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let worktree = repo.join("feature");
    let mut first = AutoLaunch::new(&repo, &worktree, "feat/auto", "First task")
        .unwrap()
        .create_run();
    let mut second = AutoLaunch::new(&repo, &worktree, "feat/auto", "Second task")
        .unwrap()
        .create_run();

    submit_auto_run(&conn, &mut first).unwrap();
    let error = submit_auto_run(&conn, &mut second).unwrap_err();

    assert_eq!(
        error,
        format!("worktree already has active Auto Flow run {}", first.run.id)
    );
    assert!(load_auto_run(&conn, &second.run.id).unwrap().is_none());
}

#[test]
#[cfg(unix)]
fn existing_pull_request_adoption_allows_stabilization_to_report_head_divergence() {
    let temp = TempDir::new("adopt-existing-pr-diverged");
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    setup_git_worktree(&origin, &work);
    run_git(&work, &["push", "-u", "origin", "feat/auto"]);
    let repo = Repository::with_config_dir_for_test(work.clone(), temp.path().join("prism-config"));
    let mut config = Config::load(&repo);
    let pr_head = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    configure_pr_observation(&temp, &mut config, "feat/auto", pr_head);
    let mut persisted = AutoLaunch::with_options(
        &work,
        &work,
        AutoLaunchOptions {
            branch: "feat/auto".to_string(),
            mode: AutoRunMode::Standard,
            implementation_source: AutoImplementationSource::ExistingPullRequest,
            plan_path: None,
            plan_run_mode: PlanRunMode::Sequential,
            variant: "existing-pr".to_string(),
            agent_profile: None,
            initial_prompt: "Stabilize existing pull request".to_string(),
        },
    )
    .unwrap()
    .create_run();

    stabilization_observe::adopt_existing_pull_request(&repo, &config, &mut persisted).unwrap();

    assert_eq!(persisted.run.pr_number, Some(42));
    assert_eq!(
        crate::remote::load_pr_cache(&repo, "feat/auto")
            .summary()
            .map(|summary| summary.head_sha.as_str()),
        Some(pr_head)
    );
    let snapshot = stabilization_observe::build_auto_run_stabilization_snapshot(
        &repo,
        &persisted.run,
        &config,
    );
    assert!(snapshot.pull_request.is_some());
    assert!(
        stabilization_plan::derive_blockers(&snapshot)
            .contains(&stabilization_model::StabilizationBlocker::HeadDiverged)
    );
}

#[test]
#[cfg(unix)]
fn prompt_implementation_pr_delegates_to_stabilization_ready_state() {
    let temp = TempDir::new("stabilization-ready-delegation");
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    setup_git_worktree(&origin, &work);
    run_git(&work, &["push", "-u", "origin", "feat/auto"]);
    let head = git_output(&work, &["rev-parse", "HEAD"]);
    let repo = Repository::with_config_dir_for_test(work.clone(), temp.path().join("prism-config"));
    let mut config = Config::load(&repo);
    config.auto.review_requirement = crate::config::ReviewRequirement::None;
    configure_pr_observation(&temp, &mut config, "feat/auto", &head);
    seed_pr_cache(&repo, "feat/auto", &head);
    crate::remote::save_repo_policy_cache(
        &repo,
        &crate::remote::RepoPolicyCache {
            repo_remote: "example/repo".to_string(),
            provider: Some(crate::remote::ProviderKind::GitHub),
            canonical_host: Some("github.com".to_string()),
            project_path: Some("example/repo".to_string()),
            target_branch: Some("main".to_string()),
            identity_complete: true,
            default_branch: Some("main".to_string()),
            ..crate::remote::RepoPolicyCache::default()
        },
    )
    .unwrap();
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

    assert!(
        ensure_next_auto_step_with_context(&conn, &repo, &config, &mut persisted).unwrap(),
        "status={:?} blocker={:?} next={:?}",
        persisted.run.stabilization_status,
        persisted.run.stabilization_blocker,
        persisted.run.stabilization_next_work
    );

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

    assert!(ensure_next_test_step(&conn, &mut persisted).unwrap());
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
    assert!(ensure_next_test_step(&conn, &mut persisted).unwrap());
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

    let outcome =
        apply_auto_run_control(&conn, &persisted.run.id, AutoRunControlIntent::RetryFailed)
            .unwrap();
    let persisted = outcome.run;

    assert_eq!(outcome.executor, AutoExecutorDecision::Start);
    assert!(!auto_run_execution_blocked(&persisted));
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

    let outcome =
        apply_auto_run_control(&conn, &persisted.run.id, AutoRunControlIntent::RetryFailed)
            .unwrap();
    let mut persisted = outcome.run;

    assert_eq!(outcome.executor, AutoExecutorDecision::Start);
    assert!(!auto_run_execution_blocked(&persisted));
    assert_eq!(persisted.steps[0].status, AutoStepStatus::Done);
    assert_eq!(persisted.run.status, AutoRunStatus::Done);
    assert!(ensure_next_test_step(&conn, &mut persisted).unwrap());
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

    let outcome = apply_auto_run_control(
        &conn,
        &persisted.run.id,
        AutoRunControlIntent::RetryFromStep {
            step_run_id: selected,
        },
    )
    .unwrap();
    let persisted = outcome.run;

    assert_eq!(outcome.executor, AutoExecutorDecision::Start);
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
fn auto_control_pause_and_resume_returns_authoritative_execution_decision() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted =
        AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
            .unwrap()
            .create_run();
    save_auto_run(&conn, &mut persisted).unwrap();

    let paused =
        apply_auto_run_control(&conn, &persisted.run.id, AutoRunControlIntent::Pause).unwrap();

    assert_eq!(paused.effect, AutoRunControlEffect::Paused);
    assert_eq!(paused.executor, AutoExecutorDecision::DoNotStart);
    assert_eq!(paused.run.run.status, AutoRunStatus::Paused);
    assert!(paused.run.run.pause_requested);

    let resumed =
        apply_auto_run_control(&conn, &persisted.run.id, AutoRunControlIntent::Resume).unwrap();

    assert_eq!(resumed.effect, AutoRunControlEffect::Resumed);
    assert_eq!(resumed.executor, AutoExecutorDecision::Start);
    assert_eq!(resumed.run.run.status, AutoRunStatus::Queued);
    assert!(!resumed.run.run.pause_requested);
    assert_eq!(
        load_auto_run(&conn, &persisted.run.id)
            .unwrap()
            .unwrap()
            .run,
        resumed.run.run
    );
}

#[test]
fn auto_control_resume_reconciles_linked_plan_before_deciding_to_execute() {
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
    persisted.run.pause_requested = true;
    persisted.run.status = AutoRunStatus::Paused;
    save_auto_run(&conn, &mut persisted).unwrap();

    let outcome =
        apply_auto_run_control(&conn, &persisted.run.id, AutoRunControlIntent::Resume).unwrap();

    assert_eq!(outcome.executor, AutoExecutorDecision::Start);
    assert_eq!(outcome.run.steps[0].status, AutoStepStatus::Queued);
    let loaded_plan = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
    assert_eq!(
        loaded_plan.steps[0].status,
        crate::plan_run::PlanStepStatus::Queued
    );
}

#[test]
#[cfg(unix)]
fn auto_control_resume_clears_pause_while_linked_plan_process_is_live() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = linked_run_plan_auto_run(&conn, &repo);
    let plan_run_id = persisted.steps[0].plan_run_id.clone().unwrap();
    let mut plan_run = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
    plan_run.run.status = PlanRunStatus::Running;
    plan_run.run.pause_requested = true;
    plan_run.steps[0].status = crate::plan_run::PlanStepStatus::Running;
    plan_run.steps[0].execution.process_id = Some(std::process::id());
    crate::plan_run::save_plan_run(&conn, &plan_run).unwrap();
    persisted.run.pause_requested = true;
    persisted.run.status = AutoRunStatus::Paused;
    save_auto_run(&conn, &mut persisted).unwrap();

    let outcome =
        apply_auto_run_control(&conn, &persisted.run.id, AutoRunControlIntent::Resume).unwrap();

    assert_eq!(outcome.executor, AutoExecutorDecision::AlreadyRunning);
    assert!(!outcome.run.run.pause_requested);
    assert_eq!(outcome.run.run.status, AutoRunStatus::Running);
    let loaded_plan = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
    assert!(!loaded_plan.run.pause_requested);
    assert_eq!(loaded_plan.run.status, PlanRunStatus::Running);
}

#[test]
fn auto_control_abort_step_persists_run_and_step_together() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted =
        AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
            .unwrap()
            .create_run();
    save_auto_run(&conn, &mut persisted).unwrap();
    let step_run_id = persisted.steps[0].id.unwrap();

    let outcome = apply_auto_run_control(
        &conn,
        &persisted.run.id,
        AutoRunControlIntent::AbortStep { step_run_id },
    )
    .unwrap();

    assert_eq!(
        outcome.effect,
        AutoRunControlEffect::AbortedStep { step_run_id }
    );
    assert_eq!(outcome.executor, AutoExecutorDecision::DoNotStart);
    assert!(outcome.warnings.is_empty());
    assert_eq!(outcome.run.run.status, AutoRunStatus::Aborted);
    assert_eq!(outcome.run.steps[0].status, AutoStepStatus::Aborted);
    assert!(outcome.run.steps[0].finished_unix_ms.is_some());
    let loaded = load_auto_run(&conn, &persisted.run.id).unwrap().unwrap();
    assert_eq!(loaded.run.status, AutoRunStatus::Aborted);
    assert_eq!(loaded.steps[0].status, AutoStepStatus::Aborted);
}

#[test]
fn auto_control_abort_run_only_aborts_active_or_pending_steps() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted =
        AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
            .unwrap()
            .create_run();
    persisted.steps.clear();
    push_test_step(
        &mut persisted,
        1,
        AutoStepKey::Implement,
        AutoStepStatus::Done,
    );
    push_test_step(
        &mut persisted,
        2,
        AutoStepKey::LocalVerify,
        AutoStepStatus::Queued,
    );
    push_test_step(
        &mut persisted,
        3,
        AutoStepKey::RunPlan,
        AutoStepStatus::Waiting,
    );
    push_test_step(
        &mut persisted,
        4,
        AutoStepKey::FixCi,
        AutoStepStatus::Running,
    );
    persisted.run.status = AutoRunStatus::Running;
    persisted.run.pause_requested = true;
    save_auto_run(&conn, &mut persisted).unwrap();
    crate::integration::arm_merge_intent(&conn, &persisted.run.id).unwrap();

    let outcome =
        apply_auto_run_control(&conn, &persisted.run.id, AutoRunControlIntent::AbortRun).unwrap();

    assert_eq!(outcome.effect, AutoRunControlEffect::AbortedRun);
    assert_eq!(outcome.executor, AutoExecutorDecision::DoNotStart);
    assert_eq!(outcome.run.run.status, AutoRunStatus::Aborted);
    assert!(!outcome.run.run.pause_requested);
    assert_eq!(outcome.run.steps[0].status, AutoStepStatus::Done);
    assert!(
        outcome.run.steps[1..]
            .iter()
            .all(|step| step.status == AutoStepStatus::Aborted)
    );
    assert!(
        crate::integration::active_merge_intent(&conn, &persisted.run.id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn auto_control_abort_run_stops_linked_plan_execution() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let persisted = linked_run_plan_auto_run(&conn, &repo);
    let plan_run_id = persisted.steps[0].plan_run_id.clone().unwrap();
    let mut plan_run = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
    plan_run.run.status = PlanRunStatus::Running;
    plan_run.steps[0].status = crate::plan_run::PlanStepStatus::Running;
    crate::plan_run::save_plan_run(&conn, &plan_run).unwrap();

    let outcome =
        apply_auto_run_control(&conn, &persisted.run.id, AutoRunControlIntent::AbortRun).unwrap();

    assert!(outcome.warnings.is_empty());
    assert_eq!(outcome.run.run.status, AutoRunStatus::Aborted);
    let loaded_plan = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
    assert_eq!(loaded_plan.run.status, PlanRunStatus::Aborted);
    assert_eq!(
        loaded_plan.steps[0].status,
        crate::plan_run::PlanStepStatus::Aborted
    );
}

#[test]
fn auto_control_abort_run_stops_queued_linked_plan() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let persisted = linked_run_plan_auto_run(&conn, &repo);
    let plan_run_id = persisted.steps[0].plan_run_id.clone().unwrap();

    let outcome =
        apply_auto_run_control(&conn, &persisted.run.id, AutoRunControlIntent::AbortRun).unwrap();

    assert!(outcome.warnings.is_empty());
    let loaded_plan = load_plan_run(&conn, &plan_run_id).unwrap().unwrap();
    assert_eq!(loaded_plan.run.status, PlanRunStatus::Aborted);
    assert!(
        loaded_plan
            .steps
            .iter()
            .all(|step| step.status == crate::plan_run::PlanStepStatus::Aborted)
    );
}

#[test]
fn auto_control_rejects_step_from_another_run() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut first = AutoLaunch::new(&repo, &repo.join("first"), "feat/first", "First")
        .unwrap()
        .create_run();
    let mut second = AutoLaunch::new(&repo, &repo.join("second"), "feat/second", "Second")
        .unwrap()
        .create_run();
    save_auto_run(&conn, &mut first).unwrap();
    save_auto_run(&conn, &mut second).unwrap();
    let foreign_step_run_id = second.steps[0].id.unwrap();

    let error = apply_auto_run_control(
        &conn,
        &first.run.id,
        AutoRunControlIntent::AbortStep {
            step_run_id: foreign_step_run_id,
        },
    )
    .expect_err("foreign step should fail");

    assert_eq!(
        error,
        format!("auto flow step not found: {foreign_step_run_id}")
    );
    let loaded = load_auto_run(&conn, &first.run.id).unwrap().unwrap();
    assert_eq!(loaded.run.status, AutoRunStatus::Queued);
    assert_eq!(loaded.steps[0].status, AutoStepStatus::Queued);
}

#[test]
fn auto_control_abort_warning_keeps_authoritative_state_persisted() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted =
        AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
            .unwrap()
            .create_run();
    persisted.run.status = AutoRunStatus::Running;
    persisted.steps[0].status = AutoStepStatus::Running;
    persisted.steps[0].session.adapter_id = Some("opencode".to_string());
    persisted.steps[0].session.id = Some("session-1".to_string());
    save_auto_run(&conn, &mut persisted).unwrap();
    let step_run_id = persisted.steps[0].id.unwrap();

    let outcome = apply_auto_run_control(
        &conn,
        &persisted.run.id,
        AutoRunControlIntent::AbortStep { step_run_id },
    )
    .unwrap();

    assert_eq!(outcome.warnings.len(), 1);
    assert!(outcome.warnings[0].contains("OpenCode session has no endpoint"));
    assert_eq!(outcome.run.run.status, AutoRunStatus::Aborted);
    assert_eq!(outcome.run.steps[0].status, AutoStepStatus::Aborted);
    let loaded = load_auto_run(&conn, &persisted.run.id).unwrap().unwrap();
    assert_eq!(loaded.run.status, AutoRunStatus::Aborted);
    assert_eq!(loaded.steps[0].status, AutoStepStatus::Aborted);
}

#[test]
fn stale_executor_snapshot_does_not_overwrite_aborted_run() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let root = PathBuf::from("/repo/prism");
    let worktree = root.join("feature");
    let mut persisted = AutoLaunch::new(&root, &worktree, "feat/auto", "Implement auto")
        .unwrap()
        .create_run();
    save_auto_run(&conn, &mut persisted).unwrap();
    let mut stale_executor_snapshot = persisted.clone();
    apply_auto_run_control(&conn, &persisted.run.id, AutoRunControlIntent::AbortRun).unwrap();
    let executor = AutoExecutorConfig::new("unused", None, worktree, "stale executor");

    execute_auto_initial_step(
        &conn,
        &Repository { root },
        &test_config(),
        &mut stale_executor_snapshot,
        &executor,
        &mut Vec::new(),
    )
    .unwrap();

    assert_eq!(stale_executor_snapshot.run.status, AutoRunStatus::Aborted);
    let loaded = load_auto_run(&conn, &persisted.run.id).unwrap().unwrap();
    assert_eq!(loaded.run.status, AutoRunStatus::Aborted);
    assert_eq!(loaded.steps[0].status, AutoStepStatus::Aborted);
}

#[test]
fn completed_agent_process_does_not_overwrite_concurrent_abort() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let root = PathBuf::from("/repo/prism");
    let mut persisted = AutoLaunch::new(&root, &root.join("feature"), "feat/auto", "Implement")
        .unwrap()
        .create_run();
    persisted.steps[0].status = AutoStepStatus::Running;
    save_auto_run(&conn, &mut persisted).unwrap();
    let mut stale = persisted.steps[0].clone();
    control::abort_auto_step(&conn, &mut persisted.steps[0]).unwrap();

    agent_step::finish_step_after_exit(&conn, &mut stale, 143, false, "test").unwrap();

    assert_eq!(stale.status, AutoStepStatus::Aborted);
    assert_eq!(
        load_auto_run(&conn, &persisted.run.id)
            .unwrap()
            .unwrap()
            .steps[0]
            .status,
        AutoStepStatus::Aborted
    );
}

#[test]
#[cfg(unix)]
fn abort_during_start_prevents_spawned_auto_process_from_becoming_running() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let root = PathBuf::from("/repo/prism");
    let mut persisted = AutoLaunch::new(&root, &root.join("feature"), "feat/auto", "Implement")
        .unwrap()
        .create_run();
    persisted.steps[0].status = AutoStepStatus::Starting;
    save_auto_run(&conn, &mut persisted).unwrap();
    let invocation = crate::harness::Invocation {
        argv: vec!["sleep".to_string(), "30".to_string()],
        environment: std::collections::BTreeMap::new(),
        stdin: None,
        prompt_file: None,
        structured_events: false,
        attach: false,
    };
    let mut child = invocation.spawn(Path::new("/tmp")).unwrap();

    control::abort_auto_step(&conn, &mut persisted.steps[0]).unwrap();
    assert!(
        !agent_step::claim_spawned_process(&conn, &mut persisted.steps[0], &mut child).unwrap()
    );

    assert_eq!(
        load_auto_run(&conn, &persisted.run.id)
            .unwrap()
            .unwrap()
            .steps[0]
            .status,
        AutoStepStatus::Aborted
    );
}

#[test]
fn auto_control_rejects_unknown_run_without_mutating_other_runs() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted =
        AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
            .unwrap()
            .create_run();
    save_auto_run(&conn, &mut persisted).unwrap();

    let error = apply_auto_run_control(&conn, "missing", AutoRunControlIntent::AbortRun)
        .expect_err("missing run should fail");

    assert_eq!(error, "auto flow run not found: missing");
    let loaded = load_auto_run(&conn, &persisted.run.id).unwrap().unwrap();
    assert_eq!(loaded.run.status, AutoRunStatus::Queued);
    assert_eq!(loaded.steps[0].status, AutoStepStatus::Queued);
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
    let identity = crate::remote::test_change_request_identity();
    save_observed_change_request_identity(&conn, &persisted.run.id, Some(&identity)).unwrap();
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
    assert_eq!(
        load_observed_change_request_identity(&conn, &persisted.run.id).unwrap(),
        Some(identity)
    );
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
        change_request_identity: Some(crate::remote::test_change_request_identity()),
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
        change_request_identity: Some(crate::remote::test_change_request_identity()),
        authorized_target_branch: Some("main".to_string()),
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

#[test]
fn done_run_with_non_push_stabilization_obligation_is_active_after_restart() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted =
        AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement auto")
            .unwrap()
            .create_run();
    persisted.run.status = AutoRunStatus::Done;
    persisted.run.stabilization_status = Some(stabilization_model::StabilizationStatus::Waiting);
    persisted.run.stabilization_blocker =
        Some(stabilization_model::StabilizationBlocker::CiPending);
    persisted.run.stabilization_next_work =
        Some(stabilization_model::StabilizationWorkKind::WaitForCi);
    persisted.steps[0].status = AutoStepStatus::Done;
    save_auto_run(&conn, &mut persisted).unwrap();

    let loaded = load_auto_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("active run");

    assert_eq!(loaded.run.status, AutoRunStatus::Paused);
    assert_eq!(
        loaded.steps, persisted.steps,
        "attempt audit must be preserved"
    );
    assert_eq!(
        load_recent_active_runs_for_repo(&conn, &repo, 10).unwrap()[0]
            .run
            .id,
        persisted.run.id
    );
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
    configure_pr_observation(&temp, &mut config, "feat/auto", &remote_head);
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
        change_request_identity: Some(crate::remote::test_change_request_identity()),
        authorized_target_branch: Some("main".to_string()),
        local_head_sha: Some(remote_head.clone()),
        remote_head_sha: None,
        pr_head_sha: Some(remote_head.clone()),
        base_sha: Some(remote_head.clone()),
        review_thread_ids: Vec::new(),
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
    assert!(guard.guarded_review_thread_ids.is_empty());
    assert_eq!(
        git_output(&work, &["log", "-1", "--pretty=%s"]),
        "fix: review template"
    );
}

#[cfg(unix)]
#[test]
fn invalidated_repair_guard_replans_without_creating_a_commit() {
    let temp = TempDir::new("invalidated-review-repair-guard");
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    setup_git_worktree(&origin, &work);
    let repo = Repository::with_config_dir_for_test(work.clone(), temp.path().join("config"));
    let original_head = git_output(&work, &["rev-parse", "HEAD"]);
    seed_pr_cache(&repo, "feat/auto", &original_head);
    fs::write(work.join("tracked.txt"), "stale review fix\n").unwrap();
    let mut config = test_config();
    configure_pr_observation(&temp, &mut config, "feat/auto", &original_head);
    let mut persisted = AutoLaunch::new(&repo.root, &work, "feat/auto", "Repair")
        .unwrap()
        .create_run();
    persisted.run.pr_number = Some(42);
    persisted.steps.clear();
    persisted.steps.push(AutoStepRun::queued(
        &persisted.run.id,
        1,
        AutoStepKey::CommitReviewFix,
        1,
        Some("commit stale repair".to_string()),
    ));
    persisted.steps[0].work_guard = Some(stabilization_model::WorkGuard {
        change_request_identity: Some(crate::remote::test_change_request_identity()),
        authorized_target_branch: Some("main".to_string()),
        local_head_sha: Some(original_head.clone()),
        remote_head_sha: None,
        pr_head_sha: Some("superseded-head".to_string()),
        base_sha: Some(original_head.clone()),
        review_thread_ids: Vec::new(),
    });
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    save_auto_run(&conn, &mut persisted).unwrap();

    execute_commit_review_fix_step(&conn, &repo, &config, &mut persisted, 0, 100).unwrap();

    assert_eq!(git_output(&work, &["rev-parse", "HEAD"]), original_head);
    assert_eq!(persisted.steps[0].status, AutoStepStatus::Skipped);
    assert!(persisted.run.pending_push.is_none());
    assert!(
        !git_output(&work, &["status", "--porcelain"]).is_empty(),
        "the stale repair remains uncommitted for the replanned work"
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
    configure_pr_observation(&temp, &mut config, "feat/auto", &remote_head);
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
    persisted.steps[0].work_guard = Some(stabilization_model::WorkGuard {
        change_request_identity: Some(crate::remote::test_change_request_identity()),
        authorized_target_branch: Some("main".to_string()),
        local_head_sha: Some(remote_head.clone()),
        remote_head_sha: Some(remote_head.clone()),
        pr_head_sha: Some(remote_head.clone()),
        base_sha: Some(remote_head.clone()),
        review_thread_ids: Vec::new(),
    });
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
fn schema_migration_preserves_and_fails_old_active_auto_runs_once() {
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
           stabilization_status text,
           stabilization_blocker text,
           stabilization_next_work text,
           pending_push_json text,
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
           work_guard_json text,
           blocker text,
           summary text,
           error text,
           unique(run_id, sequence)
         );
         create table auto_output_line (
           step_run_id integer not null references auto_step_run(id) on delete cascade,
           line_number integer not null,
           time_unix_ms integer not null,
           kind text not null,
           text text not null,
           block_id text,
           primary key (step_run_id, line_number)
         );
         create table auto_event (
           id integer primary key autoincrement,
           run_id text not null references auto_run(id) on delete cascade,
           step_run_id integer references auto_step_run(id) on delete set null,
           time_unix_ms integer not null,
           kind text not null,
           data_json text not null
         );
         create table auto_schema_version (
           id integer primary key check (id = 1),
           version integer not null
         );
         insert into auto_schema_version (id, version) values (1, 3);
         insert into auto_run (
           id, repo_root, worktree_path, branch, mode, implementation_source, plan_run_mode,
           variant, prompt_summary, initial_prompt, status, created_unix_ms, updated_unix_ms
         ) values
           ('old-running', '/repo', '/repo/feature', 'feature', 'standard', 'prompt', 'sequential',
            'default', 'old running', 'old running', 'running', 1, 1),
           ('old-paused', '/repo', '/repo/paused', 'paused', 'standard', 'prompt', 'sequential',
            'default', 'old paused', 'old paused', 'paused', 2, 2),
           ('old-done', '/repo', '/repo/done', 'done', 'standard', 'prompt', 'sequential',
            'default', 'old done', 'old done', 'done', 3, 3);
         insert into auto_step_run (
           run_id, sequence, step_key, status, attempt,
           opencode_server_url, opencode_session_id, process_id
         ) values
           ('old-running', 1, 'prepare', 'done', 1, null, null, null),
           ('old-running', 2, 'merge', 'running', 1,
            'http://127.0.0.1:41000', 'ses_old', 1234),
           ('old-paused', 1, 'wait_ci', 'waiting', 1, null, null, null),
           ('old-done', 1, 'cleanup', 'done', 1, null, null, null);
         insert into auto_output_line (
           step_run_id, line_number, time_unix_ms, kind, text
         ) values (2, 1, 4, 'status', 'legacy output');
         insert into auto_event (
           run_id, step_run_id, time_unix_ms, kind, data_json
         ) values ('old-running', 2, 5, 'legacy_event', '{}');
         ",
    )
    .unwrap();
    let pending_push = stabilization_model::PendingPushGuard {
        change_request_identity: None,
        repair_kind: stabilization_model::RepairKind::Review,
        commit_sha: "repair-sha".to_string(),
        expected_local_head_sha: "repair-sha".to_string(),
        expected_remote_head_sha: Some("remote-sha".to_string()),
        pr_number: Some(42),
        expected_pr_head_sha: Some("remote-sha".to_string()),
        expected_base_sha: Some("base-sha".to_string()),
        guarded_review_thread_ids: vec!["thread-1".to_string()],
    };
    let work_guard = stabilization_model::WorkGuard {
        change_request_identity: None,
        authorized_target_branch: None,
        local_head_sha: Some("repair-sha".to_string()),
        remote_head_sha: Some("remote-sha".to_string()),
        pr_head_sha: Some("remote-sha".to_string()),
        base_sha: Some("base-sha".to_string()),
        review_thread_ids: vec!["thread-1".to_string()],
    };
    conn.execute(
        "update auto_run set pending_push_json = ?1 where id = 'old-running'",
        params![serde_json::to_string(&pending_push).unwrap()],
    )
    .unwrap();
    conn.execute(
        "update auto_step_run set work_guard_json = ?1 where id = 2",
        params![serde_json::to_string(&work_guard).unwrap()],
    )
    .unwrap();

    migrate_schema(&conn).unwrap();
    let loaded = load_auto_run(&conn, "old-running")
        .unwrap()
        .expect("running run");
    let paused = load_auto_run(&conn, "old-paused")
        .unwrap()
        .expect("paused run");
    let done = load_auto_run(&conn, "old-done").unwrap().expect("done run");

    assert_eq!(loaded.run.status, AutoRunStatus::Failed);
    assert_eq!(loaded.run.worktree_incarnation, None);
    assert_eq!(loaded.run.archived_unix_ms, None);
    assert_eq!(
        loaded.run.stabilization_status,
        Some(stabilization_model::StabilizationStatus::Escalated)
    );
    assert_eq!(
        loaded.run.stabilization_blocker,
        Some(stabilization_model::StabilizationBlocker::ObservationFailed)
    );
    assert_eq!(
        loaded.run.stabilization_next_work,
        Some(stabilization_model::StabilizationWorkKind::Escalate)
    );
    assert_eq!(loaded.steps.len(), 2);
    assert_eq!(loaded.steps[0].status, AutoStepStatus::Done);
    assert_eq!(loaded.steps[1].status, AutoStepStatus::Failed);
    assert_eq!(
        loaded.steps[1].session.endpoint.as_deref(),
        Some("http://127.0.0.1:41000")
    );
    assert_eq!(loaded.steps[1].session.id.as_deref(), Some("ses_old"));
    assert_eq!(
        loaded.steps[1].session.adapter_id.as_deref(),
        Some("opencode")
    );
    assert_eq!(loaded.steps[1].execution.process_id, Some(1234));
    assert!(
        loaded.steps[1]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("fresh remote observation"))
    );
    assert_eq!(loaded.run.pending_push.as_ref(), Some(&pending_push));
    assert_eq!(loaded.steps[1].work_guard.as_ref(), Some(&work_guard));
    assert_eq!(paused.run.status, AutoRunStatus::Failed);
    assert_eq!(paused.run.archived_unix_ms, None);
    assert_eq!(paused.steps[0].status, AutoStepStatus::Failed);
    assert_eq!(done.run.status, AutoRunStatus::Done);
    assert_eq!(done.steps[0].status, AutoStepStatus::Done);
    assert_eq!(
        load_observed_change_request_identity(&conn, "old-running").unwrap(),
        None
    );
    assert!(
        load_recent_active_runs_for_repo(&conn, Path::new("/repo"), 10)
            .unwrap()
            .iter()
            .any(|run| run.run.id == "old-running")
    );
    assert_eq!(
        load_output_lines(&conn, 2).unwrap()[0].text,
        "legacy output"
    );
    assert_eq!(
        conn.query_row("select count(*) from auto_event", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let identity = crate::remote::test_change_request_identity();
    assert!(matches!(
        stabilization_execute::decide_guarded_push(
            loaded.run.pending_push.as_ref().unwrap(),
            Some(&identity),
            Some("repair-sha"),
            Some("remote-sha"),
            Some(42),
            Some("remote-sha"),
            Some("base-sha"),
        ),
        stabilization_execute::GuardedPushDecision::Invalidated { reason }
            if reason.contains("no canonical change request identity")
    ));
    assert!(matches!(
        stabilization_execute::decide_work_guard(
            &stabilization_model::RepairKind::Merge,
            loaded.steps[1].work_guard.as_ref().unwrap(),
            &stabilization_model::WorkGuard {
                change_request_identity: Some(identity.clone()),
                ..work_guard.clone()
            },
        ),
        stabilization_execute::WorkGuardDecision::Invalidated { reason }
            if reason.contains("no canonical change request identity")
    ));

    let first_migration = loaded.clone();
    let identity = crate::remote::test_change_request_identity();
    save_observed_change_request_identity(&conn, "old-running", Some(&identity)).unwrap();

    migrate_schema(&conn).unwrap();
    let loaded = load_auto_run(&conn, "old-running")
        .unwrap()
        .expect("running run");
    assert_eq!(loaded, first_migration);
    assert_eq!(
        load_observed_change_request_identity(&conn, "old-running").unwrap(),
        Some(identity)
    );
    assert_eq!(
        conn.query_row(
            "select version from auto_schema_version where id = 1",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        8
    );
}

#[test]
fn malformed_auto_run_identity_fails_closed_without_dropping_the_row() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Implement")
        .unwrap()
        .create_run();
    save_auto_run(&conn, &mut persisted).unwrap();
    conn.execute(
        "update auto_run set change_request_identity_json = '{not-json' where id = ?1",
        params![persisted.run.id],
    )
    .unwrap();

    let error = load_observed_change_request_identity(&conn, &persisted.run.id).unwrap_err();

    assert!(error.contains("parse auto change request identity"));
    assert!(load_auto_run(&conn, &persisted.run.id).unwrap().is_some());
}

#[test]
fn future_auto_schema_version_fails_without_changing_rows() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "create table auto_schema_version (
           id integer primary key check (id = 1),
           version integer not null
         );
         insert into auto_schema_version (id, version) values (1, 9);
         create table auto_run (id text primary key);
         insert into auto_run (id) values ('future');",
    )
    .unwrap();

    let error = migrate_schema(&conn).unwrap_err();

    assert!(error.contains("newer than supported"));
    assert_eq!(
        conn.query_row("select count(*) from auto_run", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "select version from auto_schema_version where id = 1",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        9
    );
}

#[test]
fn schema_round_trips_auto_implementation_sources() {
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
    let mut existing_pull_request = AutoLaunch::with_options(
        &repo,
        &repo.join("existing-pr"),
        AutoLaunchOptions {
            branch: "feat/existing-pr".to_string(),
            mode: AutoRunMode::Standard,
            implementation_source: AutoImplementationSource::ExistingPullRequest,
            plan_path: None,
            plan_run_mode: PlanRunMode::Sequential,
            variant: "existing-pr".to_string(),
            agent_profile: None,
            initial_prompt: "Stabilize existing pull request".to_string(),
        },
    )
    .unwrap()
    .create_run();

    save_auto_run(&conn, &mut prompt).unwrap();
    save_auto_run(&conn, &mut existing_plan).unwrap();
    save_auto_run(&conn, &mut draft_plan).unwrap();
    save_auto_run(&conn, &mut existing_pull_request).unwrap();

    let prompt = load_auto_run(&conn, &prompt.run.id).unwrap().unwrap();
    let existing_plan = load_auto_run(&conn, &existing_plan.run.id)
        .unwrap()
        .unwrap();
    let draft_plan = load_auto_run(&conn, &draft_plan.run.id).unwrap().unwrap();
    let existing_pull_request = load_auto_run(&conn, &existing_pull_request.run.id)
        .unwrap()
        .unwrap();

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
    assert_eq!(
        existing_pull_request.run.implementation_source,
        AutoImplementationSource::ExistingPullRequest
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

    let outcome =
        apply_auto_run_control(&conn, &persisted.run.id, AutoRunControlIntent::Resume).unwrap();
    persisted = outcome.run;
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
fn stale_reconciliation_preserves_submitted_merge_for_observation() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = AutoLaunch::new(
        &repo,
        &repo.join("feature"),
        "feat/auto",
        "Integrate pull request",
    )
    .unwrap()
    .create_run();
    persisted.steps.clear();
    push_test_step(
        &mut persisted,
        1,
        AutoStepKey::Merge,
        AutoStepStatus::Running,
    );
    persisted.run.status = AutoRunStatus::Running;
    save_auto_run(&conn, &mut persisted).unwrap();
    crate::integration::arm_merge_intent(&conn, &persisted.run.id).unwrap();
    crate::integration::synchronize_generation(
        &conn,
        &persisted.run.id,
        &crate::integration::CandidateGeneration {
            change_request_identity: crate::remote::test_change_request_identity(),
            target_branch: "main".to_string(),
            pr_number: 42,
            head_sha: "head".to_string(),
        },
    )
    .unwrap();
    crate::integration::publish_ready(&conn, &persisted.run.id, "head").unwrap();
    crate::integration::mark_submitting(&conn, &persisted.run.id).unwrap();
    crate::integration::mark_submitted(&conn, &persisted.run.id).unwrap();

    assert!(reconcile_stale_auto_run(&conn, &mut persisted).unwrap());

    assert_eq!(persisted.steps[0].status, AutoStepStatus::Waiting);
    assert_eq!(
        crate::integration::active_merge_intent(&conn, &persisted.run.id)
            .unwrap()
            .unwrap()
            .placement,
        crate::integration::IntegrationPlacement::Submitted
    );
}

#[test]
fn stale_reconciliation_requeues_updating_branch_for_effect_reconciliation() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = AutoLaunch::new(
        &repo,
        &repo.join("feature"),
        "feat/auto",
        "Update pull request",
    )
    .unwrap()
    .create_run();
    persisted.steps.clear();
    push_test_step(
        &mut persisted,
        1,
        AutoStepKey::UpdateBranch,
        AutoStepStatus::Running,
    );
    persisted.run.status = AutoRunStatus::Running;
    save_auto_run(&conn, &mut persisted).unwrap();
    crate::integration::arm_merge_intent(&conn, &persisted.run.id).unwrap();
    crate::integration::synchronize_generation(
        &conn,
        &persisted.run.id,
        &crate::integration::CandidateGeneration {
            change_request_identity: crate::remote::test_change_request_identity(),
            target_branch: "main".to_string(),
            pr_number: 42,
            head_sha: "head".to_string(),
        },
    )
    .unwrap();
    crate::integration::publish_ready(&conn, &persisted.run.id, "head").unwrap();
    crate::integration::mark_updating(&conn, &persisted.run.id).unwrap();

    assert!(reconcile_stale_auto_run(&conn, &mut persisted).unwrap());

    assert_eq!(persisted.steps[0].status, AutoStepStatus::Queued);
    assert_eq!(
        crate::integration::active_merge_intent(&conn, &persisted.run.id)
            .unwrap()
            .unwrap()
            .placement,
        crate::integration::IntegrationPlacement::Updating
    );
}

#[test]
fn recent_active_runs_exclude_terminal_repair_history() {
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
    let mut repair = AutoLaunch::new(&repo, &repo.join("feature-r"), "feat/r", "Repair r")
        .unwrap()
        .create_run();
    repair.run.variant = "repair".to_string();
    repair.run.status = AutoRunStatus::Aborted;
    let mut archived = AutoLaunch::new(&repo, &repo.join("feature-c"), "feat/c", "Implement c")
        .unwrap()
        .create_run();
    archived.run.status = AutoRunStatus::Failed;
    save_auto_run(&conn, &mut active).unwrap();
    save_auto_run(&conn, &mut done).unwrap();
    save_auto_run(&conn, &mut repair).unwrap();
    save_auto_run(&conn, &mut archived).unwrap();
    archive_auto_run(&conn, &mut archived).unwrap();

    let recent = load_recent_active_runs_for_repo(&conn, &repo, 10).unwrap();

    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].run.id, active.run.id);

    let history = load_terminal_repair_run_snapshots_for_repo(&conn, &repo).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].run.id, repair.run.id);
}

#[test]
fn phase_1_standalone_review_repair_never_queues_implementation_after_fix() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let mut persisted = standalone_completed_repair(&conn, AutoStepKey::FixReview);

    let repo = Repository::with_config_dir_for_test(
        PathBuf::from("/repo/prism"),
        PathBuf::from("/tmp/prism-phase-1-review-repair-config"),
    );
    ensure_next_auto_step_with_context(&conn, &repo, &test_config(), &mut persisted).unwrap();

    assert!(
        !persisted
            .steps
            .iter()
            .any(|step| matches!(step.step_key, AutoStepKey::Implement | AutoStepKey::RunPlan))
    );
}

#[test]
fn phase_1_standalone_ci_repair_never_queues_implementation_after_fix() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let mut persisted = standalone_completed_repair(&conn, AutoStepKey::FixCi);

    let repo = Repository::with_config_dir_for_test(
        PathBuf::from("/repo/prism"),
        PathBuf::from("/tmp/prism-phase-1-ci-repair-config"),
    );
    ensure_next_auto_step_with_context(&conn, &repo, &test_config(), &mut persisted).unwrap();

    assert!(
        !persisted
            .steps
            .iter()
            .any(|step| matches!(step.step_key, AutoStepKey::Implement | AutoStepKey::RunPlan))
    );
}

#[test]
fn phase_1_done_run_with_pending_push_is_discoverable_after_restart() {
    let temp = TempDir::new("phase-1-pending-push-restart");
    let database = temp.path().join("prism.db");
    let repo = temp.path().join("repo");
    let run_id = {
        let conn = rusqlite::Connection::open(&database).unwrap();
        migrate_schema(&conn).unwrap();
        let mut persisted = AutoLaunch::new(&repo, &repo, "feat/auto", "Repair review")
            .unwrap()
            .create_run();
        persisted.run.status = AutoRunStatus::Done;
        persisted.run.pending_push = Some(stabilization_model::PendingPushGuard {
            change_request_identity: Some(crate::remote::test_change_request_identity()),
            repair_kind: stabilization_model::RepairKind::Review,
            commit_sha: "repair-sha".to_string(),
            expected_local_head_sha: "repair-sha".to_string(),
            expected_remote_head_sha: Some("remote-sha".to_string()),
            pr_number: Some(42),
            expected_pr_head_sha: Some("remote-sha".to_string()),
            expected_base_sha: Some("base-sha".to_string()),
            guarded_review_thread_ids: vec!["thread-1".to_string()],
        });
        save_auto_run(&conn, &mut persisted).unwrap();
        persisted.run.id
    };

    let conn = rusqlite::Connection::open(&database).unwrap();
    migrate_schema(&conn).unwrap();
    let recent = load_recent_active_runs_for_repo(&conn, &repo, 10).unwrap();

    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].run.id, run_id);
    assert!(recent[0].run.pending_push.is_some());
    assert_ne!(recent[0].run.status, AutoRunStatus::Done);
}

#[test]
#[cfg(unix)]
fn restart_after_unrelated_commit_does_not_adopt_it_as_the_repair_commit() {
    let temp = TempDir::new("precommit-obligation-restart");
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    setup_git_worktree(&origin, &work);
    let repo = Repository::with_config_dir_for_test(work.clone(), temp.path().join("config"));
    let head = git_output(&work, &["rev-parse", "HEAD"]);
    seed_pr_cache(&repo, "feat/auto", &head);
    let database = temp.path().join("auto.db");
    let run_id = {
        let conn = rusqlite::Connection::open(&database).unwrap();
        migrate_schema(&conn).unwrap();
        let mut persisted = AutoLaunch::new(&repo.root, &work, "feat/auto", "Repair")
            .unwrap()
            .create_run();
        persisted.run.pending_push = Some(stabilization_model::PendingPushGuard {
            change_request_identity: Some(crate::remote::test_change_request_identity()),
            repair_kind: stabilization_model::RepairKind::Review,
            commit_sha: String::new(),
            expected_local_head_sha: head.clone(),
            expected_remote_head_sha: None,
            pr_number: Some(42),
            expected_pr_head_sha: Some(head.clone()),
            expected_base_sha: Some(head.clone()),
            guarded_review_thread_ids: vec!["thread-1".to_string()],
        });
        save_auto_run(&conn, &mut persisted).unwrap();
        persisted.run.id
    };
    fs::write(work.join("unrelated.txt"), "user work\n").unwrap();
    run_git(&work, &["add", "unrelated.txt"]);
    run_git(&work, &["commit", "-m", "unrelated user commit"]);
    let unrelated_head = git_output(&work, &["rev-parse", "HEAD"]);
    let conn = rusqlite::Connection::open(&database).unwrap();
    migrate_schema(&conn).unwrap();
    let mut reopened = load_auto_run(&conn, &run_id).unwrap().unwrap();
    let mut cache = crate::remote::load_pr_cache(&repo, "feat/auto");

    let mut config = test_config();
    crate::test_support::use_real_tool(&mut config, "git");
    let progress = stabilization_execute::progress_pending_push(
        &conn,
        &repo,
        &config,
        &mut reopened,
        &mut cache,
        || panic!("a precommit placeholder must never authorize a push"),
    )
    .unwrap();

    assert!(matches!(
        progress,
        stabilization_execute::GuardedPushProgress::Invalidated { .. }
    ));
    assert!(reopened.run.pending_push.is_none());
    assert!(
        load_auto_run(&conn, &run_id)
            .unwrap()
            .unwrap()
            .run
            .pending_push
            .is_none()
    );
    assert_eq!(git_output(&work, &["rev-parse", "HEAD"]), unrelated_head);
}

#[test]
#[cfg(unix)]
fn transient_base_lookup_failure_retains_pending_push_for_retry() {
    let temp = TempDir::new("pending-push-base-retry");
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    setup_git_worktree(&origin, &work);
    run_git(&work, &["push", "-u", "origin", "feat/auto"]);
    let remote_head = git_output(&work, &["rev-parse", "origin/feat/auto"]);
    fs::write(work.join("repair.txt"), "repair\n").unwrap();
    run_git(&work, &["add", "repair.txt"]);
    run_git(&work, &["commit", "-m", "repair"]);
    let repair_head = git_output(&work, &["rev-parse", "HEAD"]);
    let repo = Repository::with_config_dir_for_test(work.clone(), temp.path().join("config"));
    seed_pr_cache(&repo, "feat/auto", &remote_head);
    let mut config = test_config();
    configure_pr_observation(&temp, &mut config, "feat/auto", &remote_head);
    let marker = temp.path().join("base-lookup-failed");
    let pre_push_marker = temp.path().join("pre-push-ran");
    config.checks.pre_push = vec![format!("touch {}", pre_push_marker.display())];
    let git = temp.path().join("git");
    write_executable(
        &git,
        &format!(
            "#!/bin/sh\ncase \"$*\" in\n  *\"rev-parse --verify --quiet refs/remotes/origin/main\"*)\n    if [ ! -e '{}' ]; then touch '{}'; printf 'transient lookup failure\\n' >&2; exit 128; fi\n    ;;\n  *\"remote get-url\"*) printf 'https://github.com/example/repo.git\\n'; exit 0 ;;\n  *\"ls-remote --exit-code --heads https://github.com/example/repo.git refs/heads/feat/auto\"*) printf '%s\\t%s\\n' '{}' 'refs/heads/feat/auto'; exit 0 ;;\nesac\nexec git \"$@\"\n",
            marker.display(),
            marker.display(),
            remote_head
        ),
    );
    let mut persisted = AutoLaunch::new(&repo.root, &work, "feat/auto", "Repair")
        .unwrap()
        .create_run();
    persisted.run.pending_push = Some(stabilization_model::PendingPushGuard {
        change_request_identity: Some(crate::remote::test_change_request_identity()),
        repair_kind: stabilization_model::RepairKind::Ci,
        commit_sha: repair_head.clone(),
        expected_local_head_sha: repair_head,
        expected_remote_head_sha: Some(remote_head.clone()),
        pr_number: Some(42),
        expected_pr_head_sha: Some(remote_head.clone()),
        expected_base_sha: Some(remote_head),
        guarded_review_thread_ids: Vec::new(),
    });
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    save_auto_run(&conn, &mut persisted).unwrap();
    let mut cache = crate::remote::load_pr_cache(&repo, "feat/auto");

    let first_error = stabilization_execute::progress_pending_push(
        &conn,
        &repo,
        &config,
        &mut persisted,
        &mut cache,
        || panic!("lookup failure must stop before push"),
    )
    .unwrap_err();

    assert!(first_error.contains("transient lookup failure"));
    assert!(persisted.run.pending_push.is_some());
    assert!(
        load_auto_run(&conn, &persisted.run.id)
            .unwrap()
            .unwrap()
            .run
            .pending_push
            .is_some()
    );

    let retried_push = std::cell::Cell::new(false);
    let retry_error = stabilization_execute::progress_pending_push(
        &conn,
        &repo,
        &config,
        &mut persisted,
        &mut cache,
        || {
            retried_push.set(true);
            Err("stop test before push".to_string())
        },
    )
    .unwrap_err();

    assert_eq!(retry_error, "stop test before push");
    assert!(retried_push.get());
    assert!(pre_push_marker.exists());
    assert!(persisted.run.pending_push.is_some());
}

#[test]
fn initial_change_request_push_runs_pre_pr_then_pre_push_checks() {
    let temp = TempDir::new("initial-push-checks");
    let pre_pr = temp.path().join("pre-pr-ran");
    let pre_push = temp.path().join("pre-push-ran");
    let mut config = test_config();
    config.checks.pre_pr = vec![format!("touch {}", pre_pr.display())];
    config.checks.pre_push = vec![format!("touch {}", pre_push.display())];

    non_agent::run_initial_push_checks(&config, temp.path(), true).unwrap();

    assert!(pre_pr.exists());
    assert!(pre_push.exists());
}

#[test]
fn reserved_base_update_rejects_mutated_pre_push_state() {
    let repository = crate::remote::test_change_request_identity()
        .source_repository()
        .unwrap();
    let expected = crate::remote::dispatcher::PushGuard {
        repository,
        remote: "origin".to_string(),
        remote_branch: "feat/auto".to_string(),
        local_branch: "feat/auto".to_string(),
        expected_head_sha: "merged-head".to_string(),
        set_upstream: false,
    };

    non_agent::validate_base_update_push_guard(&expected, &expected, "merged-head", false).unwrap();

    let mut changed_head = expected.clone();
    changed_head.expected_head_sha = "check-commit".to_string();
    assert!(
        non_agent::validate_base_update_push_guard(&expected, &changed_head, "merged-head", false,)
            .unwrap_err()
            .contains("push guard changed")
    );

    let mut changed_destination = expected.clone();
    changed_destination.remote_branch = "other".to_string();
    assert!(
        non_agent::validate_base_update_push_guard(
            &expected,
            &changed_destination,
            "merged-head",
            false,
        )
        .unwrap_err()
        .contains("push guard changed")
    );
    assert!(
        non_agent::validate_base_update_push_guard(&expected, &expected, "merged-head", true,)
            .unwrap_err()
            .contains("became dirty")
    );
}

#[test]
#[cfg(unix)]
fn executor_runs_fake_opencode_pauses_before_next_step_and_persists_events() {
    let temp = TempDir::new("executor-success");
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    setup_git_worktree(&origin, &work);
    run_git(&work, &["push", "-u", "origin", "feat/auto"]);
    let repo = Repository::with_config_dir_for_test(work.clone(), temp.path().join("prism-config"));
    let mut config = Config::load(&repo);
    config.default_base = None;
    let head = crate::git::current_head_sha(&work, &config).unwrap();
    seed_pr_cache(&repo, "feat/auto", &head);
    configure_pr_observation(&temp, &mut config, "feat/auto", &head);
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
    assert_eq!(implement.session.id.as_deref(), Some("ses_auto"));
    assert_eq!(implement.summary.as_deref(), Some("working on it"));
    let verify = loaded
        .steps
        .iter()
        .find(|step| step.step_key == AutoStepKey::LocalVerify)
        .unwrap();
    assert_eq!(verify.status, AutoStepStatus::Done);
    assert!(loaded.steps.iter().any(|step| {
        step.step_key == AutoStepKey::CommitImpl
            && matches!(step.status, AutoStepStatus::Done | AutoStepStatus::Skipped)
    }));
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
#[cfg(unix)]
fn generic_headless_harness_executes_auto_flow_with_plain_text() {
    let temp = TempDir::new("executor-generic");
    let repo = Repository {
        root: temp.path().to_path_buf(),
    };
    let config = Config::load(&repo);
    let agent = temp.path().join("generic-agent");
    write_executable(
        &agent,
        r#"#!/bin/sh
printf 'generic:%s\n' "$1"
"#,
    );
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let mut persisted = AutoLaunch::new(temp.path(), temp.path(), "feat/auto", "Implement auto")
        .unwrap()
        .with_harness("generic-test", "generic")
        .create_run();
    persisted.steps[0].step_key = AutoStepKey::Implement;
    save_auto_run(&conn, &mut persisted).unwrap();
    let harness_config = crate::harness::HarnessConfig {
        adapter: "generic".to_string(),
        interactive_command: vec![agent.display().to_string()],
        arguments: Vec::new(),
        interactive_prompt_transport: None,
        headless_command: Some(vec![agent.display().to_string(), "{prompt}".to_string()]),
        headless_prompt_transport: Some(crate::harness::PromptTransport::Argument),
        output_format: crate::harness::OutputFormat::Text,
        environment: std::collections::BTreeMap::new(),
    };
    let executor =
        AutoExecutorConfig::for_harness("generic-test", harness_config, None, temp.path(), "Auto");

    execute_one_agent_step(
        &conn,
        &config,
        &mut persisted,
        0,
        &executor,
        &mut Vec::new(),
    )
    .unwrap();

    let loaded = load_auto_run(&conn, &persisted.run.id).unwrap().unwrap();
    let implement = loaded
        .steps
        .iter()
        .find(|step| step.step_key == AutoStepKey::Implement)
        .unwrap();
    assert_eq!(implement.status, AutoStepStatus::Done);
    let output = load_output_lines(&conn, implement.id.unwrap()).unwrap();
    assert!(output.iter().any(|line| line.text.starts_with("generic:")));
}

#[test]
fn unsupported_generic_headless_auto_step_fails_instead_of_remaining_starting() {
    let temp = TempDir::new("executor-interactive-only");
    let repo = Repository {
        root: temp.path().to_path_buf(),
    };
    let config = Config::load(&repo);
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let mut persisted = AutoLaunch::new(temp.path(), temp.path(), "feat/auto", "Implement auto")
        .unwrap()
        .with_harness("interactive-only", "generic")
        .create_run();
    persisted.steps[0].step_key = AutoStepKey::Implement;
    save_auto_run(&conn, &mut persisted).unwrap();
    let executor = AutoExecutorConfig::for_harness(
        "interactive-only",
        crate::harness::HarnessConfig {
            adapter: "generic".to_string(),
            interactive_command: vec!["agent".to_string()],
            arguments: Vec::new(),
            interactive_prompt_transport: None,
            headless_command: None,
            headless_prompt_transport: None,
            output_format: crate::harness::OutputFormat::Text,
            environment: std::collections::BTreeMap::new(),
        },
        None,
        temp.path(),
        "Auto",
    );

    execute_one_agent_step(
        &conn,
        &config,
        &mut persisted,
        0,
        &executor,
        &mut Vec::new(),
    )
    .unwrap_err();

    let loaded = load_auto_run(&conn, &persisted.run.id).unwrap().unwrap();
    assert_eq!(loaded.steps[0].status, AutoStepStatus::Failed);
    assert!(
        loaded.steps[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("does not support managed headless execution")
    );
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
    let details = crate::remote::PrDetails {
        comments: vec![crate::remote::PrComment {
            id: "comment-1".to_string(),
            author: "github-copilot".to_string(),
            body: "Please simplify this branch.".to_string(),
            created_at: "2026-01-01T00:01:00Z".to_string(),
        }],
        ..crate::remote::PrDetails::default()
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
    let mut config = Config::load(&repo);
    config.auto.review_requirement = crate::config::ReviewRequirement::Approved;
    let details = crate::remote::PrDetails {
        comments: vec![crate::remote::PrComment {
            id: "comment-1".to_string(),
            author: "github-copilot".to_string(),
            body: "Already handled.".to_string(),
            created_at: "2026-01-01T00:05:00Z".to_string(),
        }],
        ..crate::remote::PrDetails::default()
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
fn review_poll_keeps_old_unresolved_threads_actionable() {
    let temp = TempDir::new("review-poll-old-thread");
    let repo = Repository {
        root: temp.path().to_path_buf(),
    };
    let summary = test_pr_summary("feat/auto", "abc123", "2026-01-01T00:05:00Z");
    let mut config = Config::load(&repo);
    config.auto.review_requirement = crate::config::ReviewRequirement::Approved;
    let details = crate::remote::PrDetails {
        review_comments: vec![crate::remote::PrReviewComment {
            thread_id: "thread-old".to_string(),
            body: "still unresolved".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            resolved: false,
            ..crate::remote::PrReviewComment::default()
        }],
        ..crate::remote::PrDetails::default()
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

    assert!(outcome.fix_prompt.is_some());
    assert_eq!(outcome.review_thread_ids, vec!["thread-old".to_string()]);
}

#[test]
fn review_poll_resolved_requirement_waits_without_review_comments() {
    let temp = TempDir::new("review-poll-resolved-missing");
    let repo = Repository {
        root: temp.path().to_path_buf(),
    };
    let config = Config::load(&repo);
    let summary = test_pr_summary("feat/auto", "abc123", "2026-01-01T00:00:00Z");
    let mut persisted = AutoLaunch::new(temp.path(), temp.path(), "feat/auto", "Implement auto")
        .unwrap()
        .create_run();

    let outcome = evaluate_review_feedback(
        &config,
        &mut persisted,
        &summary,
        Some(&crate::remote::PrDetails::default()),
    )
    .unwrap();

    assert!(!outcome.complete);
    assert_eq!(outcome.summary, "no review comments found yet");
}

#[test]
fn review_poll_resolved_requirement_completes_with_a_resolved_comment() {
    let temp = TempDir::new("review-poll-resolved-complete");
    let repo = Repository {
        root: temp.path().to_path_buf(),
    };
    let config = Config::load(&repo);
    let summary = test_pr_summary("feat/auto", "abc123", "2026-01-01T00:00:00Z");
    let details = crate::remote::PrDetails {
        review_comments: vec![crate::remote::PrReviewComment {
            body: "handled feedback".to_string(),
            resolved: true,
            ..crate::remote::PrReviewComment::default()
        }],
        ..crate::remote::PrDetails::default()
    };
    let mut persisted = AutoLaunch::new(temp.path(), temp.path(), "feat/auto", "Implement auto")
        .unwrap()
        .create_run();

    let outcome =
        evaluate_review_feedback(&config, &mut persisted, &summary, Some(&details)).unwrap();

    assert!(outcome.complete);
    assert_eq!(outcome.summary, "all 1 review comment(s) are resolved");
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
        ci_failures: vec![crate::remote::CachedCiFailure {
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

    assert!(ensure_next_test_step(&conn, &mut persisted).unwrap());

    assert!(
        persisted
            .steps
            .iter()
            .any(|step| step.step_key == AutoStepKey::Cleanup)
    );
    assert_ne!(persisted.run.status, AutoRunStatus::Done);
}

#[test]
fn merged_run_finishes_cleanup_without_another_pause() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let repo = Repository {
        root: PathBuf::from("/repo/prism"),
    };
    let mut persisted = AutoLaunch::new(
        &repo.root,
        &repo.root.join("feature"),
        "feat/auto",
        "Implement auto",
    )
    .unwrap()
    .create_run();
    persisted.run.implementation_source = AutoImplementationSource::ExistingPullRequest;
    persisted.run.stabilization_status = Some(stabilization_model::StabilizationStatus::Done);
    persisted.steps.clear();
    push_test_step(&mut persisted, 1, AutoStepKey::Merge, AutoStepStatus::Done);
    push_test_step(
        &mut persisted,
        2,
        AutoStepKey::Cleanup,
        AutoStepStatus::Queued,
    );
    save_auto_run(&conn, &mut persisted).unwrap();

    pause_before_next_auto_step_with_context(&conn, &repo, &test_config(), &mut persisted).unwrap();

    assert!(!persisted.run.pause_requested);
    assert_ne!(persisted.run.status, AutoRunStatus::Paused);

    let executor =
        AutoExecutorConfig::new("unused", None, persisted.run.worktree_path.clone(), "Auto");
    execute_auto_initial_step(
        &conn,
        &repo,
        &test_config(),
        &mut persisted,
        &executor,
        &mut Vec::new(),
    )
    .unwrap();

    assert_eq!(persisted.run.status, AutoRunStatus::Done);
    assert_eq!(persisted.steps[1].status, AutoStepStatus::Skipped);
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

    assert!(!ensure_next_test_step(&conn, &mut persisted).unwrap());

    assert_eq!(persisted.run.status, AutoRunStatus::Done);
    assert!(
        !persisted
            .steps
            .iter()
            .any(|step| step.step_key == AutoStepKey::Cleanup)
    );
}

#[test]
fn reserved_integration_runs_merge_without_another_pause() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    crate::execution::migrate_schema(&conn).unwrap();
    let repo = Repository {
        root: PathBuf::from("/repo/prism"),
    };
    let mut persisted = AutoLaunch::new(
        &repo.root,
        &repo.root.join("feature"),
        "feature",
        "Integrate",
    )
    .unwrap()
    .create_run();
    persisted.steps.clear();
    push_test_step(
        &mut persisted,
        1,
        AutoStepKey::Merge,
        AutoStepStatus::Queued,
    );
    save_auto_run(&conn, &mut persisted).unwrap();
    crate::integration::arm_merge_intent(&conn, &persisted.run.id).unwrap();
    crate::integration::synchronize_generation(
        &conn,
        &persisted.run.id,
        &crate::integration::CandidateGeneration {
            change_request_identity: crate::remote::test_change_request_identity(),
            target_branch: "main".to_string(),
            pr_number: 42,
            head_sha: "head".to_string(),
        },
    )
    .unwrap();
    assert_eq!(
        crate::integration::publish_ready(&conn, &persisted.run.id, "head").unwrap(),
        crate::integration::IntegrationPlacement::Reserved
    );

    pause_before_next_auto_step_with_context(&conn, &repo, &test_config(), &mut persisted).unwrap();

    assert!(!persisted.run.pause_requested);
    assert_ne!(persisted.run.status, AutoRunStatus::Paused);
}

#[test]
fn waiting_merge_reconciliation_keeps_pending_without_resubmitting() {
    let temp = TempDir::new("merge-reconcile-pending");
    let repo = Repository::with_config_dir_for_test(
        temp.path().to_path_buf(),
        temp.path().join("prism-config"),
    );
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    crate::execution::migrate_schema(&conn).unwrap();
    let mut persisted = waiting_merge_run(&conn, temp.path());
    crate::integration::arm_merge_intent(&conn, &persisted.run.id).unwrap();
    crate::integration::synchronize_generation(
        &conn,
        &persisted.run.id,
        &crate::integration::CandidateGeneration {
            change_request_identity: crate::remote::test_change_request_identity(),
            target_branch: "main".to_string(),
            pr_number: 42,
            head_sha: "head".to_string(),
        },
    )
    .unwrap();
    crate::integration::publish_ready(&conn, &persisted.run.id, "head").unwrap();
    crate::integration::mark_submitting(&conn, &persisted.run.id).unwrap();
    crate::integration::mark_submitted(&conn, &persisted.run.id).unwrap();
    let observations = std::cell::Cell::new(0);

    for queue_state in [
        crate::remote::QueueState::Queued,
        crate::remote::QueueState::Running,
    ] {
        let progress =
            reconcile_waiting_merge_step_with(&conn, &repo, &mut persisted, 0, 100, |expected| {
                observations.set(observations.get() + 1);
                Ok(waiting_merge_observation(
                    expected,
                    crate::remote::LifecycleState::Open,
                    queue_state,
                ))
            })
            .unwrap();

        assert_eq!(progress, MergeReconciliationProgress::Waiting);
        assert_eq!(persisted.steps[0].status, AutoStepStatus::Waiting);
        assert_eq!(persisted.steps[0].attempt, 1);
    }

    assert_eq!(observations.get(), 2);
    assert!(
        persisted.steps[0]
            .summary
            .as_deref()
            .is_some_and(|summary| summary.contains("still pending"))
    );
}

#[test]
fn waiting_merge_observation_failure_keeps_submitted_lane_reserved() {
    let temp = TempDir::new("merge-reconcile-observation-failure");
    let repo = Repository::with_config_dir_for_test(
        temp.path().to_path_buf(),
        temp.path().join("prism-config"),
    );
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    crate::execution::migrate_schema(&conn).unwrap();
    let mut persisted = waiting_merge_run(&conn, temp.path());
    crate::integration::arm_merge_intent(&conn, &persisted.run.id).unwrap();
    crate::integration::synchronize_generation(
        &conn,
        &persisted.run.id,
        &crate::integration::CandidateGeneration {
            change_request_identity: crate::remote::test_change_request_identity(),
            target_branch: "main".to_string(),
            pr_number: 42,
            head_sha: "head".to_string(),
        },
    )
    .unwrap();
    crate::integration::publish_ready(&conn, &persisted.run.id, "head").unwrap();
    crate::integration::mark_submitting(&conn, &persisted.run.id).unwrap();
    crate::integration::mark_submitted(&conn, &persisted.run.id).unwrap();

    let progress = reconcile_waiting_merge_step_with(&conn, &repo, &mut persisted, 0, 100, |_| {
        Err("provider timeout".to_string())
    })
    .unwrap();

    assert_eq!(progress, MergeReconciliationProgress::Waiting);
    assert_eq!(persisted.steps[0].status, AutoStepStatus::Waiting);
    assert_eq!(
        crate::integration::active_merge_intent(&conn, &persisted.run.id)
            .unwrap()
            .unwrap()
            .placement,
        crate::integration::IntegrationPlacement::Submitted
    );
}

#[test]
fn interrupted_unobserved_submission_is_rearmed_for_guarded_retry() {
    let temp = TempDir::new("merge-reconcile-unobserved-submission");
    let repo = Repository::with_config_dir_for_test(
        temp.path().to_path_buf(),
        temp.path().join("prism-config"),
    );
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    crate::execution::migrate_schema(&conn).unwrap();
    let mut persisted = waiting_merge_run(&conn, temp.path());
    crate::integration::arm_merge_intent(&conn, &persisted.run.id).unwrap();
    crate::integration::synchronize_generation(
        &conn,
        &persisted.run.id,
        &crate::integration::CandidateGeneration {
            change_request_identity: crate::remote::test_change_request_identity(),
            target_branch: "main".to_string(),
            pr_number: 42,
            head_sha: "head".to_string(),
        },
    )
    .unwrap();
    crate::integration::publish_ready(&conn, &persisted.run.id, "head").unwrap();
    crate::integration::mark_submitting(&conn, &persisted.run.id).unwrap();

    let progress =
        reconcile_waiting_merge_step_with(&conn, &repo, &mut persisted, 0, 100, |expected| {
            Ok(waiting_merge_observation(
                expected,
                crate::remote::LifecycleState::Open,
                crate::remote::QueueState::NotQueued,
            ))
        })
        .unwrap();

    assert_eq!(progress, MergeReconciliationProgress::RetrySubmission);
    assert_eq!(persisted.steps[0].status, AutoStepStatus::Queued);
    assert_eq!(
        crate::integration::active_merge_intent(&conn, &persisted.run.id)
            .unwrap()
            .unwrap()
            .placement,
        crate::integration::IntegrationPlacement::Reserved
    );
}

#[test]
fn waiting_merge_reconciliation_completes_and_queues_cleanup_when_merged() {
    let temp = TempDir::new("merge-reconcile-merged");
    let repo = Repository::with_config_dir_for_test(
        temp.path().to_path_buf(),
        temp.path().join("prism-config"),
    );
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let mut persisted = waiting_merge_run(&conn, temp.path());

    let progress =
        reconcile_waiting_merge_step_with(&conn, &repo, &mut persisted, 0, 100, |expected| {
            Ok(waiting_merge_observation(
                expected,
                crate::remote::LifecycleState::Merged,
                crate::remote::QueueState::Complete,
            ))
        })
        .unwrap();

    assert_eq!(progress, MergeReconciliationProgress::Done);
    assert_eq!(persisted.steps[0].status, AutoStepStatus::Done);
    assert_eq!(
        persisted.run.stabilization_status,
        Some(stabilization_model::StabilizationStatus::Done)
    );
    assert!(
        ensure_next_auto_step_with_context(&conn, &repo, &test_config(), &mut persisted).unwrap()
    );
    assert_eq!(persisted.steps[1].step_key, AutoStepKey::Cleanup);
}

#[test]
fn waiting_merge_reconciliation_escalates_stale_identity_without_observing() {
    let temp = TempDir::new("merge-reconcile-stale-identity");
    let repo = Repository::with_config_dir_for_test(
        temp.path().to_path_buf(),
        temp.path().join("prism-config"),
    );
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let mut persisted = waiting_merge_run(&conn, temp.path());
    let changed_repository = crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.com", None).unwrap(),
        "example/other",
    )
    .unwrap();
    let changed_identity = crate::remote::CanonicalChangeRequestIdentity::new(
        &changed_repository,
        &crate::remote::NativeChangeRequestId::new("PR_other").unwrap(),
        &changed_repository,
        &changed_repository,
    );
    save_observed_change_request_identity(&conn, &persisted.run.id, Some(&changed_identity))
        .unwrap();
    let observed = std::cell::Cell::new(false);

    let error = reconcile_waiting_merge_step_with(&conn, &repo, &mut persisted, 0, 100, |_| {
        observed.set(true);
        unreachable!("stale identity must fail before provider observation")
    })
    .unwrap_err();

    assert!(error.contains("identity changed or was lost"));
    assert!(!observed.get());
    assert_eq!(persisted.steps[0].status, AutoStepStatus::Failed);
    assert_eq!(persisted.run.status, AutoRunStatus::Failed);
    assert_eq!(
        persisted.run.stabilization_status,
        Some(stabilization_model::StabilizationStatus::Escalated)
    );
}

#[test]
fn waiting_merge_reconciliation_escalates_terminal_unmerged_closure() {
    let temp = TempDir::new("merge-reconcile-closed");
    let repo = Repository::with_config_dir_for_test(
        temp.path().to_path_buf(),
        temp.path().join("prism-config"),
    );
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let mut persisted = waiting_merge_run(&conn, temp.path());

    let error =
        reconcile_waiting_merge_step_with(&conn, &repo, &mut persisted, 0, 100, |expected| {
            Ok(waiting_merge_observation(
                expected,
                crate::remote::LifecycleState::Closed,
                crate::remote::QueueState::NotQueued,
            ))
        })
        .unwrap_err();

    assert!(error.contains("closed without merging"));
    assert_eq!(persisted.steps[0].status, AutoStepStatus::Failed);
    assert_eq!(
        persisted.run.stabilization_status,
        Some(stabilization_model::StabilizationStatus::Escalated)
    );
}

#[test]
fn waiting_merge_reconciliation_stops_when_provider_removes_queue_entry() {
    let temp = TempDir::new("merge-reconcile-removed");
    let repo = Repository::with_config_dir_for_test(
        temp.path().to_path_buf(),
        temp.path().join("prism-config"),
    );
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    crate::plan_run::migrate_schema(&conn).unwrap();
    crate::execution::migrate_schema(&conn).unwrap();
    let mut persisted = waiting_merge_run(&conn, temp.path());
    crate::integration::arm_merge_intent(&conn, &persisted.run.id).unwrap();
    crate::integration::synchronize_generation(
        &conn,
        &persisted.run.id,
        &crate::integration::CandidateGeneration {
            change_request_identity: crate::remote::test_change_request_identity(),
            target_branch: "main".to_string(),
            pr_number: 42,
            head_sha: "head".to_string(),
        },
    )
    .unwrap();
    crate::integration::publish_ready(&conn, &persisted.run.id, "head").unwrap();
    crate::integration::mark_submitting(&conn, &persisted.run.id).unwrap();
    crate::integration::mark_submitted(&conn, &persisted.run.id).unwrap();

    let error =
        reconcile_waiting_merge_step_with(&conn, &repo, &mut persisted, 0, 100, |expected| {
            Ok(waiting_merge_observation(
                expected,
                crate::remote::LifecycleState::Open,
                crate::remote::QueueState::NotQueued,
            ))
        })
        .unwrap_err();

    assert!(error.contains("no longer queued"));
    assert_eq!(persisted.steps[0].status, AutoStepStatus::Failed);
    assert!(
        crate::integration::active_merge_intent(&conn, &persisted.run.id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn restart_preserves_waiting_merge_for_reconciliation() {
    let temp = TempDir::new("merge-reconcile-restart");
    let database = temp.path().join("auto.db");
    let repo = Repository::with_config_dir_for_test(
        temp.path().to_path_buf(),
        temp.path().join("prism-config"),
    );
    let conn = rusqlite::Connection::open(&database).unwrap();
    migrate_schema(&conn).unwrap();
    let persisted = waiting_merge_run(&conn, temp.path());
    let run_id = persisted.run.id.clone();
    drop(conn);

    let conn = rusqlite::Connection::open(&database).unwrap();
    migrate_schema(&conn).unwrap();
    let mut restarted = load_auto_run(&conn, &run_id).unwrap().unwrap();

    assert!(prepare_auto_run_for_resume(&conn, &mut restarted, 100).unwrap());
    assert_eq!(restarted.steps[0].status, AutoStepStatus::Waiting);
    assert_eq!(next_waiting_merge_step(&restarted), Some(0));

    let progress =
        reconcile_waiting_merge_step_with(&conn, &repo, &mut restarted, 0, 100, |expected| {
            Ok(waiting_merge_observation(
                expected,
                crate::remote::LifecycleState::Merged,
                crate::remote::QueueState::Complete,
            ))
        })
        .unwrap();

    assert_eq!(progress, MergeReconciliationProgress::Done);
    assert_eq!(restarted.steps[0].status, AutoStepStatus::Done);
}

#[cfg(unix)]
#[test]
fn headless_merge_step_refreshes_and_blocks_unknown_policy() {
    let temp = TempDir::new("merge-step-success");
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    setup_git_worktree(&origin, &work);
    run_git(&work, &["push", "-u", "origin", "feat/auto"]);
    let repo = Repository::with_config_dir_for_test(work.clone(), temp.path().join("prism-config"));
    let mut config = Config::load(&repo);
    config.auto.review_requirement = crate::config::ReviewRequirement::None;
    config.auto.merge = true;
    config.auto.review_wait_enabled = false;
    let gh_log = temp.path().join("gh.log");
    let head = crate::git::current_head_sha(&work, &config).unwrap();
    let gh = temp.path().join("gh");
    let git = temp.path().join("git");
    write_executable(
        &git,
        &format!(
            "#!/bin/sh\ncase \"$*\" in *\"ls-remote --exit-code --heads https://github.com/example/repo.git refs/heads/feat/auto\"*) printf '%s\\t%s\\n' '{head}' 'refs/heads/feat/auto'; exit 0 ;; esac\nif [ \"$3\" = \"remote\" ] && [ \"$4\" = \"get-url\" ]; then\n  printf 'https://github.com/example/repo.git\\n'\n  exit 0\nfi\nexec git \"$@\"\n"
        ),
    );
    write_executable(
        &gh,
        &format!(
            r#"#!/bin/sh
 printf 'args=%s\n' "$*" >> '{}'
case "$*" in
  *'/repos/example/repo/branches/main/protection'*)
   printf '%s\n' '{{"url":"https://api.github.com/repos/example/repo/branches/main/protection"}}'
   exit 0
   ;;
  *'/repos/example/repo/rules/branches/main?per_page=100'*)
   printf '%s\n' 'gh: Resource not accessible by integration (HTTP 403)' >&2
   exit 1
   ;;
esac
if [ "$1" = "api" ] && [ "$2" = "graphql" ] && printf '%s' "$*" | grep -q 'pullRequests(first: 100'; then
  printf '%s\n' '{{"data":{{"repository":{{"pullRequests":{{"nodes":[{{"id":"PR_test","number":42,"title":"Auto","author":{{"login":"example"}},"body":"","url":"https://github.com/example/repo/pull/42","state":"OPEN","reviewDecision":"APPROVED","reviewRequests":{{"nodes":[]}},"headRefName":"feat/auto","baseRefName":"main","headRefOid":"{}","headRepository":{{"nameWithOwner":"example/repo"}},"baseRepository":{{"nameWithOwner":"example/repo"}},"updatedAt":"2026-01-01T00:00:00Z","mergeStateStatus":"CLEAN","merged":false,"isDraft":false,"comments":{{"totalCount":0}},"reviewThreads":{{"totalCount":0}},"commits":{{"nodes":[{{"commit":{{"statusCheckRollup":{{"contexts":{{"pageInfo":{{"hasNextPage":false}},"nodes":[{{"name":"ci","status":"COMPLETED","conclusion":"SUCCESS"}}]}}}}}}}}]}}}}],"pageInfo":{{"hasNextPage":false}}}}}}}}}}'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ] && printf '%s' "$*" | grep -q '\$number: Int!)'; then
  printf '%s\n' '{{"data":{{"repository":{{"pullRequest":{{"id":"PR_test","number":42,"title":"Auto","state":"OPEN","reviewDecision":"APPROVED","headRefName":"feat/auto","baseRefName":"main","headRefOid":"{}","headRepository":{{"nameWithOwner":"example/repo"}},"baseRepository":{{"nameWithOwner":"example/repo"}},"mergeStateStatus":"CLEAN","commits":{{"nodes":[{{"commit":{{"statusCheckRollup":{{"contexts":{{"pageInfo":{{"hasNextPage":false}},"nodes":[{{"name":"ci","status":"COMPLETED","conclusion":"SUCCESS"}}]}}}}}}}}]}}}}}}}}}}'
  exit 0
fi
case "$*" in
  *'/repos/example/repo/issues/42/comments?per_page=100'*|*'/repos/example/repo/pulls/42/reviews?per_page=100'*|*'/repos/example/repo/pulls/42/files?per_page=100'*|*'/repos/example/repo/commits/{}/statuses?per_page=100'*) printf '%s\n' '[[]]'; exit 0 ;;
  *'/repos/example/repo/commits/{}/check-runs?per_page=100'*) printf '%s\n' '[{{"total_count":0,"check_runs":[]}}]'; exit 0 ;;
esac
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "feat/auto" ]; then
  cat <<'JSON'
{{"id":"PR_test","number":42,"title":"Auto","body":"","url":"https://github.com/example/repo/pull/42","state":"OPEN","reviewDecision":"APPROVED","reviewRequests":[],"headRefName":"feat/auto","baseRefName":"main","headRefOid":"{}","headRepository":{{"nameWithOwner":"example/repo"}},"baseRepository":{{"nameWithOwner":"example/repo"}},"updatedAt":"2026-01-01T00:00:00Z","statusCheckRollup":{{"contexts":{{"nodes":[{{"__typename":"StatusContext","context":"ci","state":"SUCCESS"}}]}}}},"mergeStateStatus":"CLEAN","mergedAt":null,"isDraft":false}}
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
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  printf '%s\n' '[{{"data":{{"repository":{{"pullRequest":{{"reviewThreads":{{"totalCount":0,"pageInfo":{{"hasNextPage":false}},"nodes":[]}}}}}}}}}}]'
  exit 0
fi
exit 1
"#,
            gh_log.display(),
            head,
            head,
            head,
            head,
            head
        ),
    );
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
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
    persisted.run.pr_number = Some(42);
    persisted.steps[0].work_guard = Some(stabilization_model::WorkGuard {
        change_request_identity: Some(crate::remote::test_change_request_identity()),
        authorized_target_branch: Some("main".to_string()),
        local_head_sha: Some(head.clone()),
        remote_head_sha: Some(head.clone()),
        pr_head_sha: Some(head.clone()),
        base_sha: Some(head.clone()),
        review_thread_ids: Vec::new(),
    });
    save_auto_run(&conn, &mut persisted).unwrap();
    start_non_agent_step(&conn, &mut persisted, 0).unwrap();

    let error = execute_merge_step(&conn, &repo, &config, &mut persisted, 0, 100).unwrap_err();

    let loaded = load_auto_run(&conn, &persisted.run.id).unwrap().unwrap();
    assert!(error.contains("repository policy is unknown"), "{error}");
    assert_eq!(loaded.steps[0].status, AutoStepStatus::Failed);
    let commands = fs::read_to_string(gh_log).unwrap();
    assert!(commands.contains("/repos/example/repo/branches/main/protection"));
    assert!(commands.contains("/repos/example/repo/rules/branches/main?per_page=100"));
    assert!(!commands.contains("args=pr merge"));
}

#[test]
fn cleanup_after_restart_rejects_legacy_run_without_incarnation() {
    let temp = TempDir::new("cleanup-legacy-incarnation");
    let repo = Repository::with_config_dir_for_test(
        temp.path().join("repo"),
        temp.path().join("prism-config"),
    );
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let mut persisted = AutoLaunch::new(&repo.root, &repo.root, "feat/auto", "Implement auto")
        .unwrap()
        .create_run();
    persisted.run.worktree_incarnation = None;
    persisted.steps.clear();
    persisted.steps.push(AutoStepRun::queued(
        &persisted.run.id,
        1,
        AutoStepKey::Cleanup,
        1,
        Some("cleanup".to_string()),
    ));
    save_auto_run(&conn, &mut persisted).unwrap();
    start_non_agent_step(&conn, &mut persisted, 0).unwrap();
    let mut config = test_config();
    config.auto.cleanup_after_merge = true;

    let error = execute_cleanup_step(&conn, &repo, &config, &mut persisted, 0, 100)
        .expect_err("legacy cleanup must fail closed");

    assert!(error.contains("no persisted worktree incarnation"));
}

#[test]
fn cleanup_after_restart_rejects_replaced_worktree_incarnation() {
    let temp = TempDir::new("cleanup-replaced-incarnation");
    let worktree = temp.path().join("worktree");
    fs::create_dir_all(&worktree).unwrap();
    fs::write(worktree.join(".git"), "old git link\n").unwrap();
    let repo = Repository::with_config_dir_for_test(
        temp.path().join("repo"),
        temp.path().join("prism-config"),
    );
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let mut persisted = AutoLaunch::new(&repo.root, &worktree, "feat/auto", "Implement auto")
        .unwrap()
        .create_run();
    persisted.steps.clear();
    persisted.steps.push(AutoStepRun::queued(
        &persisted.run.id,
        1,
        AutoStepKey::Cleanup,
        1,
        Some("cleanup".to_string()),
    ));
    save_auto_run(&conn, &mut persisted).unwrap();
    let mut restarted = load_auto_run(&conn, &persisted.run.id)
        .unwrap()
        .expect("restarted run");
    start_non_agent_step(&conn, &mut restarted, 0).unwrap();
    fs::remove_file(worktree.join(".git")).unwrap();
    fs::write(worktree.join(".git"), "replacement git link\n").unwrap();
    let mut config = test_config();
    config.auto.cleanup_after_merge = true;

    let error = execute_cleanup_step(&conn, &repo, &config, &mut restarted, 0, 100)
        .expect_err("replacement cleanup must fail closed");

    assert!(error.contains("was replaced"));
    assert!(worktree.join(".git").exists());
}

#[test]
fn cleanup_escalates_pending_worktrunk_approval_without_retiring_metadata() {
    let temp = TempDir::new("cleanup-worktrunk-approval");
    let worktree = temp.path().join("worktree");
    fs::create_dir_all(&worktree).unwrap();
    fs::write(
        worktree.join(".git"),
        "gitdir: /repo/.git/worktrees/feature\n",
    )
    .unwrap();
    let repo = Repository::with_config_dir_for_test(
        temp.path().join("repo"),
        temp.path().join("prism-config"),
    );
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let mut persisted = AutoLaunch::new(&repo.root, &worktree, "feat/auto", "Implement auto")
        .unwrap()
        .create_run();
    persisted.steps.clear();
    persisted.steps.push(AutoStepRun::queued(
        &persisted.run.id,
        1,
        AutoStepKey::Cleanup,
        1,
        Some("cleanup".to_string()),
    ));
    save_auto_run(&conn, &mut persisted).unwrap();
    start_non_agent_step(&conn, &mut persisted, 0).unwrap();
    crate::observability::with_writable_db(&repo, |db| {
        db.execute(
            "insert into task_metadata (
                branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
             ) values (?1, '', '', ?2, 0)",
            rusqlite::params!["feat/auto", worktree.display().to_string()],
        )
        .map_err(|error| error.to_string())?;
        Ok(())
    })
    .unwrap();
    let wt = temp.path().join("wt");
    write_executable(
        &wt,
        "#!/bin/sh\nprintf '%s\\n' 'repo needs approval to execute commands; cannot prompt in non-interactive mode' >&2\nexit 1\n",
    );
    let mut config = test_config();
    config.auto.cleanup_after_merge = true;
    config
        .tools
        .insert("wt".to_string(), wt.display().to_string());

    let error = execute_cleanup_step(&conn, &repo, &config, &mut persisted, 0, 100)
        .expect_err("pending approval must stop cleanup");

    assert!(error.contains("requires interactive approval"));
    assert!(error.contains("config approvals add"));
    let retained = crate::observability::with_writable_db(&repo, |db| {
        db.query_row(
            "select count(*) from task_metadata where branch = 'feat/auto'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| error.to_string())
    })
    .unwrap();
    assert_eq!(retained, 1);
    assert!(worktree.exists());
}

#[test]
fn pending_cleanup_intent_with_present_worktree_rechecks_worktrunk_approval() {
    let temp = TempDir::new("pending-cleanup-rechecks-approval");
    let worktree = temp.path().join("worktree");
    fs::create_dir_all(&worktree).unwrap();
    fs::write(
        worktree.join(".git"),
        "gitdir: /repo/.git/worktrees/feature\n",
    )
    .unwrap();
    let repo = Repository::with_config_dir_for_test(
        temp.path().join("repo"),
        temp.path().join("prism-config"),
    );
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate_schema(&conn).unwrap();
    let mut persisted = AutoLaunch::new(&repo.root, &worktree, "feat/auto", "Implement auto")
        .unwrap()
        .create_run();
    let incarnation = persisted.run.worktree_incarnation.clone().unwrap();
    persisted.steps.clear();
    persisted.steps.push(AutoStepRun::queued(
        &persisted.run.id,
        1,
        AutoStepKey::Cleanup,
        1,
        Some("cleanup".to_string()),
    ));
    save_auto_run(&conn, &mut persisted).unwrap();
    start_non_agent_step(&conn, &mut persisted, 0).unwrap();
    crate::observability::with_writable_db(&repo, |db| {
        db.execute(
            "insert into task_metadata (
                branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
             ) values (?1, '', '', ?2, 0)",
            rusqlite::params![persisted.run.branch, worktree.display().to_string()],
        )
        .map_err(|error| error.to_string())?;
        db.execute(
            "insert into pending_worktree_deletion (
                branch, worktree_path, worktree_incarnation, branch_oid,
                worktree_removed, branch_deleted, updated_unix_ms
             ) values (?1, ?2, ?3, 'branch-oid', 0, 0, 0)",
            rusqlite::params![
                persisted.run.branch,
                worktree.display().to_string(),
                incarnation
            ],
        )
        .map_err(|error| error.to_string())?;
        Ok(())
    })
    .unwrap();
    let wt_log = temp.path().join("wt.log");
    let wt = temp.path().join("wt");
    write_executable(
        &wt,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s\\n' 'repo needs approval to execute commands; cannot prompt in non-interactive mode' >&2\nexit 1\n",
            wt_log.display()
        ),
    );
    let mut config = test_config();
    config.auto.cleanup_after_merge = true;
    config
        .tools
        .insert("wt".to_string(), wt.display().to_string());

    let error = execute_cleanup_step(&conn, &repo, &config, &mut persisted, 0, 100)
        .expect_err("new approval requirement must stop pending cleanup");

    assert!(error.contains("requires interactive approval"));
    assert!(worktree.exists());
    let wt_commands = fs::read_to_string(wt_log).unwrap();
    assert!(wt_commands.contains("config approvals add"));
    assert!(!wt_commands.contains("remove"));
    let (metadata, pending) = crate::observability::with_writable_db(&repo, |db| {
        let metadata = db
            .query_row(
                "select count(*) from task_metadata where branch = 'feat/auto'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| error.to_string())?;
        let pending = db
            .query_row(
                "select count(*) from pending_worktree_deletion
                 where branch = 'feat/auto' and worktree_removed = 0",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| error.to_string())?;
        Ok((metadata, pending))
    })
    .unwrap();
    assert_eq!((metadata, pending), (1, 1));
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
        execution: crate::harness::ExecutionRef::default(),
        session: crate::harness::SessionRef::default(),
        plan_run_id: None,
        commit_sha: None,
        head_sha: None,
        work_guard: None,
        blocker: None,
        summary: Some("done".to_string()),
        error: None,
    });
}

fn waiting_merge_run(conn: &rusqlite::Connection, root: &Path) -> PersistedAutoRun {
    let mut persisted = AutoLaunch::new(root, root, "feat/auto", "Merge pending change request")
        .unwrap()
        .create_run();
    persisted.steps.clear();
    persisted.steps.push(AutoStepRun::running(
        &persisted.run.id,
        1,
        AutoStepKey::Merge,
        1,
    ));
    persisted.steps[0].status = AutoStepStatus::Waiting;
    persisted.steps[0].summary = Some("merge accepted and pending".to_string());
    let identity = crate::remote::test_change_request_identity();
    persisted.steps[0].work_guard = Some(stabilization_model::WorkGuard {
        change_request_identity: Some(identity.clone()),
        authorized_target_branch: Some("main".to_string()),
        local_head_sha: Some("head".to_string()),
        remote_head_sha: Some("head".to_string()),
        pr_head_sha: Some("head".to_string()),
        base_sha: Some("base".to_string()),
        review_thread_ids: Vec::new(),
    });
    persisted.run.pr_number = Some(42);
    persisted.run.status = AutoRunStatus::Running;
    persisted.run.stabilization_status = Some(stabilization_model::StabilizationStatus::Waiting);
    persisted.run.stabilization_blocker =
        Some(stabilization_model::StabilizationBlocker::ReadyToAutoMerge);
    persisted.run.stabilization_next_work = Some(stabilization_model::StabilizationWorkKind::Merge);
    save_auto_run(conn, &mut persisted).unwrap();
    save_observed_change_request_identity(conn, &persisted.run.id, Some(&identity)).unwrap();
    persisted
}

fn waiting_merge_observation(
    expected: &crate::remote::ChangeRequest,
    lifecycle: crate::remote::LifecycleState,
    queue_state: crate::remote::QueueState,
) -> crate::remote::ChangeRequestSummary {
    crate::remote::ChangeRequestSummary {
        change_request: expected.clone(),
        title: "Auto".to_string(),
        author: "author".to_string(),
        body: String::new(),
        web_url: Some("https://example.com/pr/42".to_string()),
        lifecycle,
        review_decision: crate::remote::ReviewDecision::Approved,
        requested_reviewers: Vec::new(),
        mergeability: crate::remote::MergeabilityState::Mergeable,
        check_state: crate::remote::CheckState::Passed,
        queue_state,
        native_state_evidence: crate::remote::NativeStateEvidence::default(),
        comment_count: 0,
        draft: false,
        updated_at: Some("2026-01-01T00:00:00Z".to_string()),
    }
}

fn standalone_completed_repair(
    conn: &rusqlite::Connection,
    repair_step: AutoStepKey,
) -> PersistedAutoRun {
    let repo = PathBuf::from("/repo/prism");
    let mut persisted = AutoLaunch::new(&repo, &repo.join("feature"), "feat/auto", "Repair PR")
        .unwrap()
        .create_run();
    persisted.run.variant = "repair".to_string();
    persisted.steps.clear();
    push_test_step(&mut persisted, 1, repair_step, AutoStepStatus::Done);
    save_auto_run(conn, &mut persisted).unwrap();
    persisted
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

fn test_config() -> Config {
    let mut config = crate::test_support::test_config();
    config.default_agent = "opencode".to_string();
    config
}

fn ensure_next_test_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<bool, String> {
    let repo = Repository::with_config_dir_for_test(
        PathBuf::from(&persisted.run.repo_root),
        PathBuf::from("/tmp/prism-auto-flow-test-config"),
    );
    ensure_next_auto_step_with_context(conn, &repo, &test_config(), persisted)
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
    let cache = crate::remote::PrCache::observed(
        crate::remote::PrSummary {
            number: 42,
            change_request_identity: Some(crate::remote::test_change_request_identity()),
            native_state_evidence: crate::remote::NativeStateEvidence::default(),
            title: "Auto".to_string(),
            author: "author".to_string(),
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
            queue_state: "not_queued".to_string(),
            comment_count: 0,
            merged: false,
            draft: false,
        },
        Some(crate::remote::PrDetails::default()),
    );
    crate::remote::save_pr_cache(repo, branch, &cache).unwrap();
}

#[cfg(unix)]
fn configure_pr_observation(temp: &TempDir, config: &mut Config, branch: &str, head_sha: &str) {
    let gh = temp.path().join("gh");
    let git = temp.path().join("git");
    write_executable(
        &git,
        &format!(
            "#!/bin/sh\ncase \"$*\" in\n  *\"remote get-url\"*) printf 'https://github.com/example/repo.git\\n'; exit 0 ;;\n  *\"ls-remote --exit-code --heads https://github.com/example/repo.git refs/heads/{branch}\"*) actual=$(git -C \"$2\" remote get-url origin); git ls-remote --exit-code --heads \"$actual\" 'refs/heads/{branch}'; exit $? ;;\nesac\nexec git \"$@\"\n"
        ),
    );
    let script = format!(
        r#"#!/bin/sh
case "$*" in
  *"/repos/example/repo/branches/main/protection"*) printf '%s\n' 'gh: Branch not protected (HTTP 404)' >&2; exit 1 ;;
  *"/repos/example/repo/rules/branches/main?per_page=100"*) printf '%s\n' '[[]]' ;;
  *"/repos/example/repo/issues/42/comments?per_page=100"*|*"/repos/example/repo/pulls/42/reviews?per_page=100"*|*"/repos/example/repo/pulls/42/files?per_page=100"*|*"/repos/example/repo/commits/{head_sha}/statuses?per_page=100"*) printf '%s\n' '[[]]' ;;
  *"/repos/example/repo/commits/{head_sha}/check-runs?per_page=100"*) printf '%s\n' '[{{"total_count":1,"check_runs":[{{"name":"ci","status":"completed","conclusion":"success"}}]}}]' ;;
  *"reviewThreads(first: 100"*)
    printf '%s\n' '[{{"data":{{"repository":{{"pullRequest":{{"reviewThreads":{{"totalCount":0,"pageInfo":{{"hasNextPage":false}},"nodes":[]}}}}}}}}}}]'
    ;;
  *"pullRequests(first: 100"*)
    printf '%s\n' '[{{"data":{{"repository":{{"pullRequests":{{"nodes":[{{"id":"PR_test","number":42,"title":"Auto","body":"","url":"https://example.com/pr/42","state":"OPEN","reviewDecision":"","reviewRequests":{{"nodes":[]}},"headRefName":"{branch}","baseRefName":"main","headRefOid":"{head_sha}","headRepository":{{"nameWithOwner":"example/repo"}},"baseRepository":{{"nameWithOwner":"example/repo"}},"updatedAt":"2026-01-01T00:00:00Z","comments":{{"totalCount":0}},"commits":{{"nodes":[{{"commit":{{"statusCheckRollup":{{"contexts":{{"pageInfo":{{"hasNextPage":false}},"nodes":[{{"context":"ci","state":"SUCCESS"}}]}}}}}}}}]}},"mergeStateStatus":"CLEAN","isDraft":false}}],"pageInfo":{{"hasNextPage":false}}}}}}}}}}]'
    ;;
  api\ graphql*)
    printf '%s\n' '{{"data":{{"repository":{{"pullRequest":{{"id":"PR_test","number":42,"title":"Auto","state":"OPEN","headRefName":"{branch}","baseRefName":"main","headRefOid":"{head_sha}","headRepository":{{"nameWithOwner":"example/repo"}},"baseRepository":{{"nameWithOwner":"example/repo"}},"mergeStateStatus":"CLEAN","commits":{{"nodes":[{{"commit":{{"statusCheckRollup":{{"contexts":{{"pageInfo":{{"hasNextPage":false}},"nodes":[{{"context":"ci","state":"SUCCESS"}}]}}}}}}}}]}}}}}}}}}}'
    ;;
  "run list "*)
    printf '[]\n'
    ;;
  *)
    printf '%s\n' '{{"id":"PR_test","number":42,"title":"Auto","body":"","url":"https://github.com/example/repo/pull/42","state":"OPEN","reviewDecision":"","reviewRequests":{{"nodes":[]}},"headRefName":"{branch}","baseRefName":"main","headRefOid":"{head_sha}","headRepository":{{"nameWithOwner":"example/repo"}},"baseRepository":{{"nameWithOwner":"example/repo"}},"updatedAt":"2026-01-01T00:00:00Z","comments":{{"totalCount":0}},"statusCheckRollup":{{"contexts":{{"nodes":[{{"__typename":"StatusContext","context":"ci","state":"SUCCESS"}}]}}}},"mergeStateStatus":"CLEAN","isDraft":false}}'
    ;;
esac
"#
    );
    write_executable(&gh, &script);
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
}

fn test_pr_summary(branch: &str, head_sha: &str, updated_at: &str) -> crate::remote::PrSummary {
    crate::remote::PrSummary {
        number: 42,
        change_request_identity: Some(crate::remote::test_change_request_identity()),
        native_state_evidence: crate::remote::NativeStateEvidence::default(),
        title: "Auto".to_string(),
        author: "author".to_string(),
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
        queue_state: "not_queued".to_string(),
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
