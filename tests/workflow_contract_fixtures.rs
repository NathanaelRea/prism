use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const FIXTURE_DIR: &str = "tests/fixtures/workflow-acceptance";
const ACCEPTANCE_FIXTURES: [&str; 10] = [
    "context-launch-missing-input.json",
    "explicit-dag-branches-and-joins.json",
    "bounded-stabilization-iterations.json",
    "addressed-thread-resolution.json",
    "extension-crash-cancel-restart.json",
    "package-customize-update-conflict.json",
    "repository-trust-invalidation.json",
    "schedule-restart-deduplication.json",
    "github-issue-admission-unique-implementation.json",
    "exact-head-merge-and-cleanup.json",
];

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_DIR)
        .join(name)
}

#[test]
fn generalized_workflow_acceptance_targets_are_named_and_well_formed() {
    let mut contracts = BTreeSet::new();
    for name in ACCEPTANCE_FIXTURES {
        let source = fs::read_to_string(fixture_path(name)).expect("acceptance fixture must exist");
        let fixture: serde_json::Value =
            serde_json::from_str(&source).expect("acceptance fixture must be valid JSON");
        assert_eq!(fixture["fixture_schema_version"], 1, "fixture {name}");
        let contract = fixture["contract"]
            .as_str()
            .expect("acceptance fixture must name its contract");
        assert!(
            contracts.insert(contract.to_owned()),
            "duplicate {contract}"
        );
        assert!(
            fixture["expect"]
                .as_array()
                .is_some_and(|events| !events.is_empty()),
            "fixture {name} must declare expected events"
        );
    }
    assert_eq!(contracts.len(), ACCEPTANCE_FIXTURES.len());
}

#[test]
fn accepted_decisions_are_indexed_and_contract_versions_are_fixed() {
    let index_source =
        fs::read_to_string(fixture_path("contract-index.toml")).expect("contract index must exist");
    let index: toml::Value = toml::from_str(&index_source).expect("contract index must be TOML");
    let decisions = index["decision"]
        .as_array()
        .expect("contract index must contain decisions");
    assert_eq!(
        decisions.len(),
        18,
        "accepted product decision count changed"
    );

    let mut ids = BTreeSet::new();
    for decision in decisions {
        let table = decision.as_table().expect("decision must be a table");
        let id = table["id"].as_str().expect("decision must have an id");
        assert!(ids.insert(id), "duplicate accepted decision {id}");
        assert!(
            table["contract"]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "decision {id} must name a contract"
        );
        let fixture = table["fixture"]
            .as_str()
            .expect("decision must reference a fixture");
        assert!(fixture_path(fixture).is_file(), "missing fixture {fixture}");
    }

    let versions_source = fs::read_to_string(fixture_path("contract-versions.json"))
        .expect("contract versions fixture must exist");
    let versions: serde_json::Value =
        serde_json::from_str(&versions_source).expect("contract versions must be JSON");
    assert_eq!(versions["workflow_definition"], 2);
    assert_eq!(versions["package_manifest"], 1);
    assert_eq!(versions["lockfile"], 1);
    assert_eq!(versions["extension_protocol"]["major"], 1);
    assert_eq!(versions["extension_protocol"]["minor"], 0);
    assert_eq!(versions["stable_json_envelope"], 1);
}

#[test]
fn legacy_inventory_is_deletion_only() {
    let inventory = fs::read_to_string(fixture_path("legacy-deletion-targets.txt"))
        .expect("legacy deletion inventory must exist");
    assert!(inventory.contains("not migration/compatibility contracts"));
    assert!(inventory.contains("src/auto_flow/"));
    assert!(inventory.contains("src/plan_run/"));
}
