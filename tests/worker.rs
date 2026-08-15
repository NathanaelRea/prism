#![cfg(unix)]

mod common;

use std::process::{Command, Output};

use common::CompactTempDir as TempDir;

fn prism(temp: &TempDir, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_prism"))
        .args(args)
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
        .env("PRISM_RUNTIME_DIR", temp.runtime_path())
        .output()
        .unwrap()
}

#[test]
fn prompt_worker_starts_once_lists_editable_defaults_and_stops() {
    let temp = TempDir::new("prompt-worker");
    let first = prism(&temp, &["worker", "ensure"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = prism(&temp, &["worker", "ensure"]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );

    let health = prism(&temp, &["worker", "health"]);
    let health = String::from_utf8_lossy(&health.stdout);
    assert!(health.starts_with("ok 6 "), "unexpected health: {health}");
    assert!(health.contains("state=running"));

    let list = prism(&temp, &["workflow", "list", "--json"]);
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(value["kind"], "workflow.list");
    assert!(
        value["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "stabilize")
    );
    assert!(
        temp.path()
            .join("config/prism/workflows/stabilize.toml")
            .is_file()
    );

    let shutdown = prism(&temp, &["worker", "shutdown"]);
    assert!(
        shutdown.status.success(),
        "{}",
        String::from_utf8_lossy(&shutdown.stderr)
    );
    let stopped = prism(&temp, &["worker", "health"]);
    assert!(String::from_utf8_lossy(&stopped.stdout).is_empty() || !stopped.status.success());
}
