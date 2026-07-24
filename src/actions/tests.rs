use crate::agent::AgentState;
use crate::agent_session::{AgentSessionSlot, AgentSessionWarmupKey, AgentSessionWarmupResult};
use crate::auto_flow::stabilization_execute::{GuardedPushProgress, progress_pending_push};
use crate::auto_flow::stabilization_model::{
    PendingPushGuard, RepairKind, StabilizationBlocker, StabilizationWorkKind,
};
use crate::auto_flow::{AutoLaunch, AutoStepKey, load_auto_run, save_auto_run};
use crate::config::Config;
use crate::github::{PrCache, PrComment, PrDetails, PrSummary, pr_summary_or_error};
use crate::opencode::{OpencodeState, OpencodeStatus, parse_event_payload};
use crate::plan_run::PlanRunMode;
use crate::repo::Repository;
use crate::session::Session;
use crate::tui::{
    DefaultBranchPollResult, OpencodeEventResult, OpencodePollKey, OpencodePollResult, PanelFocus,
    Tui, WtPollResult,
};

use super::{
    apply_bulk_review_resolution, archived_picker_overflow_message, discover_wt_columns,
    plan_run_mode_from_parallel_confirmation, pr_target_choice_list, pr_target_repo_for_choice,
    remote_pr_choice_keys, remote_pr_worktree_branch, run_browser_opener,
    should_prompt_pr_target_choice, status_label_with_behind, unresolved_review_thread_ids,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[test]
fn standalone_plan_confirmation_defaults_to_sequential() {
    assert_eq!(
        plan_run_mode_from_parallel_confirmation(false),
        PlanRunMode::Sequential
    );
    assert_eq!(
        plan_run_mode_from_parallel_confirmation(true),
        PlanRunMode::Parallel
    );
}

#[test]
fn declined_bulk_review_resolution_does_not_resolve_threads() {
    let resolved = RefCell::new(Vec::new());

    let count = apply_bulk_review_resolution(
        false,
        &["thread-1".to_string(), "thread-2".to_string()],
        |thread_id| {
            resolved.borrow_mut().push(thread_id.to_string());
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(count, 0);
    assert!(resolved.borrow().is_empty());
}

#[test]
fn confirmed_bulk_review_resolution_resolves_each_thread_once() {
    let resolved = RefCell::new(Vec::new());

    let count = apply_bulk_review_resolution(
        true,
        &[
            "thread-2".to_string(),
            "thread-1".to_string(),
            "thread-2".to_string(),
        ],
        |thread_id| {
            resolved.borrow_mut().push(thread_id.to_string());
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(count, 2);
    assert_eq!(resolved.into_inner(), vec!["thread-1", "thread-2"]);
}

#[test]
fn review_resolution_uses_only_unresolved_threads_in_the_observed_details() {
    let details = PrDetails {
        review_comments: vec![
            crate::github::PrReviewComment {
                thread_id: "thread-2".to_string(),
                resolved: false,
                ..crate::github::PrReviewComment::default()
            },
            crate::github::PrReviewComment {
                thread_id: "thread-1".to_string(),
                resolved: false,
                ..crate::github::PrReviewComment::default()
            },
            crate::github::PrReviewComment {
                thread_id: "thread-2".to_string(),
                resolved: true,
                ..crate::github::PrReviewComment::default()
            },
        ],
        ..PrDetails::default()
    };

    assert_eq!(
        unresolved_review_thread_ids(&details),
        vec!["thread-1", "thread-2"]
    );
}

#[test]
fn browser_opener_invokes_first_available_candidate() {
    let temp = unique_temp_dir("prism-browser-opener-test");
    fs::create_dir_all(&temp).unwrap();
    let log = temp.join("open.log");
    let opener = temp.join("opener");
    fs::write(
        &opener,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$@" > '{}'
exit 0
"#,
            log.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&opener).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&opener, permissions).unwrap();
    let opener = opener.display().to_string();

    let no_args: &[&str] = &[];
    let flag_args: &[&str] = &["--flag"];
    let candidates = [
        ("/definitely/missing", no_args),
        (opener.as_str(), flag_args),
    ];

    let used = run_browser_opener(&candidates, "https://example.test/pr/42").unwrap();

    assert_eq!(used, opener);
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "--flag\nhttps://example.test/pr/42\n"
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn discover_wt_columns_flattens_available_primitive_values() {
    let columns = discover_wt_columns(
        r#"{
            "path":"/repo/feature",
            "url":"https://example.test/pr/42",
            "url_active":true,
            "ci":{"status":"success","number":42},
            "vars":{"localdev":"on"},
            "empty":"",
            "labels":["bug"]
        }"#,
    );

    assert_eq!(
        columns.get("url").map(String::as_str),
        Some("https://example.test/pr/42")
    );
    assert_eq!(columns.get("url_active").map(String::as_str), Some("true"));
    assert_eq!(
        columns.get("ci.status").map(String::as_str),
        Some("success")
    );
    assert_eq!(columns.get("ci.number").map(String::as_str), Some("42"));
    assert_eq!(columns.get("vars.localdev").map(String::as_str), Some("on"));
    assert!(!columns.contains_key("path"));
    assert!(!columns.contains_key("empty"));
    assert!(!columns.contains_key("labels"));
}

#[test]
fn pr_target_choices_offer_upstream_and_origin() {
    let choices = pr_target_choice_list("me/repo", "org/repo");

    assert_eq!(choices.title, "Create Pull Request Target");
    assert_eq!(choices.choices.len(), 2);
    assert_eq!(choices.choices[0].key, "u");
    assert_eq!(choices.choices[0].label, "upstream (org/repo)");
    assert_eq!(choices.choices[1].key, "o");
    assert_eq!(choices.choices[1].label, "origin (me/repo)");
    assert_eq!(
        pr_target_repo_for_choice("u", "me/repo", "org/repo"),
        Some("org/repo".to_string())
    );
    assert_eq!(
        pr_target_repo_for_choice("o", "me/repo", "org/repo"),
        Some("me/repo".to_string())
    );
    assert_eq!(pr_target_repo_for_choice("x", "me/repo", "org/repo"), None);
    assert!(should_prompt_pr_target_choice("me/repo", "org/repo"));
    assert!(!should_prompt_pr_target_choice("me/repo", "me/repo"));
}

#[test]
fn remote_pr_picker_uses_stable_keys_and_branch_names() {
    let keys = remote_pr_choice_keys();

    assert_eq!(keys.first().map(String::as_str), Some("1"));
    assert_eq!(keys.get(8).map(String::as_str), Some("9"));
    assert_eq!(keys.get(9).map(String::as_str), Some("a"));
    assert_eq!(remote_pr_worktree_branch(42), "pr/42");
}

#[test]
fn review_fix_refreshes_pr_details_before_sending_prompt() {
    let temp = unique_temp_dir("prism-review-fix-refresh-test");
    let repo_root = temp.join("repo");
    let worktree = repo_root.join("feature");
    fs::create_dir_all(&worktree).unwrap();
    let gh = temp.join("gh");
    let git = temp.join("git");

    fs::write(
        &gh,
        r#"#!/bin/sh
case "$*" in
  "pr view feature --json comments,reviews,files,statusCheckRollup")
cat <<'JSON'
{"comments":[{"id":"PRC_fresh","author":{"login":"reviewer"},"body":"fresh top-level comment","createdAt":"2026-06-14T12:00:00Z"}],"reviews":[{"id":"PRR_fresh","author":{"login":"bot"},"state":"CHANGES_REQUESTED","body":"fresh review body","submittedAt":"2026-06-14T12:01:00Z"}],"files":[],"statusCheckRollup":{"contexts":{"nodes":[]}}}
JSON
;;
  api\ graphql*)
cat <<'JSON'
{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"id":"PRRT_fresh","isResolved":false,"comments":{"nodes":[{"id":"PRRC_fresh","path":"src/lib.rs","originalLine":12,"body":"fresh inline comment","createdAt":"2026-06-14T12:01:30Z","author":{"login":"reviewer"}}]}}]}}}}}
JSON
;;
  *)
