use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent::AgentState;
use crate::auto_flow::{
    AutoImplementationSource, AutoRun, AutoRunMode, AutoRunStatus, PersistedAutoRun,
};
use crate::config::Config;
use crate::plan_run::{
    PersistedPlanRun, PlanRun, PlanRunMode, PlanRunStatus, PlanStepRun, PlanStepStatus,
};
use crate::remote::{PrCache, PrSummary};
use crate::repo::Repository;
use crate::session::Session;

use super::super::{ManagedRepo, Tui};

pub(super) fn test_tui() -> Tui {
    let repos = vec![
        ManagedRepo::new(
            Repository {
                root: PathBuf::from("/repo-one"),
            },
            test_config(),
            Some('1'),
        ),
        ManagedRepo::new(
            Repository {
                root: PathBuf::from("/repo-two"),
            },
            test_config(),
            Some('2'),
        ),
    ];
    let sessions = vec![
        test_session(0, "/repo-one", "main"),
        test_session(0, "/repo-one", "feature-one"),
        test_session(1, "/repo-two", "main"),
        test_session(1, "/repo-two", "feature-two"),
    ];
    Tui::new(repos, 0, sessions)
}

pub(super) fn test_auto_run(
    id: &str,
    worktree_path: &str,
    updated_unix_ms: u64,
) -> PersistedAutoRun {
    PersistedAutoRun {
        run: AutoRun {
            harness_id: "opencode".to_string(),
            adapter_id: "opencode".to_string(),
            id: id.to_string(),
            repo_root: "/repo-one".to_string(),
            worktree_path: PathBuf::from(worktree_path),
            worktree_incarnation: None,
            worktree_session_id: Some(test_worktree_session_id(worktree_path)),
            branch: "feature".to_string(),
            mode: AutoRunMode::Standard,
            implementation_source: AutoImplementationSource::Prompt,
            plan_path: None,
            plan_run_mode: PlanRunMode::Sequential,
            variant: "default".to_string(),
            agent_profile: None,
            prompt_summary: id.to_string(),
            initial_prompt: String::new(),
            status: AutoRunStatus::Running,
            pause_requested: false,
            selected_step_run_id: None,
            pr_number: None,
            pr_url: None,
            current_head_sha: None,
            review_baseline_json: None,
            stabilization_status: None,
            stabilization_blocker: None,
            stabilization_next_work: None,
            pending_push: None,
            created_unix_ms: 1,
            updated_unix_ms,
            archived_unix_ms: None,
        },
        steps: Vec::new(),
    }
}

pub(super) fn test_plan_run(id: &str, scope_path: &str) -> PersistedPlanRun {
    PersistedPlanRun {
        run: PlanRun {
            harness_id: "opencode".to_string(),
            adapter_id: "opencode".to_string(),
            id: id.to_string(),
            repo_root: "/repo-one".to_string(),
            scope_path: PathBuf::from(scope_path),
            worktree_session_id: Some(test_worktree_session_id(scope_path)),
            plan_path: PathBuf::from("plan.md"),
            plan_display: "plan.md".to_string(),
            step_name: "phase".to_string(),
            start_step: 1,
            total_steps: 1,
            mode: PlanRunMode::Sequential,
            status: PlanRunStatus::Running,
            pause_requested: false,
            selected_step: 1,
            created_unix_ms: 1,
            updated_unix_ms: 1,
            archived_unix_ms: None,
        },
        steps: Vec::new(),
    }
}

fn test_worktree_session_id(path: &str) -> String {
    let branch = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("worktree");
    let repo_index = usize::from(path.starts_with("/repo-two/"));
    format!("test-{repo_index}-{branch}")
}

pub(super) fn test_plan_run_with_steps(
    id: &str,
    scope_path: &str,
    selected_step: usize,
) -> PersistedPlanRun {
    let mut run = test_plan_run(id, scope_path);
    run.run.total_steps = 3;
    run.run.selected_step = selected_step;
    run.steps = (1..=3)
        .map(|step| PlanStepRun {
            run_id: id.to_string(),
            step,
            prompt: format!("phase {step}"),
            status: if step == selected_step {
                PlanStepStatus::Running
            } else {
                PlanStepStatus::Queued
            },
            execution: crate::harness::ExecutionRef::default(),
            session: crate::harness::SessionRef::default(),
            agent_variant: None,
            started_unix_ms: (step == selected_step).then_some(step as u64),
            finished_unix_ms: None,
            exit_code: None,
            latest_message: None,
            active_tool: None,
            todos: Vec::new(),
            summary: None,
            error: None,
        })
        .collect();
    run
}

pub(super) fn test_session(repo_index: usize, root: &str, branch: &str) -> Session {
    let path = PathBuf::from(format!("{root}/{branch}"));
    let _ = fs::create_dir_all(&path);
    Session {
        repo_index,
        repo_label: format!("repo-{repo_index}"),
        repo_key: None,
        path: path.clone(),
        worktree_session_id: format!("test-{repo_index}-{branch}"),
        incarnation: String::new(),
        path_display: path.display().to_string(),
        branch: branch.to_string(),
        prompt_summary: String::new(),
        classification: crate::session::SessionClassification::Work,
        visibility: 0,
        adopted: false,
        hidden: false,
        status_label: "clean".to_string(),
        agent_state: AgentState::Idle,
        opencode_status: None,
        pr: PrCache::default(),
        wt_columns: BTreeMap::new(),
        unseen_comments: false,
    }
}

pub(super) fn test_config() -> Config {
    let mut config = crate::test_support::test_config();
    config.default_agent = "opencode".to_string();
    config.default_base = Some("main".to_string());
    config
}

pub(super) fn test_pr_summary(merged: bool) -> PrSummary {
    PrSummary {
        number: 1,
        change_request_identity: None,
        native_state_evidence: crate::remote::NativeStateEvidence::default(),
        title: "PR".to_string(),
        author: "author".to_string(),
        body: String::new(),
        url: "https://example.test/pr/1".to_string(),
        state: if merged { "MERGED" } else { "OPEN" }.to_string(),
        review_decision: String::new(),
        requested_reviewers: Vec::new(),
        head_ref: "feature".to_string(),
        base_ref: "main".to_string(),
        head_sha: "abc123".to_string(),
        updated_at: String::new(),
        check_status: String::new(),
        merge_state_status: String::new(),
        queue_state: String::new(),
        comment_count: 0,
        merged,
        draft: false,
    }
}

pub(super) fn test_change_request_identity(
    provider: crate::remote::ProviderKind,
) -> crate::remote::CanonicalChangeRequestIdentity {
    test_change_request_identity_for(provider, "example/repo", "change-request-1")
}

pub(super) fn test_change_request_identity_for(
    provider: crate::remote::ProviderKind,
    project_path: &str,
    native_id: &str,
) -> crate::remote::CanonicalChangeRequestIdentity {
    let host = match provider {
        crate::remote::ProviderKind::GitHub => "github.com",
        crate::remote::ProviderKind::GitLab => "gitlab.com",
        crate::remote::ProviderKind::Forgejo => "codeberg.org",
    };
    let host = crate::remote::HostIdentity::new(host, None).unwrap();
    let repository = crate::remote::RemoteRepositoryId::new(provider, host, project_path).unwrap();
    crate::remote::CanonicalChangeRequestIdentity::new(
        &repository,
        &crate::remote::NativeChangeRequestId::new(native_id).unwrap(),
        &repository,
        &repository,
    )
}

pub(super) fn unique_temp_dir(prefix: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{unique}", std::process::id()))
}
