use std::time::Duration;

use super::*;

fn repository(provider: ProviderKind, host: &str, path: &str) -> RemoteRepositoryId {
    RemoteRepositoryId::new(
        provider,
        HostIdentity::new(host, None).expect("valid test host"),
        path,
    )
    .expect("valid test repository")
}

fn change_request(
    provider: ProviderKind,
    host: &str,
    path: &str,
    native_id: &str,
    display_number: Option<u64>,
) -> ChangeRequestId {
    ChangeRequestId::new(
        repository(provider, host, path),
        NativeChangeRequestId::new(native_id).expect("valid test change request ID"),
        display_number,
    )
}

#[test]
fn canonical_identity_normalizes_dns_but_preserves_project_path_case() {
    let first = RemoteRepositoryId::new(
        ProviderKind::GitLab,
        HostIdentity::new("GITLAB.COM.", None).unwrap(),
        "Team/SubGroup/Project.git",
    )
    .unwrap();
    let second = repository(ProviderKind::GitLab, "gitlab.com", "Team/SubGroup/Project");
    let different_case = repository(ProviderKind::GitLab, "gitlab.com", "team/SubGroup/Project");

    assert_eq!(first, second);
    assert_ne!(first, different_case);
}

#[test]
fn change_request_identity_ignores_display_metadata_and_formats_provider_labels() {
    let github = change_request(
        ProviderKind::GitHub,
        "github.com",
        "Owner/Repo",
        "PR_kwDOopaque",
        Some(42),
    );
    let relabeled = change_request(
        ProviderKind::GitHub,
        "github.com",
        "Owner/Repo",
        "PR_kwDOopaque",
        None,
    );
    let gitlab = change_request(
        ProviderKind::GitLab,
        "gitlab.com",
        "group/repo",
        "gid://gitlab/MergeRequest/7",
        Some(7),
    );

    assert_eq!(github, relabeled);
    assert_eq!(github.display_label(), "#42");
    assert_eq!(relabeled.display_label(), "PR_kwDOopaque");
    assert_eq!(gitlab.display_label(), "!7");
}

#[test]
fn unknown_native_states_are_retained_verbatim() {
    assert_eq!(
        LifecycleState::from_native("SUPERSEDED_BY_TRAIN"),
        LifecycleState::Unknown("SUPERSEDED_BY_TRAIN".to_string())
    );
    assert_eq!(
        ReviewDecision::from_native("WAITING_FOR_CODEOWNERS"),
        ReviewDecision::Unknown("WAITING_FOR_CODEOWNERS".to_string())
    );
    assert_eq!(
        MergeabilityState::from_native("checking_again"),
        MergeabilityState::Unknown("checking_again".to_string())
    );
    assert_eq!(
        CheckState::from_native("warning_with_exceptions"),
        CheckState::Unknown("warning_with_exceptions".to_string())
    );
    assert_eq!(
        QueueState::from_native("preparing_merged_result"),
        QueueState::Unknown("preparing_merged_result".to_string())
    );
}

#[test]
fn capability_support_is_independent_from_observation_quality() {
    let capabilities = Capabilities {
        ci_logs: SupportLevel::Supported,
        resolve_review_thread: SupportLevel::Unsupported,
        merge_queue: SupportLevel::Conditional,
        ..Capabilities::default()
    };
    let failed: Observation<Vec<CiFailure>> = Observation::Failed(
        RemoteError::new(
            ProviderKind::GitLab,
            RemoteOperation::LoadCiLogs,
            RemoteErrorClass::Timeout,
            Retryability::Retryable,
            "CI log request timed out",
        )
        .with_retry_hint(RetryHint::After(Duration::from_secs(10))),
    );

    assert_eq!(
        capabilities.support_for(RemoteOperation::LoadCiLogs),
        SupportLevel::Supported
    );
    assert!(matches!(failed, Observation::Failed(_)));
    assert_eq!(
        capabilities.support_for(RemoteOperation::ResolveReviewThread),
        SupportLevel::Unsupported
    );
    assert_eq!(
        capabilities.support_for(RemoteOperation::ObserveMergeQueue),
        SupportLevel::Conditional
    );
}