cat <<'JSON'
{"number":42,"title":"Review refresh","body":"","url":"https://github.com/example/repo/pull/42","state":"OPEN","reviewDecision":"CHANGES_REQUESTED","reviewRequests":{"nodes":[]},"headRefName":"feature","baseRefName":"main","headRefOid":"abc123","updatedAt":"2026-06-14T12:02:00Z","comments":{"totalCount":2},"statusCheckRollup":{"contexts":{"nodes":[]}},"isDraft":false}
JSON
;;
esac
"#,
    )
    .unwrap();
    fs::write(
        &git,
        r#"#!/bin/sh
case "$*" in
  *"remote get-url origin"*)
echo "https://github.com/example/repo.git"
;;
esac
"#,
    )
    .unwrap();
    for executable in [&gh, &git] {
        let mut permissions = fs::metadata(executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(executable, permissions).unwrap();
    }

    let mut config = test_config();
    config.default_base = Some("main".to_string());
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let repo = Repository::with_config_dir_for_test(repo_root.clone(), temp.join("config"));
    let mut session = test_session(worktree, "feature");
    session.pr = PrCache::observed(
        PrSummary {
            number: 42,
            title: "Stale review".to_string(),
            body: String::new(),
            url: "https://github.com/example/repo/pull/42".to_string(),
            state: "OPEN".to_string(),
            review_decision: "CHANGES_REQUESTED".to_string(),
            requested_reviewers: Vec::new(),
            head_ref: "feature".to_string(),
            base_ref: "main".to_string(),
            head_sha: "oldsha".to_string(),
            updated_at: "2026-06-14T11:00:00Z".to_string(),
            check_status: "unknown".to_string(),
            merge_state_status: "CLEAN".to_string(),
            comment_count: 1,
            merged: false,
            draft: false,
        },
        Some(PrDetails {
            comments: vec![PrComment {
                author: "reviewer".to_string(),
                body: "stale cached comment".to_string(),
                ..PrComment::default()
            }],
            ..PrDetails::default()
        }),
    );
    let mut tui = Tui::new_single(repo, config, vec![session]);
    tui.prompt_submissions = Some(Vec::new());

    tui.start_review_fix_for_test().unwrap();

    let run_id = tui
        .active_auto_runs
        .get(&tui.sessions[0].path)
        .unwrap()
        .clone();
    let persisted =
        crate::observability::with_writable_db(&tui.repo, |conn| load_auto_run(conn, &run_id))
            .unwrap()
            .unwrap();
    assert_eq!(persisted.steps.len(), 1);
    assert_eq!(persisted.steps[0].step_key, AutoStepKey::FixReview);
    let prompt = persisted.steps[0].reason.as_deref().unwrap();
    assert!(!prompt.contains("fresh top-level comment"));
    assert!(prompt.contains("fresh review body"));
    assert!(prompt.contains("fresh inline comment"));
    assert!(!prompt.contains("stale cached comment"));
    assert_eq!(
        persisted.steps[0]
            .work_guard
            .as_ref()
            .unwrap()
            .review_thread_ids,
        vec!["PRRT_fresh"]
    );
    assert_eq!(persisted.run.variant, "repair");

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn phase_1_external_guarded_push_satisfaction_resolves_exact_threads_and_replans_from_refreshed_details()
 {
    let temp = unique_temp_dir("prism-phase-1-external-guarded-push-test");
    let repo_root = temp.join("repo");
    let worktree = repo_root.join("feature");
    fs::create_dir_all(&worktree).unwrap();
    let gh_log = temp.join("gh.log");
    let resolved = temp.join("resolved");
    let gh = temp.join("gh");
    let git = temp.join("git");

    fs::write(
        &gh,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"thread=PRRT_guarded_1"*|*"thread=PRRT_guarded_2"*)
    touch '{}'
    echo '{{"data":{{"resolveReviewThread":{{"thread":{{"isResolved":true}}}}}}}}'
    ;;
  "pr view feature --json comments,reviews,files,statusCheckRollup")
    if [ -f '{}' ]; then
      echo '{{"comments":[],"reviews":[{{"id":"PRR_fresh","author":{{"login":"reviewer"}},"state":"APPROVED","body":"","submittedAt":"2026-07-13T12:01:00Z"}}],"files":[],"statusCheckRollup":{{"contexts":{{"nodes":[]}}}}}}'
    else
      echo '{{"comments":[],"reviews":[{{"id":"PRR_stale","author":{{"login":"reviewer"}},"state":"CHANGES_REQUESTED","body":"address guarded feedback","submittedAt":"2026-07-13T12:00:00Z"}}],"files":[],"statusCheckRollup":{{"contexts":{{"nodes":[]}}}}}}'
    fi
    ;;
  api\ graphql*)
    if [ -f '{}' ]; then
      echo '{{"data":{{"repository":{{"pullRequest":{{"reviewThreads":{{"nodes":[]}}}}}}}}}}'
    else
      echo '{{"data":{{"repository":{{"pullRequest":{{"reviewThreads":{{"nodes":[{{"id":"PRRT_guarded_1","isResolved":false,"comments":{{"nodes":[{{"id":"PRRC_guarded","path":"src/lib.rs","originalLine":12,"body":"address guarded feedback","createdAt":"2026-07-13T12:00:30Z","author":{{"login":"reviewer"}}}}]}}}}]}}}}}}}}}}'
    fi
    ;;
  *)
    if [ -f '{}' ]; then decision=APPROVED; else decision=CHANGES_REQUESTED; fi
    echo "{{\"number\":42,\"title\":\"Guarded repair\",\"body\":\"\",\"url\":\"https://github.com/example/repo/pull/42\",\"state\":\"OPEN\",\"reviewDecision\":\"$decision\",\"reviewRequests\":{{\"nodes\":[]}},\"headRefName\":\"feature\",\"baseRefName\":\"main\",\"headRefOid\":\"repair-sha\",\"updatedAt\":\"2026-07-13T12:02:00Z\",\"comments\":{{\"totalCount\":0}},\"statusCheckRollup\":{{\"contexts\":{{\"nodes\":[]}}}},\"mergeStateStatus\":\"CLEAN\",\"isDraft\":false}}"
    ;;
esac
"#,
            gh_log.display(),
            resolved.display(),
            resolved.display(),
            resolved.display(),
            resolved.display(),
        ),
    )
    .unwrap();
    fs::write(
        &git,
        r#"#!/bin/sh
case "$*" in
  *"remote get-url origin"*) echo "https://github.com/example/repo.git" ;;
  *"rev-parse HEAD"*) echo "repair-sha" ;;
  *"refs/remotes/origin/feature"*) echo "repair-sha" ;;
  *"refs/remotes/origin/main"*) echo "base-sha" ;;
  *"status --porcelain"*) exit 0 ;;
  *"fetch origin"*) exit 0 ;;
  *) exit 0 ;;
