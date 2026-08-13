use std::collections::BTreeMap;
use std::fs;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::remote::{PrCache, PrDetails, PrReview, PrReviewComment};
use crate::repo::Repository;

use super::super::{
    RemoteActionDelivery, RemoteActionReconciliationContext, RemoteActionValue,
    RemoteMutationTarget, Tui, TuiJobKey, TuiJobKind, TuiJobPayload,
    remote_action_abandon_requested, remote_action_timeout, remote_mutation_targets_overlap,
};
use super::support::{
    test_change_request_identity, test_config, test_pr_summary, test_session, test_tui,
    unique_temp_dir,
};

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
    )
    .unwrap();
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

    assert!(restarted.remote_action_reconciliation_blocked(&key, &target));
    assert!(restarted.sessions[0].pr.trusted_summary().is_err());

    restarted.reconcile_remote_mutation_summaries(&repository, &[], &BTreeMap::new());
    assert!(restarted.remote_action_reconciliation_blocked(&key, &target));

    let mut wrong_base = observed.clone();
    wrong_base.base_ref = "release".to_string();
    restarted.reconcile_remote_mutation_summaries(&repository, &[wrong_base], &BTreeMap::new());
    assert!(restarted.remote_action_reconciliation_blocked(&key, &target));

    restarted.reconcile_remote_mutation_summaries(&repository, &[observed], &BTreeMap::new());

    assert!(!restarted.remote_action_reconciliation_blocked(&key, &target));
    let marker = crate::persistence::database::load_metadata(
        &crate::observability::db_path(&repo),
        super::super::REMOTE_MUTATION_RECONCILIATION_KEY,
    )
    .unwrap();
    assert!(marker.is_none());

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
        tui.record_remote_mutation_reconciliation(&key, job_id as u64 + 1, "uncertain", target)
            .unwrap();
    }

    tui.reconcile_remote_mutation_summaries(&repository, &[], &BTreeMap::new());
    let mut empty_summary = test_pr_summary(false);
    empty_summary.change_request_identity = Some(identity.clone());
    tui.reconcile_remote_mutation_details(
        &repository,
        &PrCache::observed(empty_summary, Some(PrDetails::default())),
    );
    assert_eq!(
        tui.remote_mutations_requiring_reconciliation[&repository.root].len(),
        5
    );

    let mut create = test_pr_summary(false);
    create.change_request_identity = Some(identity.clone());
    let mut pending_merge = create.clone();
    pending_merge.queue_state = "AWAITING_CHECKS".to_string();
    tui.reconcile_remote_mutation_summaries(
        &repository,
        &[create, pending_merge.clone()],
        &BTreeMap::from([(
            ("origin".to_string(), "feature".to_string()),
            "abc123".to_string(),
        )]),
    );
    assert_eq!(
        tui.remote_mutations_requiring_reconciliation[&repository.root].len(),
        2
    );

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
    tui.reconcile_remote_mutation_details(
        &repository,
        &PrCache::observed(pending_merge, Some(details)),
    );
    assert!(
        !tui.remote_mutations_requiring_reconciliation
            .contains_key(&repository.root)
    );
    fs::remove_dir_all(temp).unwrap();
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
    tui.remote_actions_requiring_reconciliation.insert(id);
    tui.remote_action_reconciliation_contexts.insert(
        id,
        RemoteActionReconciliationContext {
            key: job_key,
            target,
        },
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    while tui.jobs.has_jobs() {
        tui.route_tui_job_messages();
        assert!(Instant::now() < deadline);
    }
    tui.route_tui_job_messages();

    tui.cleanup_tui_jobs(super::super::ShutdownReason::Sigterm)
        .unwrap();

    assert!(tui.remote_actions_requiring_reconciliation.is_empty());
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
        tui.remote_actions_requiring_reconciliation.insert(id);
    }
    tui.session_inventory_generation += 1;

    let deadline = Instant::now() + Duration::from_secs(1);
    let delivery = loop {
        tui.route_tui_job_messages();
        if let Ok(delivery) = tui.remote_action_rx.try_recv() {
            break delivery;
        }
        assert!(Instant::now() < deadline, "push result was discarded");
        std::thread::sleep(Duration::from_millis(5));
    };

    assert_eq!(delivery.id, id);
    assert!(matches!(delivery.result, Ok(RemoteActionValue::Complete)));
    tui.remote_actions_requiring_reconciliation.remove(&id);
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
    let delivery = tui.remote_action_rx.try_recv().unwrap();
    assert_eq!(delivery.id, 42);
    assert!(matches!(delivery.result, Ok(RemoteActionValue::Complete)));
    let _ = fs::remove_dir_all(temp);
}