#[test]
fn observation_absence_empty_stale_failed_and_unavailable_states_are_distinct() {
    let error = RemoteError::new(
        ProviderKind::Forgejo,
        RemoteOperation::ObserveChecks,
        RemoteErrorClass::Transport,
        Retryability::Retryable,
        "check refresh failed",
    );
    let states = [
        Observation::<Vec<String>>::NotLoaded,
        Observation::Unsupported,
        Observation::Unconfigured,
        Observation::Unauthorized,
        Observation::AuthoritativelyAbsent,
        Observation::EmptyKnown,
        Observation::Known(Vec::new()),
        Observation::Stale {
            value: vec!["previous".to_string()],
            error: Some(error.clone()),
        },
        Observation::Failed(error),
    ];

    for (index, state) in states.iter().enumerate() {
        for other in states.iter().skip(index + 1) {
            assert_ne!(state, other);
        }
    }
    assert!(states[4].is_authoritative());
    assert!(states[5].is_authoritative());
    assert!(states[6].is_authoritative());
    assert!(!states[7].is_authoritative());
}

#[test]
fn head_association_requires_canonical_identity_and_exact_sha() {
    let id = change_request(
        ProviderKind::GitHub,
        "github.com",
        "Owner/Repo",
        "opaque-11",
        Some(11),
    );
    let relabeled = change_request(
        ProviderKind::GitHub,
        "GITHUB.COM.",
        "Owner/Repo.git",
        "opaque-11",
        None,
    );
    let different = change_request(
        ProviderKind::GitHub,
        "github.com",
        "Owner/Repo",
        "opaque-12",
        Some(12),
    );
    let association = HeadAssociation::new(id, "abc123");

    assert!(association.matches(&relabeled, "abc123"));
    assert!(!association.matches(&relabeled, "def456"));
    assert!(!association.matches(&different, "abc123"));
}

#[test]
fn parser_accepts_https_ssh_and_scp_forms_and_normalizes_default_ports() {
    let parser = GitRemoteParser::default();
    let cases = [
        (
            "https://GitHub.COM.:443/Owner/Repo.git",
            GitTransport::Https,
        ),
        ("ssh://git@github.com:22/Owner/Repo.git", GitTransport::Ssh),
        ("git@github.com:Owner/Repo.git", GitTransport::Ssh),
        ("github.com:Owner/Repo.git", GitTransport::Ssh),
    ];

    for (remote, transport) in cases {
        let parsed = parser.parse(remote).unwrap();
        assert_eq!(parsed.transport, transport);
        assert_eq!(parsed.host, HostIdentity::new("github.com", None).unwrap());
        assert_eq!(parsed.project_path, "Owner/Repo");
    }
}

#[test]
fn parser_preserves_non_default_ports_and_nested_project_paths() {
    let parsed = GitRemoteParser::default()
        .parse("ssh://git@Git.Example.COM.:2222/Division/Team/Project.git")
        .unwrap();

    assert_eq!(
        parsed.host,
        HostIdentity::new("git.example.com", Some(2222)).unwrap()
    );
    assert_eq!(parsed.project_path, "Division/Team/Project");
}

#[test]
fn http_requires_an_explicit_parser_or_host_profile_opt_in() {
    let remote = "http://git.example.com/Owner/Repo.git";
    assert!(matches!(
        GitRemoteParser::default().parse(remote),
        Err(DiscoveryError::InsecureHttp(_))
    ));
    assert!(GitRemoteParser::new(true).parse(remote).is_ok());

    let host = HostIdentity::new("git.example.com", None).unwrap();
    let profile = HostProfile::new(host, ProviderKind::Forgejo)
        .unwrap()
        .with_http_allowed(true);
    let discovered = RemoteDiscovery::new([profile])
        .unwrap()
        .discover(remote)
        .unwrap();
    assert_eq!(discovered.repository.id.provider(), ProviderKind::Forgejo);
}

#[test]
fn builtins_map_codeberg_to_forgejo_and_use_provider_bases() {
    let discovery = RemoteDiscovery::default();
    let codeberg = discovery
        .discover("https://codeberg.org/Team/Project.git")
        .unwrap();
    let github = discovery
        .discover("git@github.com:Owner/Project.git")
        .unwrap();

    assert_eq!(codeberg.repository.id.provider(), ProviderKind::Forgejo);
    assert_eq!(
        codeberg.repository.api_base.to_string(),
        "https://codeberg.org/api/v1"
    );
    assert_eq!(github.repository.id.provider(), ProviderKind::GitHub);
    assert_eq!(
        github.repository.api_base.to_string(),
        "https://api.github.com"
    );
}