esac
"#,
    )
    .unwrap();
    for executable in [&gh, &git] {
        let mut permissions = fs::metadata(executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(executable, permissions).unwrap();
    }

    let mut config = test_config();
    config.default_base = Some("main".to_string());
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let repo = Repository::with_config_dir_for_test(repo_root.clone(), temp.join("config"));
    let mut persisted = AutoLaunch::new(&repo_root, &worktree, "feature", "Guarded repair")
        .unwrap()
        .create_run();
    persisted.run.pr_number = Some(42);
    persisted.run.pending_push = Some(PendingPushGuard {
        repair_kind: RepairKind::Review,
        commit_sha: "repair-sha".to_string(),
        expected_local_head_sha: "repair-sha".to_string(),
        expected_remote_head_sha: Some("old-remote-sha".to_string()),
        pr_number: Some(42),
        expected_pr_head_sha: Some("old-remote-sha".to_string()),
        expected_base_sha: Some("base-sha".to_string()),
        guarded_review_thread_ids: vec!["PRRT_guarded_1".to_string(), "PRRT_guarded_2".to_string()],
    });
    crate::observability::with_writable_db(&repo, |conn| save_auto_run(conn, &mut persisted))
        .unwrap();

    let mut session = test_session(worktree.clone(), "feature");
    session.pr = PrCache::observed(
        PrSummary {
            number: 42,
            title: "Guarded repair".to_string(),
            body: String::new(),
            url: "https://github.com/example/repo/pull/42".to_string(),
            state: "OPEN".to_string(),
            review_decision: "CHANGES_REQUESTED".to_string(),
            requested_reviewers: Vec::new(),
            head_ref: "feature".to_string(),
            base_ref: "main".to_string(),
            head_sha: "repair-sha".to_string(),
            updated_at: "2026-07-13T12:00:00Z".to_string(),
            check_status: "passed".to_string(),
            merge_state_status: "CLEAN".to_string(),
            comment_count: 1,
            merged: false,
            draft: false,
        },
        Some(PrDetails {
            review_comments: vec![crate::github::PrReviewComment {
                thread_id: "PRRT_guarded_1".to_string(),
                body: "address guarded feedback".to_string(),
                resolved: false,
                ..crate::github::PrReviewComment::default()
            }],
            ..PrDetails::default()
        }),
    );
    let mut tui = Tui::new_single(repo.clone(), config.clone(), vec![session]);
    tui.active_auto_runs
        .insert(worktree, persisted.run.id.clone());

    let progress = crate::observability::with_writable_db(&repo, |conn| {
        progress_pending_push(
            conn,
            &repo,
            &config,
            &mut persisted,
            &mut tui.sessions[0].pr,
            || Ok(()),
        )
    })
    .unwrap();
    assert_eq!(progress, GuardedPushProgress::AlreadySatisfied);

    let commands = fs::read_to_string(&gh_log).unwrap();
    assert_eq!(commands.matches("thread=PRRT_guarded_1").count(), 1);
    assert_eq!(
        commands.matches("thread=PRRT_guarded_2").count(),
        0,
        "an obligation already absent from an authoritative observation needs no mutation"
    );
    assert!(!commands.contains("thread=PRRT_unguarded"));
    let details = tui.sessions[0].pr.details().unwrap();
    assert!(details.review_comments.is_empty());
    assert!(
        details
            .reviews
            .iter()
            .any(|review| review.state == "APPROVED")
    );
    let reloaded = crate::observability::with_writable_db(&repo, |conn| {
        load_auto_run(conn, &persisted.run.id)
    })
    .unwrap()
    .unwrap();
    assert!(reloaded.run.pending_push.is_none());
    assert_eq!(
        reloaded.run.stabilization_blocker,
        Some(StabilizationBlocker::ReadyForManualMerge)
    );
    assert_eq!(
        reloaded.run.stabilization_next_work,
        Some(StabilizationWorkKind::MarkReadyForManualMerge)
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn partial_thread_resolution_persists_remainder_and_retries_after_reopen() {
    let temp = unique_temp_dir("prism-partial-thread-resolution-restart-test");
    let repo_root = temp.join("repo");
    let worktree = repo_root.join("feature");
    fs::create_dir_all(&worktree).unwrap();
    let gh_log = temp.join("gh.log");
    let first_resolved = temp.join("first-resolved");
    let second_resolved = temp.join("second-resolved");
    let allow_second = temp.join("allow-second");
    let allow_refresh = temp.join("allow-refresh");
    let gh = temp.join("gh");
    let git = temp.join("git");
    fs::write(
        &gh,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"thread=PRRT_1"*)
    touch '{}'
    echo '{{"data":{{"resolveReviewThread":{{"thread":{{"isResolved":true}}}}}}}}'
    ;;
  *"thread=PRRT_2"*)
    if [ ! -f '{}' ]; then echo 'transient mutation failure' >&2; exit 1; fi
    touch '{}'
    echo '{{"data":{{"resolveReviewThread":{{"thread":{{"isResolved":true}}}}}}}}'
    ;;
  "pr view feature --json comments,reviews,files,statusCheckRollup")
    echo '{{"comments":[],"reviews":[],"files":[],"statusCheckRollup":{{"contexts":{{"nodes":[]}}}}}}'
    ;;
  api\ graphql*)
    if [ ! -f '{}' ]; then
      echo '{{"data":{{"repository":{{"pullRequest":{{"reviewThreads":{{"nodes":[{{"id":"PRRT_1","isResolved":false,"comments":{{"nodes":[{{"id":"C1","path":"src/lib.rs","body":"one","createdAt":"2026-07-13T12:00:00Z","author":{{"login":"reviewer"}}}}]}}}},{{"id":"PRRT_2","isResolved":false,"comments":{{"nodes":[{{"id":"C2","path":"src/lib.rs","body":"two","createdAt":"2026-07-13T12:00:01Z","author":{{"login":"reviewer"}}}}]}}}}]}}}}}}}}}}'
    elif [ ! -f '{}' ]; then
      echo '{{"data":{{"repository":{{"pullRequest":{{"reviewThreads":{{"nodes":[{{"id":"PRRT_2","isResolved":false,"comments":{{"nodes":[{{"id":"C2","path":"src/lib.rs","body":"two","createdAt":"2026-07-13T12:00:01Z","author":{{"login":"reviewer"}}}}]}}}}]}}}}}}}}}}'
    else
      echo '{{"data":{{"repository":{{"pullRequest":{{"reviewThreads":{{"nodes":[]}}}}}}}}}}'
    fi
    ;;
  pr\ view\ feature\ --json\ number,title,*)
    if [ -f '{}' ] && [ ! -f '{}' ]; then
      echo 'transient refresh failure' >&2
      exit 1
    fi
    echo '{{"number":42,"title":"Repair","body":"","url":"https://github.com/example/repo/pull/42","state":"OPEN","reviewDecision":"","reviewRequests":{{"nodes":[]}},"headRefName":"feature","baseRefName":"main","headRefOid":"repair-sha","updatedAt":"2026-07-13T12:02:00Z","comments":{{"totalCount":0}},"statusCheckRollup":{{"contexts":{{"nodes":[]}}}},"mergeStateStatus":"CLEAN","isDraft":false}}'
    ;;
  "run list "*) echo '[]' ;;
  *)
    echo '{{"number":42,"title":"Repair","body":"","url":"https://github.com/example/repo/pull/42","state":"OPEN","reviewDecision":"","reviewRequests":{{"nodes":[]}},"headRefName":"feature","baseRefName":"main","headRefOid":"repair-sha","updatedAt":"2026-07-13T12:02:00Z","comments":{{"totalCount":0}},"statusCheckRollup":{{"contexts":{{"nodes":[]}}}},"mergeStateStatus":"CLEAN","isDraft":false}}'
    ;;
