use std::sync::Arc;

use prism::extension::{
    ExtensionClient, ExtensionOperations, ExtensionSupervisor, HostLimits, NoHostOperations,
};
use prism_extension_sdk::protocol::{AttemptEnvelope, AttemptOutcome};

fn attempt(id: &str, input: serde_json::Value) -> AttemptEnvelope {
    AttemptEnvelope {
        attempt_id: id.into(),
        generation: 1,
        implementation_id: "acme.fixture/echo".into(),
        input,
        artifacts: Default::default(),
    }
}

fn standard_workflow_sources() -> Vec<(String, String)> {
    [
        ("plan", include_str!("../../../assets/workflows/plan.toml")),
        (
            "implement",
            include_str!("../../../assets/workflows/implement.toml"),
        ),
        ("auto", include_str!("../../../assets/workflows/auto.toml")),
        (
            "stabilize",
            include_str!("../../../assets/workflows/stabilize.toml"),
        ),
        (
            "stabilize-change-request",
            include_str!("../../../assets/workflows/stabilize-change-request.toml"),
        ),
        (
            "triage-issues",
            include_str!("../../../assets/workflows/triage-issues.toml"),
        ),
    ]
    .into_iter()
    .map(|(name, source)| {
        (
            name.into(),
            format!("id = \"prism.standard/{name}\"\n{source}"),
        )
    })
    .collect()
}