#[test]
fn explicit_profiles_enable_known_self_hosted_instances_without_network_detection() {
    let host = HostIdentity::new("git.internal.example", None).unwrap();
    let profile = HostProfile::new(host, ProviderKind::GitLab).unwrap();
    let discovery = RemoteDiscovery::new([profile]).unwrap();
    let discovered = discovery
        .discover("ssh://git@git.internal.example:2202/Group/Subgroup/Repo.git")
        .unwrap();

    assert_eq!(discovered.repository.id.provider(), ProviderKind::GitLab);
    assert_eq!(discovered.parsed.host.port(), Some(2202));
    assert_eq!(discovered.repository.id.host().port(), None);
    assert_eq!(
        discovered.repository.id.project_path(),
        "Group/Subgroup/Repo"
    );
}

#[test]
fn explicit_profile_ports_and_credential_names_remain_configuration_not_secrets() {
    let host = HostIdentity::new("forge.example", Some(8443)).unwrap();
    let profile = HostProfile::new(host, ProviderKind::Forgejo)
        .unwrap()
        .with_credential_environment("FORGEJO_TOKEN")
        .unwrap();

    assert_eq!(profile.web_base.to_string(), "https://forge.example:8443");
    assert_eq!(
        profile.api_base.to_string(),
        "https://forge.example:8443/api/v1"
    );
    assert_eq!(
        profile.credential_environment.as_deref(),
        Some("FORGEJO_TOKEN")
    );
    assert!(
        HostProfile::new(
            HostIdentity::new("forge.example", None).unwrap(),
            ProviderKind::Forgejo
        )
        .unwrap()
        .with_credential_environment("not-a-variable")
        .is_err()
    );
}

#[test]
fn unknown_hosts_and_conflicting_profiles_are_rejected() {
    assert!(matches!(
        RemoteDiscovery::default().discover("https://unknown.example/owner/repo.git"),
        Err(DiscoveryError::UnknownHost(_))
    ));

    let host = HostIdentity::new("github.com", None).unwrap();
    let conflict = HostProfile::new(host, ProviderKind::GitLab).unwrap();
    assert!(matches!(
        RemoteDiscovery::new([conflict]),
        Err(DiscoveryError::ConflictingProfile(_))
    ));
}

#[test]
fn malformed_remotes_fail_without_partial_identity() {
    let parser = GitRemoteParser::default();
    for remote in [
        "",
        " github.com:owner/repo.git",
        "file:///owner/repo",
        "https://github.com",
        "https://github.com/only-repository",
        "https://github.com/owner//repo",
        "https://github.com/owner/../repo",
        "https://github.com:invalid/owner/repo",
        "https://token@github.com/owner/repo",
        "C:\\owner\\repo",
    ] {
        assert!(
            parser.parse(remote).is_err(),
            "accepted malformed remote {remote}"
        );
    }
}

#[test]
fn remote_error_keeps_classification_and_only_exposes_bounded_single_line_message() {
    let error = RemoteError::new(
        ProviderKind::GitHub,
        RemoteOperation::MergeChangeRequest,
        RemoteErrorClass::StaleHead,
        Retryability::NotRetryable,
        format!("  expected head changed\n{}", "x".repeat(600)),
    )
    .with_status(409)
    .with_exit_code(1)
    .with_retry_hint(RetryHint::RefreshObservation);

    assert_eq!(error.provider(), ProviderKind::GitHub);
    assert_eq!(error.operation(), RemoteOperation::MergeChangeRequest);
    assert_eq!(error.class(), RemoteErrorClass::StaleHead);
    assert_eq!(error.retryability(), Retryability::NotRetryable);
    assert_eq!(error.status(), Some(409));
    assert_eq!(error.exit_code(), Some(1));
    assert_eq!(error.retry_hint(), Some(RetryHint::RefreshObservation));
    assert!(!error.safe_message().contains('\n'));
    assert!(error.safe_message().chars().count() <= 512);
}