esac
"#,
            gh_log.display(),
            first_resolved.display(),
            allow_second.display(),
            second_resolved.display(),
            first_resolved.display(),
            second_resolved.display(),
            second_resolved.display(),
            allow_refresh.display(),
        ),
    )
    .unwrap();
    fs::write(
        &git,
        r#"#!/bin/sh
case "$*" in
  *"remote get-url origin"*) echo "https://github.com/example/repo.git" ;;
  *"rev-parse HEAD"*|*"refs/remotes/origin/feature"*) echo "repair-sha" ;;
  *"refs/remotes/origin/main"*) echo "base-sha" ;;
  *"status --porcelain"*|*"fetch origin"*) exit 0 ;;
  *) exit 0 ;;
esac
"#,
    )
    .unwrap();
    for executable in [&gh, &git] {
        let mut permissions = fs::metadata(executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(executable, permissions).unwrap();
    }
    let mut config = test_config();
    config.default_base = Some("main".to_string());
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let repo = Repository::with_config_dir_for_test(repo_root.clone(), temp.join("config"));
    let mut persisted = AutoLaunch::new(&repo_root, &worktree, "feature", "Repair")
        .unwrap()
        .create_run();
    persisted.run.pr_number = Some(42);
    persisted.run.pending_push = Some(PendingPushGuard {
        repair_kind: RepairKind::Review,
        commit_sha: "repair-sha".to_string(),
        expected_local_head_sha: "repair-sha".to_string(),
        expected_remote_head_sha: Some("old-sha".to_string()),
        pr_number: Some(42),
        expected_pr_head_sha: Some("old-sha".to_string()),
        expected_base_sha: Some("base-sha".to_string()),
        guarded_review_thread_ids: vec!["PRRT_1".to_string(), "PRRT_2".to_string()],
    });
    crate::observability::with_writable_db(&repo, |conn| save_auto_run(conn, &mut persisted))
        .unwrap();

    let mut cache = PrCache::default();
    let first = crate::observability::with_writable_db(&repo, |conn| {
        progress_pending_push(conn, &repo, &config, &mut persisted, &mut cache, || Ok(()))
    });
    assert!(first.is_err());

    let mut reopened = crate::observability::with_writable_db(&repo, |conn| {
        load_auto_run(conn, &persisted.run.id)
    })
    .unwrap()
    .unwrap();
    assert_eq!(
        reopened
            .run
            .pending_push
            .as_ref()
            .unwrap()
            .guarded_review_thread_ids,
        vec!["PRRT_2"]
    );
    fs::write(&allow_second, "retry").unwrap();
    let mut cache = PrCache::default();
    let refresh_failure = crate::observability::with_writable_db(&repo, |conn| {
        progress_pending_push(conn, &repo, &config, &mut reopened, &mut cache, || Ok(()))
    });
    assert!(refresh_failure.is_err());
    let mut reopened =
        crate::observability::with_writable_db(&repo, |conn| load_auto_run(conn, &reopened.run.id))
            .unwrap()
            .unwrap();
    assert!(
        reopened
            .run
            .pending_push
            .as_ref()
            .unwrap()
            .guarded_review_thread_ids
            .is_empty()
    );
    fs::write(&allow_refresh, "retry").unwrap();
    let mut cache = PrCache::default();
    crate::observability::with_writable_db(&repo, |conn| {
        progress_pending_push(conn, &repo, &config, &mut reopened, &mut cache, || Ok(()))
    })
    .unwrap();
    let final_run =
        crate::observability::with_writable_db(&repo, |conn| load_auto_run(conn, &reopened.run.id))
            .unwrap()
            .unwrap();
    assert!(final_run.run.pending_push.is_none());
    let commands = fs::read_to_string(&gh_log).unwrap();
    assert_eq!(commands.matches("thread=PRRT_1").count(), 1);
    assert_eq!(commands.matches("thread=PRRT_2").count(), 2);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn phase_1_failed_details_refresh_does_not_start_repair_from_stale_thread_ids() {
    let temp = unique_temp_dir("prism-phase-1-stale-review-authorization-test");
    let worktree = temp.join("worktree");
    fs::create_dir_all(&worktree).unwrap();
    let gh = temp.join("gh");
    fs::write(
        &gh,
        "#!/bin/sh\necho 'review details unavailable' >&2\nexit 1\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&gh).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&gh, permissions).unwrap();

    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(worktree, "feature");
    session.pr = PrCache::observed(
        phase_1_pr_summary("old-head"),
        Some(PrDetails {
            review_comments: vec![crate::github::PrReviewComment {
                thread_id: "PRRT_stale".to_string(),
                body: "stale review feedback".to_string(),
                resolved: false,
                ..crate::github::PrReviewComment::default()
            }],
            ..PrDetails::default()
        }),
    );
    let mut tui = Tui::new_single(repo, config, vec![session]);
    tui.prompt_submissions = Some(Vec::new());

    let result = tui.start_review_fix_for_test();

    assert!(
        result.is_err(),
        "forced review repair must report refresh failure"
    );
    assert!(
        tui.active_auto_runs.is_empty(),
        "stale thread IDs must not authorize repair work"
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn ci_fix_sends_prompt_to_agent_session() {
    let temp = unique_temp_dir("prism-ci-fix-send-test");
    let repo_root = temp.join("repo");
    let worktree = repo_root.join("feature");
    fs::create_dir_all(&worktree).unwrap();
    let gh = temp.join("gh");
    let git = temp.join("git");

    fs::write(
        &gh,
        r#"#!/bin/sh
case "$*" in
  "pr view feature --json comments,reviews,files,statusCheckRollup")
cat <<'JSON'
{"comments":[],"reviews":[],"files":[],"statusCheckRollup":{"contexts":{"nodes":[{"name":"test","status":"COMPLETED","conclusion":"FAILURE"}]}}}
JSON
;;
  "run list "*)
printf '[]\n'
;;
  api\ graphql*)
printf '%s\n' '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[]}}}}}'
;;
  *)
cat <<'JSON'
{"number":42,"title":"CI refresh","body":"","url":"https://github.com/example/repo/pull/42","state":"OPEN","reviewDecision":"","reviewRequests":{"nodes":[]},"headRefName":"feature","baseRefName":"main","headRefOid":"abc123","updatedAt":"2026-06-14T12:02:00Z","comments":{"totalCount":0},"statusCheckRollup":{"contexts":{"nodes":[{"name":"test","status":"COMPLETED","conclusion":"FAILURE"}]}},"isDraft":false}
JSON
;;
esac
"#,
    )
    .unwrap();
    fs::write(
        &git,
        r#"#!/bin/sh
case "$*" in
  *"remote get-url origin"*)
echo "https://github.com/example/repo.git"
;;
esac
"#,
    )
    .unwrap();
    for executable in [&gh, &git] {
        let mut permissions = fs::metadata(executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(executable, permissions).unwrap();
    }

    let mut config = test_config();
    config.default_base = Some("main".to_string());
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let repo = Repository::with_config_dir_for_test(repo_root.clone(), temp.join("config"));
    let session = test_session(worktree, "feature");
    let mut tui = Tui::new_single(repo, config, vec![session]);
    tui.prompt_submissions = Some(Vec::new());

    tui.start_ci_fix_for_test().unwrap();

    let run_id = tui
        .active_auto_runs
        .get(&tui.sessions[0].path)
        .unwrap()
        .clone();
    let persisted =
        crate::observability::with_writable_db(&tui.repo, |conn| load_auto_run(conn, &run_id))
            .unwrap()
            .unwrap();
    assert_eq!(persisted.steps.len(), 1);
    assert_eq!(persisted.steps[0].step_key, AutoStepKey::FixCi);
    let prompt = persisted.steps[0].reason.as_deref().unwrap();
    assert!(prompt.contains("Here are CI failures on PR 42."));
    assert!(prompt.contains("- test"));
    assert_eq!(persisted.run.variant, "repair");

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn pr_summary_or_error_returns_refresh_error() {
    let cache = PrCache::stale_for_test(None, "gh pr view: authentication failed");

    let error = pr_summary_or_error(&cache).unwrap_err();

    assert_eq!(error, "gh pr view: authentication failed");
}

#[test]
fn default_branch_status_replaces_stale_behind_count() {
    assert_eq!(status_label_with_behind("clean", 2), "behind 2");
    assert_eq!(status_label_with_behind("dirty 1 behind 9", 0), "dirty 1");
    assert_eq!(
        status_label_with_behind("dirty 1 ahead 3 behind 9", 2),
        "dirty 1 ahead 3 behind 2"
    );
}

#[test]
fn archived_picker_reports_overflow_instead_of_truncating() {
    assert!(archived_picker_overflow_message(35, 35).is_none());

    let message = archived_picker_overflow_message(36, 35).unwrap();

    assert!(message.contains("36 archived worktrees"));
    assert!(message.contains("picker limit 35"));
}

#[test]
fn opencode_poll_does_not_mark_busy_session_done_before_completed_message() {
    let temp = unique_temp_dir("prism-opencode-status-order-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(temp.join("worktree"), "feature");
    session.agent_state = AgentState::Running;
    session.opencode_status = Some(test_opencode_status(OpencodeState::Busy));
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);

    tui.opencode_poll_tx
        .send(OpencodePollResult {
            key: OpencodePollKey::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]),
            started_at: Instant::now(),
            status: Ok(test_opencode_status(OpencodeState::Idle)),
        })
        .unwrap();

    tui.poll_opencode_status();
    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().state,
        OpencodeState::Busy
    );
    assert_eq!(tui.sessions[0].agent_state, AgentState::Running);

    tui.opencode_event_tx
        .send(OpencodeEventResult {
            key: tui.sessions[0].identity_key(&tui.repos[0].identity),
            server_url: "http://127.0.0.1:41000".to_string(),
            event: Ok(parse_event_payload(
                r#"{"type":"message.updated","properties":{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"finish":"stop"}}}"#,
            )
            .unwrap()),
        })
        .unwrap();

    assert!(tui.poll_opencode_events());
    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().state,
        OpencodeState::Done
    );
    assert_eq!(tui.sessions[0].agent_state, AgentState::ExitedOk);

    tui.opencode_event_tx
        .send(OpencodeEventResult {
            key: tui.sessions[0].identity_key(&tui.repos[0].identity),
            server_url: "http://127.0.0.1:41000".to_string(),
            event: Ok(parse_event_payload(
                r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":"busy"}}"#,
            )
            .unwrap()),
        })
        .unwrap();
    assert!(tui.poll_opencode_events());

    let poll_started_at = Instant::now();
    tui.opencode_poll_tx
        .send(OpencodePollResult {
            key: OpencodePollKey::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]),
            started_at: poll_started_at,
            status: Ok(test_opencode_status(OpencodeState::Busy)),
        })
        .unwrap();
    tui.opencode_event_tx
        .send(OpencodeEventResult {
            key: tui.sessions[0].identity_key(&tui.repos[0].identity),
            server_url: "http://127.0.0.1:41000".to_string(),
            event: Ok(parse_event_payload(
                r#"{"type":"message.updated","properties":{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":3,"completed":4},"finish":"stop"}}}"#,
            )
            .unwrap()),
        })
        .unwrap();

    assert!(tui.poll_opencode_events());
    tui.poll_opencode_status();
    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().state,
        OpencodeState::Done
    );
    assert_eq!(tui.sessions[0].agent_state, AgentState::ExitedOk);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn opencode_poll_does_not_mark_reconnected_running_session_done_before_completed_message() {
    let temp = unique_temp_dir("prism-opencode-reconnected-status-order-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(temp.join("worktree"), "feature");
    session.agent_state = AgentState::Running;
    session.opencode_status = Some(test_opencode_status(OpencodeState::Unknown));
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);

    tui.opencode_poll_tx
        .send(OpencodePollResult {
            key: OpencodePollKey::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]),
            started_at: Instant::now(),
            status: Ok(test_opencode_status(OpencodeState::Idle)),
        })
        .unwrap();

    tui.poll_opencode_status();
    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().state,
        OpencodeState::Busy
    );
    assert_eq!(tui.sessions[0].agent_state, AgentState::Running);

    tui.opencode_event_tx
        .send(OpencodeEventResult {
            key: tui.sessions[0].identity_key(&tui.repos[0].identity),
            server_url: "http://127.0.0.1:41000".to_string(),
            event: Ok(parse_event_payload(
                r#"{"type":"message.updated","properties":{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"error":{"name":"MessageAbortedError"}}}}"#,
            )
            .unwrap()),
        })
        .unwrap();

    assert!(tui.poll_opencode_events());
    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().state,
        OpencodeState::Done
    );
    assert_eq!(tui.sessions[0].agent_state, AgentState::ExitedOk);
    assert_eq!(
        tui.sessions[0]
            .opencode_status
            .as_ref()
            .unwrap()
            .detail
            .as_deref(),
        Some("MessageAbortedError")
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn opencode_permission_event_marks_session_as_needing_input() {
    let temp = unique_temp_dir("prism-opencode-permission-status-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(temp.join("worktree"), "feature");
    session.agent_state = AgentState::Running;
    session.opencode_status = Some(test_opencode_status(OpencodeState::Busy));
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);

    tui.opencode_event_tx
        .send(OpencodeEventResult {
            key: tui.sessions[0].identity_key(&tui.repos[0].identity),
            server_url: "http://127.0.0.1:41000".to_string(),
            event: Ok(parse_event_payload(
                r#"{"type":"permission.asked","properties":{"sessionID":"ses_1","permission":"bash"}}"#,
            )
            .unwrap()),
        })
        .unwrap();

    assert!(tui.poll_opencode_events());
    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().state,
        OpencodeState::NeedsInput
    );
    assert_eq!(tui.sessions[0].agent_state, AgentState::NeedsInput);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn opencode_prompt_submission_clears_done_status_immediately() {
    let temp = unique_temp_dir("prism-opencode-prompt-status-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    config.default_agent = "opencode".to_string();
    let mut session = test_session(temp.join("worktree"), "feature");
    session.agent_state = AgentState::ExitedOk;
    session.opencode_status = Some(test_opencode_status(OpencodeState::Done));
    session.opencode_status.as_mut().unwrap().detail = Some("MessageAbortedError".to_string());
    let mut tui = Tui::new_single(repo, config, vec![session]);
    tui.prompt_submissions = Some(Vec::new());

    tui.paste_prompt_into_tmux_agent(0, "try again", false)
        .unwrap();

    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().state,
        OpencodeState::Busy
    );
    assert_eq!(
        tui.sessions[0].opencode_status.as_ref().unwrap().detail,
        None
    );
    assert_eq!(tui.sessions[0].agent_state, AgentState::Running);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn automatic_pr_polling_does_not_block_input_loop() {
    let temp = unique_temp_dir("prism-pr-poll-test");
    fs::create_dir_all(&temp).unwrap();
    let gh = temp.join("gh");
    fs::write(
        &gh,
        r#"#!/bin/sh
sleep 1
echo 'no pull requests found' >&2
exit 1
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&gh).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&gh, permissions).unwrap();

    let mut config = test_config();
    config
        .tools
        .insert("gh".to_string(), gh.display().to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "feature");
    let mut tui = Tui::new_single(repo, config, vec![session]);

    let started = Instant::now();
    let changed = tui.poll_pull_requests(false);

    assert!(!changed);
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "automatic PR polling blocked for {:?}",
        started.elapsed()
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn delete_session_does_not_block_input_loop() {
    let temp = unique_temp_dir("prism-delete-nonblocking-test");
    fs::create_dir_all(&temp).unwrap();
    let git_log = temp.join("git.log");
    let git = temp.join("git");
    let tmux = temp.join("tmux");
    fs::write(
        &git,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"rev-parse --verify refs/heads/feature/delete"*)
    echo branch-oid
    exit 0
    ;;
  *"worktree remove --force"*)
    sleep 1
    exit 0
    ;;
  *"worktree list --porcelain"*)
    exit 0
    ;;
  *"branch -D feature/delete"*)
    exit 0
    ;;
