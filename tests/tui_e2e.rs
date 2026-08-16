#[allow(dead_code, unused_imports)]
#[path = "common/e2e/mod.rs"]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use support::{E2eSandbox, capture_pane, read_events, run_tmux, wait_until};

#[test]
#[ignore = "full-gate controller tmux test"]
fn dashboard_attaches_pushes_merges_and_quits_from_physical_keys() {
    let sandbox = E2eSandbox::new("black-box-attach");
    let attached_path = sandbox.worktrees.join("feature-attach");
    let created = sandbox.git(
        &sandbox.repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature/attach",
            attached_path.to_str().unwrap(),
            "main",
        ],
    );
    assert!(created.status.success());
    fs::write(attached_path.join("remote.txt"), "black-box remote flow\n").unwrap();
    assert!(
        sandbox
            .git(&attached_path, &["add", "remote.txt"])
            .status
            .success()
    );
    assert!(
        sandbox
            .git(
                &attached_path,
                &["commit", "-m", "exercise black-box remote flow"]
            )
            .status
            .success()
    );
    let head = sandbox.git_stdout(&attached_path, &["rev-parse", "HEAD"]);
    let controller_socket = sandbox.controller_socket.clone();
    let prism = PathBuf::from(env!("CARGO_BIN_EXE_prism"));

    let status = sandbox
        .command(&sandbox.real_tmux)
        .args([
            "-S",
            controller_socket.to_str().unwrap(),
            "new-session",
            "-d",
            "-x",
            "100",
            "-y",
            "30",
            "-s",
            "controller",
            "-c",
            sandbox.repo.to_str().unwrap(),
            prism.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());

    wait_until(
        Duration::from_secs(8),
        "repository onboarding prompt",
        || {
            capture_pane(&sandbox.real_tmux, &controller_socket, "controller:0")
                .contains("Base/main path")
                .then_some(())
        },
    );
    let path = run_tmux(
        &sandbox.real_tmux,
        &controller_socket,
        &[
            "send-keys",
            "-l",
            "-t",
            "controller:0",
            sandbox.repo.to_str().unwrap(),
        ],
    );
    assert!(path.status.success());
    let enter = run_tmux(
        &sandbox.real_tmux,
        &controller_socket,
        &["send-keys", "-t", "controller:0", "Enter"],
    );
    assert!(enter.status.success());

    let screen = wait_until(Duration::from_secs(8), "dashboard first frame", || {
        let screen = capture_pane(&sandbox.real_tmux, &controller_socket, "controller:0");
        (screen.contains("Status") && screen.contains("Repos") && screen.contains("Worktrees"))
            .then_some(screen)
    });
    assert!(screen.contains("Worktrees"));
    wait_until(Duration::from_secs(8), "linked worktree inventory", || {
        let screen = capture_pane(&sandbox.real_tmux, &controller_socket, "controller:0");
        screen.contains("feature/attach").then_some(())
    });

    let focus = run_tmux(
        &sandbox.real_tmux,
        &controller_socket,
        &["send-keys", "-t", "controller:0", "3"],
    );
    assert!(focus.status.success());
    wait_until(Duration::from_secs(8), "worktree panel focus", || {
        let screen = capture_pane(&sandbox.real_tmux, &controller_socket, "controller:0");
        (screen.contains("feature/attach") && screen.contains("Open Enter")).then_some(())
    });
    let open = run_tmux(
        &sandbox.real_tmux,
        &controller_socket,
        &["send-keys", "-t", "controller:0", "Enter"],
    );
    assert!(open.status.success());
    wait_until(Duration::from_secs(8), "harness migration choice", || {
        capture_pane(&sandbox.real_tmux, &controller_socket, "controller:0")
            .contains("Worktree Harness Changed")
            .then_some(())
    });
    let migrate = run_tmux(
        &sandbox.real_tmux,
        &controller_socket,
        &["send-keys", "-t", "controller:0", "m"],
    );
    assert!(migrate.status.success());
    let event = wait_until(Duration::from_secs(8), "migrated harness event", || {
        read_events(&sandbox.events_path())
            .into_iter()
            .find(|event| {
                event["tool"] == "harness" && event["cwd"].as_str() == attached_path.to_str()
            })
    });
    assert_eq!(event["argv"], serde_json::json!(["interactive"]));
    wait_until(Duration::from_secs(8), "nested harness attachment", || {
        capture_pane(&sandbox.real_tmux, &controller_socket, "controller:0")
            .contains("PRISM_E2E_HARNESS_READY")
            .then_some(())
    });

    let detach = run_tmux(
        &sandbox.real_tmux,
        &controller_socket,
        &["send-keys", "-t", "controller:0", "C-b", "d"],
    );
    assert!(detach.status.success());
    wait_until(Duration::from_secs(8), "dashboard after detach", || {
        let screen = capture_pane(&sandbox.real_tmux, &controller_socket, "controller:0");
        (screen.contains("feature/attach") && screen.contains("Quit q")).then_some(())
    });

    let push = run_tmux(
        &sandbox.real_tmux,
        &controller_socket,
        &["send-keys", "-t", "controller:0", "Space", "g", "P"],
    );
    assert!(push.status.success());
    wait_until(
        Duration::from_secs(30),
        "pull request description prompt",
        || {
            capture_pane(&sandbox.real_tmux, &controller_socket, "controller:0")
                .contains("Create Pull Request")
                .then_some(())
        },
    );
    let body = "Black-box pull request body";
    let description = run_tmux(
        &sandbox.real_tmux,
        &controller_socket,
        &["send-keys", "-l", "-t", "controller:0", body],
    );
    assert!(description.status.success());
    let submit_description = run_tmux(
        &sandbox.real_tmux,
        &controller_socket,
        &["send-keys", "-t", "controller:0", "Enter"],
    );
    assert!(submit_description.status.success());
    wait_until(
        Duration::from_secs(30),
        "push and pull request creation",
        || {
            let state = fs::read_to_string(sandbox.state.join("github.json")).ok()?;
            let state: serde_json::Value = serde_json::from_str(&state).ok()?;
            let pull = state["pull_requests"].as_array()?.first()?;
            let pushed = sandbox.git(&sandbox.origin, &["rev-parse", "refs/heads/feature/attach"]);
            (pull["head"] == "feature/attach"
                && pull["base"] == "main"
                && pull["body"] == body
                && pull["head_sha"] == head
                && pushed.status.success()
                && String::from_utf8_lossy(&pushed.stdout).trim() == head
                && runtime_log_contains(sandbox.root(), "push complete; pull request created"))
            .then_some(())
        },
    );
    let create_event = read_events(&sandbox.events_path())
        .into_iter()
        .find(|event| {
            event["tool"] == "gh"
                && event["argv"].as_array().is_some_and(|argv| {
                    argv.first().and_then(|value| value.as_str()) == Some("pr")
                        && argv.get(1).and_then(|value| value.as_str()) == Some("create")
                })
        })
        .expect("gh pull request creation invocation");
    assert!(
        create_event["argv"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(body))
    );

    let merge = run_tmux(
        &sandbox.real_tmux,
        &controller_socket,
        &["send-keys", "-t", "controller:0", "Space", "g", "M"],
    );
    assert!(merge.status.success());
    wait_until(
        Duration::from_secs(30),
        "guarded pull request merge",
        || {
            let state = fs::read_to_string(sandbox.state.join("github.json")).ok()?;
            let state: serde_json::Value = serde_json::from_str(&state).ok()?;
            (state["pull_requests"][0]["merged"] == true).then_some(())
        },
    );
    let merge_event = read_events(&sandbox.events_path())
        .into_iter()
        .find(|event| {
            event["tool"] == "gh"
                && event["argv"].as_array().is_some_and(|argv| {
                    argv.first().and_then(|value| value.as_str()) == Some("pr")
                        && argv.get(1).and_then(|value| value.as_str()) == Some("merge")
                })
        })
        .expect("guarded gh merge invocation");
    assert_eq!(merge_event["argv"][4], "--match-head-commit");
    assert_eq!(merge_event["argv"][5], head);
    wait_until(Duration::from_secs(8), "merge confirmation", || {
        runtime_log_contains(sandbox.root(), "pull request merged").then_some(())
    });

    let quit = run_tmux(
        &sandbox.real_tmux,
        &controller_socket,
        &["send-keys", "-t", "controller:0", "q"],
    );
    assert!(quit.status.success());
    let _ = run_tmux(
        &sandbox.real_tmux,
        &controller_socket,
        &["send-keys", "-t", "controller:0", "y"],
    );
    wait_until(Duration::from_secs(8), "dashboard process exit", || {
        let output = run_tmux(
            &sandbox.real_tmux,
            &controller_socket,
            &["has-session", "-t", "controller"],
        );
        (!output.status.success()).then_some(())
    });

    sandbox.assert_clean_adapters();
}

fn runtime_log_contains(root: &Path, needle: &str) -> bool {
    let Ok(entries) = fs::read_dir(root.join("config/prism/repos")) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        fs::read_to_string(entry.path().join("runtime.log")).is_ok_and(|log| log.contains(needle))
    })
}