#[tokio::test]
async fn portable_shell_fixture_proves_language_neutral_protocol_conformance() {
    let executable = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/non_rust_extension.sh");
    let client = ExtensionClient::launch(
        executable,
        Arc::new(NoHostOperations),
        HostLimits::default(),
    )
    .await
    .unwrap();
    assert_eq!(client.platform(), "portable-shell");
    assert_eq!(client.descriptor().implementations[0].id, "acme.shell/echo");
    let (_owner, cancellation) = tokio::sync::watch::channel(false);
    let result = client
        .execute(
            AttemptEnvelope {
                implementation_id: "acme.shell/echo".into(),
                ..attempt("shell-attempt", serde_json::json!({}))
            },
            cancellation,
        )
        .await
        .unwrap();
    assert_eq!(
        result.outcome,
        AttemptOutcome::Succeeded {
            outputs: serde_json::json!({"language":"shell"})
        }
    );
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn six_standard_workflows_compile_through_the_public_extension_contract() {
    let client = ExtensionClient::launch(
        env!("CARGO_BIN_EXE_prism-standard-extension"),
        Arc::new(NoHostOperations),
        HostLimits::default(),
    )
    .await
    .unwrap();
    let mut registry = prism::extension::DescriptorRegistry::default();
    registry.register(client.descriptor()).unwrap();
    let catalog =
        prism::DefinitionCatalog::from_sources(standard_workflow_sources(), registry).unwrap();
    assert_eq!(catalog.list().len(), 6);
    let stabilize = catalog.compile("prism.standard/stabilize").unwrap();
    assert_eq!(
        stabilize.definition.launch,
        [prism::LaunchMode::Manual].into()
    );
    assert!(stabilize.definition.inputs["candidate"].from_context);
    assert!(
        stabilize
            .definition
            .steps
            .iter()
            .all(|step| step.id != "implement")
    );
    let flagship = catalog.compile("prism.standard/auto").unwrap();
    let stabilization = flagship
        .definition
        .steps
        .iter()
        .find(|step| step.id == "stabilize")
        .unwrap();
    assert_eq!(stabilization.repeat.as_ref().unwrap().max_iterations, 3);
    assert_eq!(flagship.children.len(), 2);
    assert!(
        flagship
            .definition
            .steps
            .iter()
            .any(|step| step.id == "approve-exact-head")
    );
    assert!(
        flagship
            .definition
            .steps
            .iter()
            .any(|step| step.id == "cleanup")
    );
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn flagship_launch_is_pinned_after_working_copy_changes_or_deletion() {
    let client = ExtensionClient::launch(
        env!("CARGO_BIN_EXE_prism-standard-extension"),
        Arc::new(NoHostOperations),
        HostLimits::default(),
    )
    .await
    .unwrap();
    let mut registry = prism::extension::DescriptorRegistry::default();
    registry.register(client.descriptor()).unwrap();
    let mut sources = standard_workflow_sources();
    let catalog = prism::DefinitionCatalog::from_sources(sources.clone(), registry).unwrap();
    let snapshot = catalog.compile("prism.standard/auto").unwrap();
    let original_digest = snapshot.digest.clone();
    let original_body = serde_json::to_string(&snapshot).unwrap();

    let auto = sources.iter_mut().find(|(name, _)| name == "auto").unwrap();
    auto.1 = auto.1.replace(
        "Implement, stabilize, approve",
        "CUSTOMIZED: Implement and approve",
    );
    let mut changed_registry = prism::extension::DescriptorRegistry::default();
    changed_registry.register(client.descriptor()).unwrap();
    let changed = prism::DefinitionCatalog::from_sources(sources, changed_registry)
        .unwrap()
        .compile("prism.standard/auto")
        .unwrap();
    assert_ne!(changed.digest, original_digest);

    let database = std::env::temp_dir().join(format!(
        "prism-phase6-pinned-{}-{}.sqlite3",
        std::process::id(),
        original_digest.trim_start_matches("sha256:")
    ));
    let _ = std::fs::remove_file(&database);
    let operations = prism::WorkflowOperations::open(&database).await.unwrap();
    operations
        .register_definition(prism::DefinitionSnapshot {
            id: &snapshot.digest,
            name: &snapshot.definition.name,
            revision: &snapshot.sources["prism.standard/auto"].revision,
            source: "deleted-working-copy/auto.toml",
            trusted: true,
            body_json: &original_body,
            digest: &snapshot.digest,
            now_unix_ms: 1,
        })
        .await
        .unwrap();
    operations
        .launch(prism::LaunchWorkflow {
            run_id: "phase6-pinned-run",
            definition_snapshot_id: &snapshot.digest,
            repository: Some("repo-1"),
            idempotency_key: "phase6-pinned-run",
            input_json: r#"{"task":{"title":"Phase 6"},"worktree":{"id":"wt-1","revision":"incarnation-1"}}"#,
            now_unix_ms: 2,
        })
        .await
        .unwrap();
    let run = operations
        .inspect("phase6-pinned-run")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.definition_name, "auto");
    assert!(run.steps.iter().any(|step| step.key == "implement"));
    assert!(run.steps.iter().any(|step| step.key == "cleanup"));
    drop(operations);
    std::fs::remove_file(database).unwrap();
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn forked_flagship_can_replace_remove_reorder_and_add_steps_without_rust_changes() {
    let client = ExtensionClient::launch(
        env!("CARGO_BIN_EXE_prism-standard-extension"),
        Arc::new(NoHostOperations),
        HostLimits::default(),
    )
    .await
    .unwrap();
    let mut descriptor = client.descriptor().clone();
    let mut security_gate = descriptor
        .implementations
        .iter()
        .find(|implementation| implementation.id == "prism.standard/gate")
        .unwrap()
        .clone();
    security_gate.id = "acme.security/review-gate".into();
    descriptor.implementations.push(security_gate);
    let mut registry = prism::extension::DescriptorRegistry::default();
    registry.register(&descriptor).unwrap();

    let mut sources = standard_workflow_sources();
    let standard_auto = sources
        .iter()
        .find(|(name, _)| name == "auto")
        .unwrap()
        .1
        .clone();
    let notify = standard_auto.find("[[steps]]\nid = \"notify\"").unwrap();
    let mut fork = standard_auto[..notify].to_owned();
    fork = fork.replacen(
        "id = \"prism.standard/auto\"",
        "id = \"acme.delivery/secure-auto\"",
        1,
    );
    fork = fork.replace(
        "depends_on = [\"ci-gate\", \"review-gate\", \"policy-gate\", \"mergeability-gate\", \"merge-relation-gate\"]",
        "depends_on = [\"policy-gate\", \"security-review\", \"ci-gate\", \"review-gate\", \"merge-relation-gate\", \"mergeability-gate\"]",
    );
    let approval = fork.find("[[steps]]\nid = \"approve-exact-head\"").unwrap();
    fork.insert_str(
        approval,
        "[[steps]]\nid = \"security-review\"\nclass = \"gate\"\nuse = \"acme.security/review-gate\"\ndepends_on = [\"stabilize\"]\ninputs = { request = \"steps.stabilize.outputs.policy\" }\nsettings = { prompt_template = \"acme.security/review.md\" }\nskippable = false\n\n",
    );
    sources.push(("secure-auto".into(), fork));
    let catalog = prism::DefinitionCatalog::from_sources(sources, registry).unwrap();
    let snapshot = catalog.compile("acme.delivery/secure-auto").unwrap();
    assert!(
        snapshot
            .definition
            .steps
            .iter()
            .any(|step| step.id == "security-review")
    );
    assert!(
        !snapshot
            .definition
            .steps
            .iter()
            .any(|step| step.id == "notify")
    );
    assert!(
        snapshot
            .implementations
            .contains_key("acme.security/review-gate")
    );
    assert_eq!(
        snapshot
            .definition
            .steps
            .iter()
            .find(|step| step.id == "security-review")
            .unwrap()
            .settings["prompt_template"],
        "acme.security/review.md"
    );
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn standard_and_third_party_extensions_use_the_same_host_contract() {
    let fixtures = [
        (
            env!("CARGO_BIN_EXE_prism-standard-extension"),
            "prism.standard/extension",
            "prism.standard/echo",
        ),
        (
            env!("CARGO_BIN_EXE_third-party-fixture"),
            "acme.fixture/extension",
            "acme.fixture/echo",
        ),
    ];
    for (executable, extension_id, implementation_id) in fixtures {
        let client = ExtensionClient::launch(
            executable,
            Arc::new(NoHostOperations),
            HostLimits::default(),
        )
        .await
        .unwrap();
        assert_eq!(client.id(), extension_id);
        assert!(
            client
                .descriptor()
                .implementations
                .iter()
                .any(|implementation| implementation.id == implementation_id)
        );
        let (_cancellation_owner, cancellation) = tokio::sync::watch::channel(false);
        let result = client
            .execute(
                AttemptEnvelope {
                    attempt_id: format!("{extension_id}:attempt"),
                    generation: 1,
                    implementation_id: implementation_id.into(),
                    input: serde_json::json!({"message":"hello"}),
                    artifacts: Default::default(),
                },
                cancellation,
            )
            .await
            .unwrap();
        assert!(matches!(result.outcome, AttemptOutcome::Succeeded { .. }));
        assert!(
            client
                .invoke_trigger(
                    &client.descriptor().triggers[0].id,
                    serde_json::json!({"sequence":1}),
                )
                .await
                .is_ok()
        );
        assert!(
            client
                .send_notification(
                    &client.descriptor().notification_channels[0].id,
                    serde_json::json!({"body":"done"}),
                )
                .await
                .is_ok()
        );
        client.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn standard_extension_exposes_typed_input_and_structured_render_support() {
    let client = ExtensionClient::launch(
        env!("CARGO_BIN_EXE_prism-standard-extension"),
        Arc::new(NoHostOperations),
        HostLimits::default(),
    )
    .await
    .unwrap();
    client
        .validate_input(
            "prism.task/v1",
            serde_json::json!({"title":"Implement the phase"}),
        )
        .await
        .unwrap();
    let render = client
        .render_artifact(
            "prism.gate-result/v1",
            serde_json::json!({"status":"satisfied"}),
            80,
        )
        .await
        .unwrap();
    assert!(render.summary.contains("satisfied"));
    assert_eq!(render.spans[0].style, "status");
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn deliberately_unbrokered_extension_is_visibly_unprotected() {
    let client = ExtensionClient::launch(
        env!("CARGO_BIN_EXE_third-party-fixture"),
        Arc::new(NoHostOperations),
        HostLimits::default(),
    )
    .await
    .unwrap();
    let implementation = &client.descriptor().implementations[0];
    assert_eq!(
        implementation.effect_boundary,
        prism_extension_sdk::protocol::EffectBoundary::Unbrokered
    );
    assert!(!implementation.effect_boundary.has_broker_guarantees());
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_correlates_out_of_order_calls_and_recovers_after_a_crash() {
    let client = ExtensionClient::launch(
        env!("CARGO_BIN_EXE_third-party-fixture"),
        Arc::new(NoHostOperations),
        HostLimits::default(),
    )
    .await
    .unwrap();
    let (_slow_owner, slow_cancellation) = tokio::sync::watch::channel(false);
    let (_fast_owner, fast_cancellation) = tokio::sync::watch::channel(false);
    let slow = client.execute(
        attempt("slow", serde_json::json!({"delay_ms":100})),
        slow_cancellation,
    );
    let fast = client.execute(
        attempt("fast", serde_json::json!({"delay_ms":0})),
        fast_cancellation,
    );
    let (slow, fast) = tokio::join!(slow, fast);
    assert_eq!(slow.unwrap().attempt_id, "slow");
    assert_eq!(fast.unwrap().attempt_id, "fast");

    let (_crash_owner, crash_cancellation) = tokio::sync::watch::channel(false);
    assert!(
        client
            .execute(
                attempt("crash", serde_json::json!({"crash":true})),
                crash_cancellation
            )
            .await
            .is_err()
    );
    let replacement = client.restart().await.unwrap();
    replacement.heartbeat().await.unwrap();
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn calls_are_bounded_per_executable_revision() {
    use std::os::unix::fs::PermissionsExt;

    let wrapper = std::env::temp_dir().join(format!(
        "prism-extension-concurrency-{}.sh",
        std::process::id()
    ));
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n# concurrency fixture {}\nexec '{}' \"$@\"\n",
            std::process::id(),
            env!("CARGO_BIN_EXE_third-party-fixture")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let limits = HostLimits {
        max_concurrent_calls_per_revision: 1,
        ..HostLimits::default()
    };
    let client = ExtensionClient::launch(&wrapper, Arc::new(NoHostOperations), limits)
        .await
        .unwrap();
    let slow_client = client.clone();
    let slow = tokio::spawn(async move {
        let (_owner, cancellation) = tokio::sync::watch::channel(false);
        slow_client
            .execute(
                attempt("bounded-slow", serde_json::json!({"delay_ms":100})),
                cancellation,
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let (_owner, cancellation) = tokio::sync::watch::channel(false);
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(25),
            client.execute(
                attempt("bounded-fast", serde_json::json!({"delay_ms":0})),
                cancellation,
            ),
        )
        .await
        .is_err()
    );
    slow.await.unwrap().unwrap();
    client.shutdown().await.unwrap();
    std::fs::remove_file(wrapper).unwrap();
}

#[tokio::test]
async fn host_cancellation_is_attempt_and_generation_bound() {
    let client = ExtensionClient::launch(
        env!("CARGO_BIN_EXE_third-party-fixture"),
        Arc::new(NoHostOperations),
        HostLimits::default(),
    )
    .await
    .unwrap();
    let (cancel, cancellation) = tokio::sync::watch::channel(false);
    let execution = client.execute(
        attempt("cancel-me", serde_json::json!({"delay_ms":5000})),
        cancellation,
    );
    tokio::pin!(execution);
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    cancel.send(true).unwrap();
    let result = execution.await.unwrap();
    assert!(matches!(result.outcome, AttemptOutcome::Cancelled));
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancellation_race_has_one_terminal_correlated_result() {
    let client = ExtensionClient::launch(
        env!("CARGO_BIN_EXE_third-party-fixture"),
        Arc::new(NoHostOperations),
        HostLimits::default(),
    )
    .await
    .unwrap();
    for sequence in 0..20 {
        let (cancel, cancellation) = tokio::sync::watch::channel(false);
        let execution = client.execute(
            attempt(
                &format!("race-{sequence}"),
                serde_json::json!({"delay_ms":0}),
            ),
            cancellation,
        );
        tokio::pin!(execution);
        let _ = cancel.send(true);
        let result = execution.await.unwrap();
        assert_eq!(result.attempt_id, format!("race-{sequence}"));
        assert_eq!(result.generation, 1);
    }
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn supervisor_detects_a_crash_and_restarts_the_pinned_executable() {
    let supervisor = ExtensionSupervisor::launch(
        env!("CARGO_BIN_EXE_third-party-fixture"),
        Arc::new(NoHostOperations),
        HostLimits {
            heartbeat_interval: std::time::Duration::from_millis(20),
            heartbeat_timeout: std::time::Duration::from_millis(20),
            ..HostLimits::default()
        },
    )
    .await
    .unwrap();
    let initial_revision = supervisor.client().await.revision().to_owned();
    let (_owner, cancellation) = tokio::sync::watch::channel(false);
    assert!(
        supervisor
            .execute(
                attempt("supervised-crash", serde_json::json!({"crash":true})),
                cancellation,
            )
            .await
            .is_err()
    );
    // Restart re-verifies the complete executable digest before activation. Debug binaries can
    // be large on constrained builders, so keep this above the handshake timeout.
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let current = supervisor.client().await;
            if !current.is_failed() && current.heartbeat().await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(supervisor.client().await.revision(), initial_revision);
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn stdout_contamination_is_rejected_during_handshake() {
    let error = ExtensionClient::launch(
        env!("CARGO_BIN_EXE_stdout-contamination-fixture"),
        Arc::new(NoHostOperations),
        HostLimits::default(),
    )
    .await
    .err()
    .expect("contaminated stdout must fail");
    assert!(error.to_string().contains("malformed extension frame"));
}

#[tokio::test]
async fn heartbeat_timeout_terminates_an_unresponsive_extension() {
    let client = ExtensionClient::launch(
        env!("CARGO_BIN_EXE_unresponsive-fixture"),
        Arc::new(NoHostOperations),
        HostLimits {
            heartbeat_timeout: std::time::Duration::from_millis(20),
            shutdown_timeout: std::time::Duration::from_millis(20),
            ..HostLimits::default()
        },
    )
    .await
    .unwrap();
    assert!(client.heartbeat().await.is_err());
    client.shutdown().await.unwrap();
}

#[test]
fn platform_contract_extension_process_host_is_supported() {
    assert!(std::path::Path::new(env!("CARGO_BIN_EXE_third-party-fixture")).is_file());
}

#[tokio::test]
async fn old_pinned_executable_runs_after_working_copy_removal() {
    let root = std::env::temp_dir().join(format!("prism-pinned-extension-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let operations = ExtensionOperations::new(root.join("work"), root.join("state"));
    let (revision, retained) = operations
        .snapshot_executable(
            "acme.fixture/extension",
            env!("CARGO_BIN_EXE_third-party-fixture"),
        )
        .unwrap();
    operations.pin_executable("run-pinned", &revision).unwrap();
    operations
        .remove_working_copy("acme.fixture/extension")
        .unwrap();
    let client =
        ExtensionClient::launch(retained, Arc::new(NoHostOperations), HostLimits::default())
            .await
            .unwrap();
    assert_eq!(client.id(), "acme.fixture/extension");
    client.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