esac
exit 0
"#,
            git_log.display()
        ),
    )
    .unwrap();
    fs::write(
        &tmux,
        r#"#!/bin/sh
case "$1" in
  list-sessions|kill-session)
    exit 0
    ;;
esac
exit 0
"#,
    )
    .unwrap();
    for executable in [&git, &tmux] {
        let mut permissions = fs::metadata(executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(executable, permissions).unwrap();
    }

    let mut config = test_config();
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    config
        .tools
        .insert("tmux".to_string(), tmux.display().to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "feature/delete");
    let mut tui = Tui::new_single(repo, config, vec![session]);

    let started = Instant::now();
    tui.start_delete_session_for_test().unwrap();

    assert!(
        started.elapsed() < Duration::from_millis(250),
        "delete blocked input loop for {:?}",
        started.elapsed()
    );
    assert_eq!(tui.sessions.len(), 1);
    assert!(tui.sessions[0].hidden);
    assert!(tui.visible_session_indices().is_empty());
    assert_eq!(tui.delete_sessions_in_flight.len(), 1);

    let wait_started = Instant::now();
    while !tui.delete_sessions_in_flight.is_empty()
        && wait_started.elapsed() < Duration::from_secs(3)
    {
        tui.poll_delete_sessions();
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(tui.delete_sessions_in_flight.is_empty());
    assert!(tui.sessions.is_empty());
    let commands = fs::read_to_string(&git_log).unwrap();
    assert!(commands.contains("worktree remove --force"));
    assert!(commands.contains("branch -D feature/delete"));

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn failed_async_delete_restores_hidden_worktree() {
    let temp = unique_temp_dir("prism-delete-restore-test");
    fs::create_dir_all(&temp).unwrap();
    let worktree = temp.join("worktree");
    fs::create_dir_all(&worktree).unwrap();
    let git = temp.join("git");
    let tmux = temp.join("tmux");
    fs::write(
        &git,
        format!(
            r#"#!/bin/sh
case "$*" in
  *"rev-parse --verify refs/heads/feature/delete"*)
    echo branch-oid
    exit 0
    ;;
  *"worktree remove --force"*)
    exit 1
    ;;
  *"worktree list --porcelain"*)
    printf 'worktree {}\nHEAD abc\nbranch refs/heads/feature/delete\n\n'
    exit 0
    ;;
esac
exit 0
"#,
            worktree.display()
        ),
    )
    .unwrap();
    fs::write(
        &tmux,
        r#"#!/bin/sh
case "$1" in
  list-sessions|kill-session)
    exit 0
    ;;
