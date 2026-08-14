use std::collections::BTreeMap;
use std::fs;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::remote::{PrCache, PrDetails, PrReview, PrReviewComment};
use crate::repo::Repository;

use super::super::{
    RemoteActionDelivery, RemoteActionValue, RemoteMutationTarget, Tui, TuiJobKey, TuiJobKind,
    TuiJobPayload, remote_action_abandon_requested, remote_action_timeout,
    remote_mutation_targets_overlap,
};
use super::support::{
    test_change_request_identity, test_config, test_pr_summary, test_session, test_tui,
    unique_temp_dir,
};

fn unknown_marker_target(id: &str) -> RemoteMutationTarget {
    RemoteMutationTarget::Unknown {
        marker_id: id.to_string(),
    }
}

fn wait_for_background(tui: &mut Tui) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while tui.background.has_jobs() {
        tui.route_tui_job_messages();
        assert!(Instant::now() < deadline, "background work did not finish");
        std::thread::sleep(Duration::from_millis(2));
    }
    while tui.route_tui_job_messages() > 0 {}
}

#[test]
fn coordinated_mutations_are_blocked_until_startup_markers_are_loaded() {
    let temp = unique_temp_dir("prism-tui-marker-startup-admission");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    let key = TuiJobKey::Worktree(tui.sessions[0].identity_key(&tui.repos[0].identity));
    let target = unknown_marker_target("startup-admission");

    assert!(tui.remote_action_reconciliation_blocked(&key, &target));
    wait_for_background(&mut tui);
    assert!(!tui.remote_action_reconciliation_blocked(&key, &target));

    drop(tui);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn corrupt_startup_marker_fails_closed_and_marks_sessions_untrusted() {
    let temp = unique_temp_dir("prism-tui-corrupt-startup-marker");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    crate::persistence::database::upsert_metadata(
        &crate::observability::db_path(&repo),
        super::super::REMOTE_MUTATION_RECONCILIATION_KEY,
        "not valid marker json",
    )
    .unwrap();
    let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    let key = TuiJobKey::Worktree(tui.sessions[0].identity_key(&tui.repos[0].identity));
    let target = unknown_marker_target("after-load-error");

    wait_for_background(&mut tui);

    assert_eq!(tui.background.marker_count(&temp), 1);
    assert!(tui.remote_action_reconciliation_blocked(&key, &target));
    assert!(tui.sessions[0].pr.trusted_summary().is_err());
    assert_eq!(tui.background.take_shutdown_errors().len(), 1);

    drop(tui);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn stale_marker_load_error_does_not_block_a_new_repository_incarnation() {
    let temp = unique_temp_dir("prism-tui-stale-marker-load-error");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut tui = Tui::new_single(repo.clone(), test_config(), vec![session]);
    wait_for_background(&mut tui);
    let stale_repository = tui.repos[0].identity.clone();
    let current_repository = crate::session::WorktreeRepositoryKey::new(temp.clone());
    tui.repos[0].identity = current_repository.clone();
    let key = TuiJobKey::Worktree(tui.sessions[0].identity_key(&current_repository));
    let target = unknown_marker_target("new-incarnation");

    tui.apply_loaded_remote_mutation_markers(
        super::super::remote_action::LoadedRemoteMutationMarkers {
            repositories: std::collections::BTreeSet::from([stale_repository.clone()]),
            markers: BTreeMap::new(),
            errors: vec![super::super::remote_action::RemoteMutationMarkerLoadError {
                repository: stale_repository,
                database_path: crate::observability::db_path(&repo),
                message: "stale load error".to_string(),
            }],
        },
    );

    assert!(tui.remote_action_reconciliation_blocked(&key, &target));
    wait_for_background(&mut tui);
    assert_eq!(tui.background.marker_count(&temp), 0);
    assert!(!tui.remote_action_reconciliation_blocked(&key, &target));
    assert!(tui.sessions[0].pr.trusted_summary().is_ok());
    assert!(tui.background.take_shutdown_errors().is_empty());

    drop(tui);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn startup_marker_load_survives_session_generation_change() {
    let temp = unique_temp_dir("prism-tui-marker-generation-race");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let database = crate::observability::db_path(&repo);
    let marker = super::super::remote_action::RemoteMutationReconciliationMarker {
        target: unknown_marker_target("startup-race"),
        ledger: None,
        database_path: std::path::PathBuf::new(),
        job_id: 7,
        reason: "uncertain".into(),
        recorded_unix_ms: 11,
    };
    crate::persistence::database::upsert_metadata(
        &database,
        super::super::REMOTE_MUTATION_RECONCILIATION_KEY,
        &serde_json::to_string(&vec![marker]).unwrap(),
    )
    .unwrap();

    let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    tui.session_inventory_generation += 1;
    wait_for_background(&mut tui);
    assert_eq!(tui.background.marker_count(&temp), 1);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn marker_persistence_remains_admitted_after_general_shutdown_cutoff() {
    let temp = unique_temp_dir("prism-tui-marker-after-cutoff");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut tui = Tui::new_single(repo.clone(), test_config(), vec![session]);
    wait_for_background(&mut tui);
    tui.background.begin_shutdown();
    tui.background.stop_admission_for_shutdown();
    let target = unknown_marker_target("after-cutoff");
    let key = TuiJobKey::Worktree(tui.sessions[0].identity_key(&tui.repos[0].identity));
    tui.record_remote_mutation_reconciliation(&key, 9, "late uncertainty", &target, None)
        .unwrap();
    wait_for_background(&mut tui);
    drop(tui);

    let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut restarted = Tui::new_single(repo, test_config(), vec![session]);
    wait_for_background(&mut restarted);
    assert_eq!(restarted.background.marker_count(&temp), 1);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn marker_writer_retries_repeated_start_failures_and_survives_restart() {
    let temp = unique_temp_dir("prism-tui-marker-spawn-retry");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut tui = Tui::new_single(repo.clone(), test_config(), vec![session]);
    wait_for_background(&mut tui);
    tui.background.fail_marker_writer_spawns(4);
    let target = unknown_marker_target("spawn-retry");
    let key = TuiJobKey::Worktree(tui.sessions[0].identity_key(&tui.repos[0].identity));
    tui.record_remote_mutation_reconciliation(&key, 13, "uncertain", &target, None)
        .unwrap();
    wait_for_background(&mut tui);
    drop(tui);

    let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut restarted = Tui::new_single(repo, test_config(), vec![session]);
    wait_for_background(&mut restarted);
    assert_eq!(restarted.background.marker_count(&temp), 1);
    assert!(restarted.background.take_shutdown_errors().is_empty());
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn marker_writer_retries_repeated_write_failures_and_survives_restart() {
    let temp = unique_temp_dir("prism-tui-marker-write-retry");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut tui = Tui::new_single(repo.clone(), test_config(), vec![session]);
    wait_for_background(&mut tui);
    tui.background.fail_marker_writes(4);
    let target = unknown_marker_target("write-retry");
    let key = TuiJobKey::Worktree(tui.sessions[0].identity_key(&tui.repos[0].identity));
    tui.record_remote_mutation_reconciliation(&key, 14, "uncertain", &target, None)
        .unwrap();
    wait_for_background(&mut tui);
    drop(tui);

    let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut restarted = Tui::new_single(repo, test_config(), vec![session]);
    wait_for_background(&mut restarted);
    assert_eq!(restarted.background.marker_count(&temp), 1);
    assert!(restarted.background.take_shutdown_errors().is_empty());
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn permanent_marker_write_failure_makes_shutdown_explicitly_unsuccessful() {
    let temp = unique_temp_dir("prism-tui-marker-permanent-failure");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    wait_for_background(&mut tui);
    tui.background.fail_marker_writes(usize::MAX);
    let target = unknown_marker_target("permanent-write-failure");
    let key = TuiJobKey::Worktree(tui.sessions[0].identity_key(&tui.repos[0].identity));
    tui.record_remote_mutation_reconciliation(&key, 15, "uncertain", &target, None)
        .unwrap();

    let error = tui
        .cleanup_tui_jobs(super::super::ShutdownReason::UserQuit)
        .unwrap_err();
    assert!(error.contains("shutdown durability failure"), "{error}");
    assert!(error.contains("remain unacknowledged"), "{error}");
    assert_eq!(tui.background.unresolved_marker_persistence(), 1);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn empty_list_retains_persisted_create_marker_until_matching_summary_is_observed() {
    let temp = unique_temp_dir("prism-tui-mutation-reconciliation-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut tui = Tui::new_single(repo.clone(), test_config(), vec![session]);
    let worktree = tui.sessions[0].identity_key(&tui.repos[0].identity);
    let target = RemoteMutationTarget::Create {
        source_provider: crate::remote::ProviderKind::GitHub,
        source_host: "github.com".to_string(),
        source_project: "example/repo".to_string(),
        source_branch: "feature".to_string(),
        expected_head_sha: "abc123".to_string(),
        target_provider: Some(crate::remote::ProviderKind::GitHub),
        target_host: "github.com".to_string(),
        target_project: "example/repo".to_string(),
        target_branch: "main".to_string(),
        expected_base_sha: "base123".to_string(),
    };
    let legacy_target = RemoteMutationTarget::Create {
        source_provider: crate::remote::ProviderKind::GitHub,
        source_host: "github.com".to_string(),
        source_project: "example/repo".to_string(),
        source_branch: "feature".to_string(),
        expected_head_sha: "abc123".to_string(),
        target_provider: None,
        target_host: String::new(),
        target_project: String::new(),
        target_branch: String::new(),
        expected_base_sha: String::new(),
    };
    assert!(remote_mutation_targets_overlap(&legacy_target, &target));
    tui.record_remote_mutation_reconciliation(
        &TuiJobKey::Worktree(worktree),
        7,
        "shutdown bound exceeded",
        &target,
        None,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while tui.background.has_jobs() {
        tui.route_tui_job_messages();
        assert!(
            Instant::now() < deadline,
            "marker persistence did not finish"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(tui);

    let mut session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut observed = test_pr_summary(false);
    observed.change_request_identity = Some(test_change_request_identity(
        crate::remote::ProviderKind::GitHub,
    ));
    session.pr = PrCache::observed(observed.clone(), None);
    let mut restarted = Tui::new_single(repo.clone(), test_config(), vec![session]);
    let repository = restarted.repos[0].identity.clone();
    let key = TuiJobKey::Worktree(restarted.sessions[0].identity_key(&repository));
    let deadline = Instant::now() + Duration::from_secs(1);
    while restarted.background.has_jobs() {
        restarted.route_tui_job_messages();
        assert!(Instant::now() < deadline, "marker loading did not finish");
        std::thread::sleep(Duration::from_millis(2));
    }

    assert!(restarted.remote_action_reconciliation_blocked(&key, &target));
    assert!(restarted.sessions[0].pr.trusted_summary().is_err());

    restarted.enqueue_summary_reconciliation(&repository, &[], &BTreeMap::new());
    assert!(restarted.remote_action_reconciliation_blocked(&key, &target));

    let mut wrong_base = observed.clone();
    wrong_base.base_ref = "release".to_string();
    restarted.enqueue_summary_reconciliation(&repository, &[wrong_base], &BTreeMap::new());
    assert!(restarted.remote_action_reconciliation_blocked(&key, &target));

    restarted.enqueue_summary_reconciliation(&repository, &[observed], &BTreeMap::new());

    // Legacy markers intentionally remain blocked: exact durable request identity cannot be
    // inferred safely from only the presentation target.
    assert!(restarted.remote_action_reconciliation_blocked(&key, &target));
    let marker = crate::persistence::database::load_metadata(
        &crate::observability::db_path(&repo),
        super::super::REMOTE_MUTATION_RECONCILIATION_KEY,
    )
    .unwrap();
    assert!(marker.is_some());

    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn typed_push_and_create_markers_select_authoritative_result_observations() {
    let provider = crate::remote::ProviderKind::GitHub;
    let repository_id = crate::remote::RemoteRepositoryId::new(
        provider,
        crate::remote::HostIdentity::new("github.com", None).unwrap(),
        "example/repo",
    )
    .unwrap();
    let push = crate::remote::dispatcher::PushGuard {
        repository: repository_id.clone(),
        remote: "origin".into(),
        remote_branch: "feature".into(),
        local_branch: "feature".into(),
        expected_head_sha: "abc123".into(),
        set_upstream: false,
    };
    let repository = crate::session::WorktreeRepositoryKey::new("/repo".into());
    let push_operation = crate::workflow::remote_operation::RemoteMutationOperation::TuiPushBranch(
        crate::workflow::remote_operation::TuiRemotePushPayload {
            repository: "/repo".into(),
            worktree: "/repo/worktree".into(),
            branch: "feature".into(),
            expected: push.clone(),
        },
    );
    let create_operation =
        crate::workflow::remote_operation::RemoteMutationOperation::TuiCreateChangeRequest(
            crate::workflow::remote_operation::TuiRemoteCreatePayload {
                repository: "/repo".into(),
                worktree: "/repo/worktree".into(),
                branch: "feature".into(),
                body: "description".into(),
                target_repository: repository_id,
                source_push: push,
            },
        );
    let marker = |job_id, target, operation| {
        super::super::remote_action::RemoteMutationReconciliationMarker {
            target,
            ledger: Some(crate::tui::RemoteMutationLedgerContext {
                repository: "/repo".into(),
                worktree: "/repo/worktree".into(),
                request_id: format!("request-{job_id}"),
                operation,
                subject: "/repo:feature".into(),
            }),
            database_path: std::path::PathBuf::new(),
            job_id,
            reason: "uncertain".into(),
            recorded_unix_ms: job_id,
        }
    };
    let markers = vec![
        marker(
            1,
            RemoteMutationTarget::Push {
                remote: "origin".into(),
                branch: "feature".into(),
                expected_head_sha: "abc123".into(),
                repository_provider: Some(provider),
                repository_host: "github.com".into(),
                repository_project: "example/repo".into(),
            },
            push_operation,
        ),
        marker(
            2,
            RemoteMutationTarget::Create {
                source_provider: provider,
                source_host: "github.com".into(),
                source_project: "example/repo".into(),
                source_branch: "feature".into(),
                expected_head_sha: "abc123".into(),
                target_provider: Some(provider),
                target_host: "github.com".into(),
                target_project: "example/repo".into(),
                target_branch: "main".into(),
                expected_base_sha: "base123".into(),
            },
            create_operation,
        ),
    ];
    let mut summary = test_pr_summary(false);
    summary.change_request_identity = Some(test_change_request_identity(provider));
    let commands = super::super::remote_reconciliation::classify_summary_evidence(
        &repository,
        &markers,
        &[summary],
        &BTreeMap::from([(
            ("origin".to_string(), "feature".to_string()),
            "abc123".to_string(),
        )]),
    );

    assert_eq!(commands.len(), 2);
    assert!(matches!(
        commands[0].observation,
        Some(super::super::remote_reconciliation::ReconciliationObservation::PushResult(_))
    ));
    assert!(matches!(
        commands[1].observation,
        Some(super::super::remote_reconciliation::ReconciliationObservation::Cache(_))
    ));
}

#[test]
fn failed_reconciliation_releases_the_marker_for_retry() {
    let temp = unique_temp_dir("prism-tui-reconciliation-retry");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    wait_for_background(&mut tui);
    let repository = tui.repos[0].identity.clone();
    let key = TuiJobKey::Worktree(tui.sessions[0].identity_key(&repository));
    let identity = test_change_request_identity(crate::remote::ProviderKind::GitHub);
    let target = RemoteMutationTarget::Review {
        change_request: identity.clone(),
        expected_state: "APPROVED".to_string(),
        expected_body: "looks good".to_string(),
        prior_review_ids: Vec::new(),
    };
    tui.record_remote_mutation_reconciliation(&key, 77, "uncertain", &target, None)
        .unwrap();
    wait_for_background(&mut tui);
    let marker = tui.background.markers(&repository.root).unwrap()[0].clone();
    let marker_key = (
        repository.root.clone(),
        marker.recorded_unix_ms,
        marker.job_id,
    );
    let mut summary = test_pr_summary(false);
    summary.change_request_identity = Some(identity);
    let details = PrDetails {
        reviews: vec![PrReview {
            id: "review-1".to_string(),
            state: "APPROVED".to_string(),
            body: "looks good".to_string(),
            ..PrReview::default()
        }],
        ..PrDetails::default()
    };

    tui.enqueue_details_reconciliation(&repository, &PrCache::observed(summary, Some(details)));
    wait_for_background(&mut tui);

    assert_eq!(tui.background.marker_count(&repository.root), 1);
    assert!(tui.background.begin_reconciliation(marker_key));
    tui.background.finish_reconciliation(
        &repository.root,
        marker.recorded_unix_ms,
        marker.job_id,
        &target,
        false,
    );

    drop(tui);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn reconciliation_clears_only_markers_with_matched_authoritative_evidence() {
    let temp = unique_temp_dir("prism-tui-matched-mutation-evidence-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    let repository = tui.repos[0].identity.clone();
    let key = TuiJobKey::Worktree(tui.sessions[0].identity_key(&repository));
    let identity = test_change_request_identity(crate::remote::ProviderKind::GitHub);
    let targets = [
        RemoteMutationTarget::Push {
            remote: "origin".to_string(),
            branch: "feature".to_string(),
            expected_head_sha: "abc123".to_string(),
            repository_provider: None,
            repository_host: String::new(),
            repository_project: String::new(),
        },
        RemoteMutationTarget::Create {
            source_provider: crate::remote::ProviderKind::GitHub,
            source_host: "github.com".to_string(),
            source_project: "example/repo".to_string(),
            source_branch: "feature".to_string(),
            expected_head_sha: "abc123".to_string(),
            target_provider: None,
            target_host: String::new(),
            target_project: String::new(),
            target_branch: String::new(),
            expected_base_sha: String::new(),
        },
        RemoteMutationTarget::Review {
            change_request: identity.clone(),
            expected_state: "APPROVED".to_string(),
            expected_body: "looks good".to_string(),
            prior_review_ids: vec!["review-1".to_string()],
        },
        RemoteMutationTarget::Resolve {
            change_request: identity.clone(),
            thread_ids: vec!["thread-1".to_string()],
        },
        RemoteMutationTarget::Merge {
            change_request: identity.clone(),
            expected_head_sha: "abc123".to_string(),
        },
    ];
    for (job_id, target) in targets.iter().enumerate() {
        tui.record_remote_mutation_reconciliation(
            &key,
            job_id as u64 + 1,
            "uncertain",
            target,
            None,
        )
        .unwrap();
    }

    tui.enqueue_summary_reconciliation(&repository, &[], &BTreeMap::new());
    let mut empty_summary = test_pr_summary(false);
    empty_summary.change_request_identity = Some(identity.clone());
    tui.enqueue_details_reconciliation(
        &repository,
        &PrCache::observed(empty_summary, Some(PrDetails::default())),
    );
    assert_eq!(tui.background.marker_count(&repository.root), 5);

    let mut create = test_pr_summary(false);
    create.change_request_identity = Some(identity.clone());
    let mut pending_merge = create.clone();
    pending_merge.queue_state = "AWAITING_CHECKS".to_string();
    tui.enqueue_summary_reconciliation(
        &repository,
        &[create, pending_merge.clone()],
        &BTreeMap::from([(
            ("origin".to_string(), "feature".to_string()),
            "abc123".to_string(),
        )]),
    );
    assert_eq!(tui.background.marker_count(&repository.root), 5);

    let details = PrDetails {
        reviews: vec![PrReview {
            id: "review-2".to_string(),
            state: "APPROVED".to_string(),
            body: "looks good".to_string(),
            ..PrReview::default()
        }],
        review_comments: vec![PrReviewComment {
            thread_id: "thread-1".to_string(),
            resolved: true,
            ..PrReviewComment::default()
        }],
        ..PrDetails::default()
    };
    tui.enqueue_details_reconciliation(
        &repository,
        &PrCache::observed(pending_merge, Some(details)),
    );
    assert_eq!(
        tui.background.marker_count(&repository.root),
        5,
        "legacy markers without exact ledger identity must not be inferred or cleared"
    );
    drop(tui);
    let deadline = Instant::now() + Duration::from_secs(1);
    while fs::remove_dir_all(&temp).is_err() {
        assert!(
            Instant::now() < deadline,
            "background marker persistence did not stop"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn cleanup_applies_a_mutation_payload_routed_before_shutdown_started() {
    let temp = unique_temp_dir("prism-tui-routed-mutation-shutdown-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let branch = "feature";
    let session = test_session(0, &temp.join("worktree").display().to_string(), branch);
    let mut tui = Tui::new_single(repo.clone(), test_config(), vec![session]);
    let key = TuiJobKey::Worktree(tui.sessions[0].identity_key(&tui.repos[0].identity));
    let job_key = key.clone();
    let target = RemoteMutationTarget::Push {
        remote: "origin".to_string(),
        branch: branch.to_string(),
        expected_head_sha: "abc123".to_string(),
        repository_provider: None,
        repository_host: String::new(),
        repository_project: String::new(),
    };
    let mut summary = test_pr_summary(false);
    summary.number = 42;
    summary.head_ref = branch.to_string();
    summary.change_request_identity = Some(test_change_request_identity(
        crate::remote::ProviderKind::GitHub,
    ));
    let cache = PrCache::observed(summary, None);
    let id = tui.spawn_tui_job(
        TuiJobKind::RemoteAction,
        key,
        0,
        None,
        "already-routed-mutation".to_string(),
        move |context| {
            Ok(Some(TuiJobPayload::RemoteAction(Box::new(
                RemoteActionDelivery {
                    id: context.id(),
                    result: Ok(RemoteActionValue::Cache(Box::new(cache))),
                },
            ))))
        },
    );
    // This regression covers a result routed before shutdown, not an in-flight coordinated
    // mutation. Route it to the delivery channel before cleanup so the shutdown path persists it
    // without treating it as unfinished provider work.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        tui.route_tui_job_messages();
        if let Some(delivery) = tui.background.receive_remote_action() {
            tui.background.deliver_remote_action(delivery);
            break;
        }
        assert!(Instant::now() < deadline);
    }
    tui.background.track_remote_action(
        id,
        super::super::remote_action::RemoteActionReconciliationContext {
            key: job_key,
            target,
            ledger: crate::tui::RemoteMutationLedgerContext {
                repository: repo.root.clone(),
                worktree: temp.join("worktree"),
                request_id: "shutdown-test".to_string(),
                operation: crate::workflow::remote_operation::RemoteMutationOperation::TuiFetchChangeRequest(
                    crate::workflow::remote_operation::TuiRemoteFetchPayload {
                        repository: repo.root.clone(),
                        worktree: temp.join("worktree"),
                        branch: branch.to_string(),
                        summary: test_pr_summary(false),
                    },
                ),
                subject: "shutdown-test".to_string(),
            },
        },
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    while tui.background.has_jobs() {
        tui.route_tui_job_messages();
        assert!(Instant::now() < deadline);
    }
    tui.route_tui_job_messages();

    tui.cleanup_tui_jobs(super::super::ShutdownReason::Sigterm)
        .unwrap();

    assert!(tui.background.tracked_remote_action_ids().is_empty());
    assert_eq!(
        crate::remote::load_pr_cache(&repo, branch)
            .summary()
            .unwrap()
            .number,
        42
    );
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn mutation_remote_action_jobs_cannot_be_abandoned_or_timed_out() {
    let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

    for mutation in [
        "branch push",
        "guarded push",
        "review submission",
        "change request creation",
        "thread resolution",
        "merge",
    ] {
        assert!(
            !remote_action_abandon_requested(false, escape),
            "{mutation} must ignore Escape"
        );
        assert!(
            !remote_action_abandon_requested(false, interrupt),
            "{mutation} must ignore Ctrl-C"
        );
        assert_eq!(remote_action_timeout(false), None, "{mutation}");
    }

    assert!(remote_action_abandon_requested(true, escape));
    assert!(remote_action_abandon_requested(true, interrupt));
    assert_eq!(
        remote_action_timeout(true),
        Some(super::super::TUI_ACTION_JOB_TIMEOUT)
    );
}

#[test]
fn push_remote_action_cannot_be_abandoned_and_reconciles_after_generation_change() {
    let abandon_cancelable = false;
    let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(!remote_action_abandon_requested(abandon_cancelable, escape));
    assert!(!remote_action_abandon_requested(
        abandon_cancelable,
        interrupt
    ));
    assert_eq!(remote_action_timeout(abandon_cancelable), None);

    let mut tui = test_tui();
    let key = TuiJobKey::Repository(tui.repos[0].identity.clone());
    let id = tui.spawn_tui_job(
        TuiJobKind::RemoteAction,
        key,
        tui.session_inventory_generation,
        remote_action_timeout(abandon_cancelable),
        "push-reconciliation-test".to_string(),
        |context| {
            Ok(Some(TuiJobPayload::RemoteAction(Box::new(
                RemoteActionDelivery {
                    id: context.id(),
                    result: Ok(RemoteActionValue::Complete),
                },
            ))))
        },
    );
    if !abandon_cancelable {
        tui.background.track_remote_action(id, super::super::remote_action::RemoteActionReconciliationContext {
            key: TuiJobKey::Repository(tui.repos[0].identity.clone()),
            target: RemoteMutationTarget::Unknown { marker_id: "test".into() },
            ledger: crate::tui::RemoteMutationLedgerContext {
                repository: tui.repo.root.clone(), worktree: tui.repo.root.clone(), request_id: "test".into(),
                operation: crate::workflow::remote_operation::RemoteMutationOperation::TuiFetchChangeRequest(crate::workflow::remote_operation::TuiRemoteFetchPayload { repository: tui.repo.root.clone(), worktree: tui.repo.root.clone(), branch: "branch".into(), summary: test_pr_summary(false) }), subject: "test".into()
            }
        });
    }
    tui.session_inventory_generation += 1;

    let deadline = Instant::now() + Duration::from_secs(1);
    let delivery = loop {
        tui.route_tui_job_messages();
        if let Some(delivery) = tui.background.receive_remote_action() {
            break delivery;
        }
        assert!(Instant::now() < deadline, "push result was discarded");
        std::thread::sleep(Duration::from_millis(5));
    };

    assert_eq!(delivery.id, id);
    assert!(matches!(delivery.result, Ok(RemoteActionValue::Complete)));
    tui.background.finish_remote_action(id);
}

#[test]
fn queued_remote_timing_updates_the_visible_progress_dialog() {
    let temp = unique_temp_dir("prism-tui-remote-wait-progress-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    let session = test_session(0, &temp.display().to_string(), "feature");
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    tui.dialog = Some(crate::view::DialogModel::Progress {
        title: "Remote".into(),
        message: "Starting".into(),
    });

    tui.route_tui_job_payload(TuiJobPayload::RemoteActionProgress {
        id: 42,
        message: "waiting for github.com request slot; position 2".into(),
    });

    assert!(matches!(
        tui.dialog,
        Some(crate::view::DialogModel::Progress { ref message, .. })
            if message.contains("position 2")
    ));
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn merge_summary_refresh_preserves_matching_details() {
    let summary = test_pr_summary(false);
    let details = PrDetails {
        comments: vec![crate::remote::PrComment {
            id: "comment-1".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut cache = PrCache::observed(summary.clone(), Some(details));
    let mut queued = summary;
    queued.queue_state = "AWAITING_CHECKS".into();

    cache.apply_worker_summary(queued);

    assert_eq!(cache.summary().unwrap().queue_state, "AWAITING_CHECKS");
    assert_eq!(cache.details().unwrap().comments.len(), 1);
}

#[test]
fn uncertain_merge_summary_remains_untrusted_until_reobserved() {
    let mut cache = PrCache::observed(test_pr_summary(false), None);
    cache.require_reconciliation(
        "provider merge outcome is uncertain; authoritative re-observation required",
    );

    assert!(cache.trusted_summary().is_err());
}

#[test]
fn uncertain_merge_result_requires_authoritative_reconciliation() {
    assert!(
        super::super::uncertain_remote_mutation_error(&Err("provider rejected the request".into()))
            .is_none()
    );
    assert!(
        super::super::uncertain_remote_mutation_error(&Err(
            "uncertain remote mutation: provider outcome unknown".into()
        ))
        .is_some()
    );
    let rejected = Ok(RemoteActionValue::MergeRejected(
        "provider policy does not currently authorize merge".into(),
    ));
    assert!(super::super::uncertain_remote_mutation_error(&rejected).is_none());

    let cache = PrCache::observed(test_pr_summary(false), None);
    let result = Ok(RemoteActionValue::Merge {
        cache: Box::new(cache),
        outcome: crate::workflow::standard_remote::TuiRemoteMergeOutcome::Uncertain,
    });
    assert!(
        super::super::uncertain_remote_mutation_error(&result)
            .is_some_and(|error| error.contains("not authoritative"))
    );

    for outcome in [
        crate::workflow::standard_remote::TuiRemoteMergeOutcome::Merged,
        crate::workflow::standard_remote::TuiRemoteMergeOutcome::Pending,
    ] {
        let result = Ok(RemoteActionValue::Merge {
            cache: Box::new(PrCache::observed(test_pr_summary(false), None)),
            outcome,
        });
        assert!(super::super::uncertain_remote_mutation_error(&result).is_none());
    }
}

#[test]
fn applying_remote_action_results_performs_no_provider_or_database_io() {
    let temp = unique_temp_dir("prism-tui-remote-action-result-test");
    fs::create_dir_all(&temp).unwrap();
    let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
    crate::observability::with_writable_db(&repo, |_| Ok(())).unwrap();
    let session = test_session(0, &temp.display().to_string(), "feature");
    let mut tui = Tui::new_single(repo, test_config(), vec![session]);
    let cache = PrCache::observed(test_pr_summary(false), None);

    crate::flight_recorder::deny_external_calls_on_current_thread(|| {
        crate::observability::deny_database_access_on_current_thread(|| {
            tui.apply_remote_cache_result(0, cache);
            tui.route_tui_job_payload(TuiJobPayload::RemoteAction(Box::new(
                RemoteActionDelivery {
                    id: 42,
                    result: Ok(RemoteActionValue::Complete),
                },
            )));
        });
    });

    assert_eq!(tui.sessions[0].pr.summary().unwrap().number, 1);
    let delivery = tui.background.receive_remote_action().unwrap();
    assert_eq!(delivery.id, 42);
    assert!(matches!(delivery.result, Ok(RemoteActionValue::Complete)));
    let _ = fs::remove_dir_all(temp);
}