esac
exit 0
"#,
    )
    .unwrap();
    for executable in [&git, &tmux] {
        let mut permissions = fs::metadata(executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(executable, permissions).unwrap();
    }

    let mut config = test_config();
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    config
        .tools
        .insert("tmux".to_string(), tmux.display().to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(worktree, "feature/delete");
    let mut tui = Tui::new_single(repo, config, vec![session]);

    tui.start_delete_session_for_test().unwrap();

    assert!(tui.sessions[0].hidden);
    assert!(tui.visible_session_indices().is_empty());

    let wait_started = Instant::now();
    while !tui.delete_sessions_in_flight.is_empty()
        && wait_started.elapsed() < Duration::from_secs(3)
    {
        tui.poll_delete_sessions();
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(tui.delete_sessions_in_flight.is_empty());
    assert_eq!(tui.sessions.len(), 1);
    assert!(!tui.sessions[0].hidden);
    assert_eq!(tui.visible_session_indices(), vec![0]);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn phase_1_branch_delete_failure_reconciles_without_vanished_worktree_path() {
    let temp = unique_temp_dir("prism-phase-1-delete-reconcile-test");
    fs::create_dir_all(&temp).unwrap();
    let worktree = temp.join("worktree");
    fs::create_dir_all(&worktree).unwrap();
    let git = temp.join("git");
    let tmux = temp.join("tmux");
    fs::write(
        &git,
        r#"#!/bin/sh
case "$*" in
  *"rev-parse --verify refs/heads/feature/delete"*) echo branch-oid; exit 0 ;;
  *"worktree remove --force"*) exit 0 ;;
  *"branch -D feature/delete"*) exit 1 ;;
  *"worktree list --porcelain"*) exit 0 ;;
esac
exit 0
"#,
    )
    .unwrap();
    fs::write(
        &tmux,
        r#"#!/bin/sh
case "$1" in
  list-sessions|kill-session) exit 0 ;;
esac
exit 0
"#,
    )
    .unwrap();
    for executable in [&git, &tmux] {
        let mut permissions = fs::metadata(executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(executable, permissions).unwrap();
    }

    let mut config = test_config();
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    config
        .tools
        .insert("tmux".to_string(), tmux.display().to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(worktree.clone(), "feature/delete");
    let mut tui = Tui::new_single(repo, config, vec![session]);

    tui.start_delete_session_for_test().unwrap();
    let wait_started = Instant::now();
    while !tui.delete_sessions_in_flight.is_empty() {
        tui.poll_delete_sessions();
        assert!(
            tui.delete_sessions_in_flight.is_empty()
                || wait_started.elapsed() < Duration::from_secs(10),
            "delete did not finish within 10 seconds"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(tui.delete_sessions_in_flight.is_empty());
    assert!(tui.sessions.iter().all(|session| session.path != worktree));
    assert!(tui.visible_session_indices().is_empty());

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn phase_1_missing_github_remote_clears_live_and_persisted_pr_cache_state() {
    let temp = unique_temp_dir("prism-phase-1-removed-remote-poll-test");
    fs::create_dir_all(&temp).unwrap();
    let git = temp.join("git");
    fs::write(&git, "#!/bin/sh\necho 'origin is missing' >&2\nexit 2\n").unwrap();
    let mut permissions = fs::metadata(&git).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&git, permissions).unwrap();

    let mut config = test_config();
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let summary = phase_1_pr_summary("old-head");
    let cache = PrCache::observed(
        summary,
        Some(PrDetails {
            files: vec!["src/stale.rs".to_string()],
            ..PrDetails::default()
        }),
    );
    crate::github::save_pr_cache(&repo, "feature", &cache).unwrap();
    crate::github::save_pr_details_cache(&repo, "feature", cache.details().unwrap()).unwrap();
    let mut session = test_session(temp.join("worktree"), "feature");
    session.pr = cache;
    session.unseen_comments = true;
    let mut tui = Tui::new_single(repo.clone(), config, vec![session]);

    assert!(tui.poll_pull_requests(true));

    assert!(tui.sessions[0].pr.summary().is_none());
    assert!(tui.sessions[0].pr.details().is_none());
    assert!(!tui.sessions[0].unseen_comments);
    let persisted = crate::github::load_pr_cache(&repo, "feature");
    assert!(persisted.summary().is_none());
    assert!(persisted.details().is_none());

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn missing_github_remote_clears_hidden_non_pollable_pr_cache_state() {
    let temp = unique_temp_dir("prism-removed-remote-hidden-cache-test");
    fs::create_dir_all(&temp).unwrap();
    let git = temp.join("git");
    fs::write(&git, "#!/bin/sh\nexit 2\n").unwrap();
    let mut permissions = fs::metadata(&git).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&git, permissions).unwrap();
    let mut config = test_config();
    config
        .tools
        .insert("git".to_string(), git.display().to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let cache = PrCache::observed(phase_1_pr_summary("old-head"), None);
    crate::github::save_pr_cache(&repo, "feature", &cache).unwrap();
    let mut session = test_session(temp.join("worktree"), "feature");
    session.hidden = true;
    session.pr = cache;
    let mut tui = Tui::new_single(repo.clone(), config, vec![session]);

    assert!(tui.poll_pull_requests(true));

    assert!(tui.sessions[0].pr.summary().is_none());
    assert!(
        crate::github::load_pr_cache(&repo, "feature")
            .summary()
            .is_none()
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn default_branch_does_not_start_pr_polling() {
    let temp = unique_temp_dir("prism-default-branch-pr-poll-test");
    fs::create_dir_all(&temp).unwrap();

    let mut config = test_config();
    config.default_base = Some("main".to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "main");
    let mut tui = Tui::new_single(repo, config, vec![session]);

    let changed = tui.poll_pull_requests(false);

    assert!(!changed);
    assert!(!tui.repos[0].pr_summary_poll_in_flight);
    assert!(tui.pr_polls_in_flight.is_empty());

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn tmux_agent_warmup_does_not_block_startup() {
    let temp = unique_temp_dir("prism-tmux-warmup-test");
    fs::create_dir_all(&temp).unwrap();
    let state = temp.join("tmux-state");
    let release = temp.join("tmux-release");
    let timed_out = temp.join("tmux-timed-out");
    let tmux = temp.join("tmux");
    fs::write(
        &tmux,
        format!(
            r#"#!/bin/sh
state="$(cat '{}' 2>/dev/null || echo missing)"
case "$1" in
  has-session)
attempts=0
while [ ! -f '{}' ]; do
  attempts=$((attempts + 1))
  if [ "$attempts" -ge 100 ]; then
    touch '{}'
    break
  fi
  sleep 0.01
done
[ "$state" = exists ]
exit $?
;;
  new-session)
echo exists > '{}'
exit 0
;;
  set-option)
exit 0
;;
  display-message)
echo opencode
exit 0
;;
esac
exit 0
"#,
            state.display(),
            release.display(),
            timed_out.display(),
            state.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&tmux).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&tmux, permissions).unwrap();

    let mut config = test_config();
    config.default_agent = "custom".to_string();
    config
        .agent_commands
        .insert("custom".to_string(), "opencode".to_string());
    config
        .tools
        .insert("tmux".to_string(), tmux.display().to_string());
    config
        .tools
        .insert("opencode".to_string(), "opencode".to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "feature");
    let mut tui = Tui::new_single(repo, config, vec![session]);

    tui.start_tmux_agent_warmup();

    assert!(!timed_out.exists(), "tmux warm-up blocked startup");
    assert_eq!(tui.tmux_warmups_in_flight.len(), 1);
    fs::write(&release, "continue").unwrap();

    let wait_started = Instant::now();
    while !tui.tmux_warmups_in_flight.is_empty() {
        tui.poll_tmux_agent_warmup();
        assert!(
            tui.tmux_warmups_in_flight.is_empty()
                || wait_started.elapsed() < Duration::from_secs(10),
            "tmux warm-up did not finish within 10 seconds"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(tui.tmux_warmups_in_flight.is_empty());

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn attach_waits_for_selected_tmux_warmup() {
    let temp = unique_temp_dir("prism-tmux-attach-wait-test");
    fs::create_dir_all(&temp).unwrap();
    let tmux = temp.join("tmux");
    fs::write(
        &tmux,
        r#"#!/bin/sh
case "$1" in
  has-session|set-option|attach-session)
exit 0
;;
  display-message)
echo opencode
exit 0
;;
esac
exit 0
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&tmux).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&tmux, permissions).unwrap();

    let mut config = test_config();
    config.default_agent = "custom".to_string();
    config
        .agent_commands
        .insert("custom".to_string(), "opencode".to_string());
    config
        .tools
        .insert("tmux".to_string(), tmux.display().to_string());
    config
        .tools
        .insert("opencode".to_string(), "opencode".to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "feature");
    let mut tui = Tui::new_single(repo, config, vec![session]);
    let key = AgentSessionWarmupKey::new(
        AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]),
        0,
    );
    tui.tmux_warmups_in_flight.insert(key.clone());
    let tx = tui.tmux_warmup_tx.clone();

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        let _ = tx.send(AgentSessionWarmupResult {
            key,
            running: Some(true),
            error: None,
        });
    });

    let started = Instant::now();
    tui.attach_selected_tmux_session().unwrap();

    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "attach did not wait for selected warm-up"
    );
    let wait_started = Instant::now();
    while !tui.tmux_warmups_in_flight.is_empty() && wait_started.elapsed() < Duration::from_secs(3)
    {
        tui.poll_tmux_agent_warmup();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(tui.tmux_warmups_in_flight.is_empty());

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn prompt_paste_targets_tmux_agent_session() {
    let temp = unique_temp_dir("prism-tmux-prompt-paste-test");
    fs::create_dir_all(&temp).unwrap();
    let log = temp.join("tmux.log");
    let prompt_file = temp.join("prompt.txt");
    let tmux = temp.join("tmux");
    fs::write(
        &tmux,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  has-session|set-option|move-window|rename-window|new-window)
exit 0
;;
  list-windows)
exit 0
;;
  display-message)
echo opencode
exit 0
;;
  capture-pane)
echo 'Ask anything'
exit 0
;;
  load-buffer)
cat > '{}'
exit 0
;;
  paste-buffer)
exit 0
;;
esac
exit 1
"#,
            log.display(),
            prompt_file.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&tmux).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&tmux, permissions).unwrap();

    let mut config = test_config();
    config.default_agent = "custom".to_string();
    config
        .agent_commands
        .insert("custom".to_string(), "opencode".to_string());
    config
        .tools
        .insert("tmux".to_string(), tmux.display().to_string());
    config
        .tools
        .insert("opencode".to_string(), "opencode".to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "feature");
    let mut tui = Tui::new_single(repo, config, vec![session]);

    tui.paste_prompt_into_tmux_agent(0, "build the thing", false)
        .unwrap();

    assert_eq!(fs::read_to_string(&prompt_file).unwrap(), "build the thing");
    assert_eq!(tui.sessions[0].agent_state, AgentState::Attached);
    let commands = fs::read_to_string(&log).unwrap();
    assert!(commands.contains("load-buffer -b"));
    assert!(commands.contains("paste-buffer -d -b"));

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn stale_tmux_warmup_result_does_not_update_current_generation() {
    let temp = unique_temp_dir("prism-tmux-stale-generation-test");
    fs::create_dir_all(&temp).unwrap();
    let mut config = test_config();
    config.default_agent = "opencode".to_string();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "feature");
    let mut tui = Tui::new_single(repo, config, vec![session]);
    let slot = AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
    let stale_key = AgentSessionWarmupKey::new(slot.clone(), 0);
    tui.tmux_generations.insert(slot, 1);

    let changed = tui.apply_tmux_warmup_result(AgentSessionWarmupResult {
        key: stale_key,
        running: Some(true),
        error: None,
    });

    assert!(!changed);
    assert_eq!(tui.sessions[0].agent_state, AgentState::Idle);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn worktrunk_columns_reject_deleted_and_recreated_session_result() {
    let temp = unique_temp_dir("prism-wt-recreated-session-result-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut session = test_session(temp.join("worktree"), "feature");
    session.incarnation = "old".to_string();
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    let stale_key = tui.sessions[0].identity_key(&tui.repos[0].identity);
    tui.sessions[0].incarnation = "new".to_string();
    let columns = BTreeMap::from([(
        stale_key,
        BTreeMap::from([("ci".to_string(), "passed".to_string())]),
    )]);

    tui.wt_poll_tx
        .send(WtPollResult {
            repository: tui.repos[0].identity.clone(),
            columns: Ok(columns),
        })
        .unwrap();

    assert!(!tui.poll_wt_columns());
    assert!(tui.sessions[0].wt_columns.is_empty());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn default_branch_result_is_rejected_after_default_branch_config_changes() {
    let temp = unique_temp_dir("prism-default-branch-config-result-test");
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let mut config = test_config();
    config.default_base = Some("main".to_string());
    let session = test_session(temp.join("worktree"), "main");
    let mut tui = Tui::new_single(repo, config, vec![session]);
    let key = tui.sessions[0].identity_key(&tui.repos[0].identity);
    tui.repos[0].config.default_base = Some("develop".to_string());

    tui.default_branch_poll_tx
        .send(DefaultBranchPollResult {
            key,
            status_label: Ok("behind 3".to_string()),
        })
        .unwrap();

    assert!(!tui.poll_default_branch_status());
    assert_eq!(tui.sessions[0].status_label, "clean");
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn attach_schedules_delayed_rewarm_after_return() {
    let temp = unique_temp_dir("prism-tmux-delayed-rewarm-test");
    fs::create_dir_all(&temp).unwrap();
    let log = temp.join("tmux.log");
    let count = temp.join("display-count");
    let tmux = temp.join("tmux");
    fs::write(
        &tmux,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  has-session|set-option|attach-session|kill-session|new-session)
exit 0
;;
  display-message)
count="$(cat '{}' 2>/dev/null || echo 0)"
count="$((count + 1))"
echo "$count" > '{}'
if [ "$count" -eq 1 ]; then
  echo opencode
else
  echo bash
fi
exit 0
;;
esac
exit 0
"#,
            log.display(),
            count.display(),
            count.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&tmux).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&tmux, permissions).unwrap();

    let mut config = test_config();
    config.default_agent = "custom".to_string();
    config
        .agent_commands
        .insert("custom".to_string(), "opencode".to_string());
    config
        .tools
        .insert("tmux".to_string(), tmux.display().to_string());
    config
        .tools
        .insert("opencode".to_string(), "opencode".to_string());
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(temp.join("worktree"), "feature");
    let mut tui = Tui::new_single(repo, config, vec![session]);
    tui.focused_panel = PanelFocus::Worktrees;
    tui.tmux_portal_size = Some((72, 18));

    tui.prepare_tmux_session_for_attach(0, (120, 39)).unwrap();
    tui.attach_tmux_session_for_index(0).unwrap();

    let wait_started = Instant::now();
    while !tui.tmux_warmups_in_flight.is_empty() && wait_started.elapsed() < Duration::from_secs(5)
    {
        tui.poll_tmux_agent_warmup();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(tui.tmux_warmups_in_flight.is_empty());
    let commands = fs::read_to_string(&log).unwrap();
    assert!(
        commands.find("resize-window -x 120 -y 39").unwrap()
            < commands.find("attach-session -t").unwrap(),
        "agent window should match the terminal before attach"
    );
    assert!(
        commands.find("attach-session -t").unwrap()
            < commands.rfind("resize-window -x 72 -y 18").unwrap(),
        "portal should resize the agent window immediately after detach"
    );
    assert!(commands.contains("kill-session -t"));
    assert!(commands.contains("new-session -d -s"));

    let _ = fs::remove_dir_all(temp);
}

fn test_session(path: PathBuf, branch: &str) -> Session {
    fs::create_dir_all(&path).unwrap();
    Session {
        repo_index: 0,
        repo_label: "repo".to_string(),
        repo_key: None,
        path: path.clone(),
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

fn test_opencode_status(state: OpencodeState) -> OpencodeStatus {
    OpencodeStatus {
        server_url: Some("http://127.0.0.1:41000".to_string()),
        session_id: Some("ses_1".to_string()),
        title: None,
        state,
        detail: None,
        latest_message: None,
        latest_user_message: None,
        recent_messages: Vec::new(),
        active_tool: None,
        todos: Vec::new(),
        last_updated_unix_ms: Some(1),
    }
}

fn test_config() -> Config {
    crate::test_support::test_config()
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
}

fn phase_1_pr_summary(head_sha: &str) -> PrSummary {
    PrSummary {
        number: 42,
        title: "Phase 1 safety".to_string(),
        body: String::new(),
        url: "https://github.com/example/repo/pull/42".to_string(),
        state: "OPEN".to_string(),
        review_decision: "CHANGES_REQUESTED".to_string(),
        requested_reviewers: Vec::new(),
        head_ref: "feature".to_string(),
        base_ref: "main".to_string(),
        head_sha: head_sha.to_string(),
        updated_at: "2026-07-13T12:00:00Z".to_string(),
        check_status: "passed".to_string(),
        merge_state_status: "CLEAN".to_string(),
        comment_count: 1,
        merged: false,
        draft: false,
    }
}
