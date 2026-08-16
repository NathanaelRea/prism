#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::config::Config;
use crate::harness::{HarnessConfig, OutputFormat, PromptTransport};
use crate::repo::Repository;

use super::super::command::{CommandState, DashboardCommand};
use super::super::{PanelFocus, Tui};
use super::support::ScriptedTerminal;

#[allow(dead_code, unused_imports)]
#[path = "../../../tests/common/e2e/mod.rs"]
mod support;

use support::{E2eSandbox, read_events, run_tmux, wait_until};

fn e2e_config(sandbox: &E2eSandbox) -> Config {
    let mut config = crate::test_support::test_config();
    config.default_harness = "e2e".to_string();
    config.default_agent = "e2e".to_string();
    config.default_base = Some("main".to_string());
    config.worktree_command = "wt".to_string();
    config.tools.insert(
        "git".to_string(),
        sandbox.bin.join("git-proxy").display().to_string(),
    );
    config.tools.insert(
        "gh".to_string(),
        sandbox.bin.join("gh").display().to_string(),
    );
    config.tools.insert(
        "wt".to_string(),
        sandbox.bin.join("wt").display().to_string(),
    );
    config.tools.insert(
        "tmux".to_string(),
        sandbox.bin.join("tmux").display().to_string(),
    );
    config.harnesses.insert(
        "e2e".to_string(),
        HarnessConfig {
            adapter: "generic".to_string(),
            interactive_command: vec![
                sandbox.bin.join("harness").display().to_string(),
                "interactive".to_string(),
                "{prompt}".to_string(),
            ],
            arguments: Vec::new(),
            interactive_prompt_transport: Some(PromptTransport::Argument),
            headless_command: Some(vec![
                sandbox.bin.join("harness").display().to_string(),
                "headless".to_string(),
            ]),
            headless_prompt_transport: Some(PromptTransport::Stdin),
            output_format: OutputFormat::Text,
            environment: BTreeMap::new(),
        },
    );
    config
}

fn e2e_tui(sandbox: &E2eSandbox) -> Tui {
    let repo =
        Repository::with_config_dir_for_test(sandbox.repo.clone(), sandbox.state.join("prism"));
    fs::create_dir_all(repo.prism_dir()).unwrap();
    fs::write(
        repo.prism_dir().join("config.toml"),
        "this is deliberately invalid TOML = [\n",
    )
    .unwrap();
    let config = e2e_config(sandbox);
    let sessions = crate::session::discover_sessions(&repo, &config).unwrap();
    Tui::new_single(repo, config, sessions)
}

#[test]
fn create_command_uses_real_git_worktree_and_warms_the_selected_harness() {
    let sandbox = E2eSandbox::new("create-command");
    let mut tui = e2e_tui(&sandbox);
    let mut terminal = ScriptedTerminal::default();
    terminal.queue_text_dialog("feature/e2e-create");
    terminal.queue_create_session_form("");
    let mut state = CommandState::default();

    tui.dispatch_command(&mut terminal, DashboardCommand::Create, &mut state)
        .unwrap();

    let created = sandbox.worktrees.join("feature-e2e-create");
    let inventory = sandbox.git_stdout(&sandbox.repo, &["worktree", "list", "--porcelain"]);
    assert!(
        created.is_dir(),
        "created path missing: {}",
        created.display()
    );
    assert!(inventory.contains(&format!("worktree {}", created.display())));
    assert!(inventory.contains("branch refs/heads/feature/e2e-create"));
    assert_eq!(
        tui.focused_panel,
        PanelFocus::Worktrees,
        "create command status: {:?}",
        tui.status_message
    );
    assert_eq!(
        tui.selected_worktree_index()
            .map(|index| tui.sessions[index].branch.as_str()),
        Some("feature/e2e-create")
    );
    assert_eq!(
        crate::session::worktree_harness(
            &tui.repo,
            &tui.sessions[tui.selected_worktree_index().unwrap()],
        )
        .unwrap()
        .harness_id,
        "e2e"
    );

    assert!(
        !tui.tmux_warmups_in_flight.is_empty(),
        "create did not schedule warmup; harness configs={:?}",
        tui.worktree_harness_configs.keys().collect::<Vec<_>>()
    );
    let event = wait_until(Duration::from_secs(8), "detached harness warmup", || {
        tui.tick_tui_action_jobs();
        let event = read_events(&sandbox.events_path())
            .into_iter()
            .find(|event| {
                event["tool"] == "harness" && event["argv"] == serde_json::json!(["interactive"])
            });
        assert!(
            event.is_some() || !tui.tmux_warmups_in_flight.is_empty(),
            "warmup completed without launching harness: {:?}",
            tui.status_message
        );
        event
    });
    assert_eq!(
        Path::new(event["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        created.canonicalize().unwrap()
    );
    assert!(
        sandbox
            .events()
            .iter()
            .all(|event| event["tool"] != "harness-stdin"),
        "an empty initial prompt must not be submitted"
    );
    assert!(!sandbox.prism_tmux_sessions().is_empty());
    sandbox.assert_clean_adapters();
}

#[test]
fn permanent_delete_removes_only_selected_worktree_branch_and_tmux_session() {
    let sandbox = E2eSandbox::new("delete-command");
    let mut tui = e2e_tui(&sandbox);
    let mut terminal = ScriptedTerminal::default();
    terminal.queue_text_dialog("feature/e2e-delete");
    terminal.queue_create_session_form("");
    let mut state = CommandState::default();
    tui.dispatch_command(&mut terminal, DashboardCommand::Create, &mut state)
        .unwrap();

    wait_until(Duration::from_secs(8), "created session warmup", || {
        tui.tick_tui_action_jobs();
        (tui.tmux_warmups_in_flight.is_empty()
            && read_events(&sandbox.events_path())
                .iter()
                .any(|event| event["tool"] == "harness"))
        .then_some(())
    });
    let deleted_path = tui.sessions[tui.selected].path.clone();
    let deleted_tmux = sandbox
        .prism_tmux_sessions()
        .into_iter()
        .find(|name| name.contains("e2e-delete"))
        .unwrap();

    let keep_path = sandbox.worktrees.join("keep");
    sandbox.git(
        &sandbox.repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature/keep",
            keep_path.to_str().unwrap(),
            "main",
        ],
    );
    let unrelated_tmux = run_tmux(
        &sandbox.real_tmux,
        &sandbox.prism_socket,
        &["new-session", "-d", "-s", "unrelated-e2e", "sleep", "60"],
    );
    assert!(unrelated_tmux.status.success());

    terminal.queue_confirmation(true);
    tui.dispatch_command(&mut terminal, DashboardCommand::DeletePermanent, &mut state)
        .unwrap();
    wait_until(Duration::from_secs(8), "permanent deletion", || {
        tui.tick_tui_action_jobs();
        (tui.delete_sessions_in_flight.is_empty()
            && !deleted_path.exists()
            && tui.selected < tui.sessions.len()
            && tui
                .sessions
                .iter()
                .all(|session| session.branch != "feature/e2e-delete"))
        .then_some(())
    });

    let branches = sandbox.git_stdout(&sandbox.repo, &["branch", "--format=%(refname:short)"]);
    assert!(!branches.contains("feature/e2e-delete"));
    assert!(branches.contains("feature/keep"));
    assert!(keep_path.exists());
    let sessions = sandbox.prism_tmux_sessions();
    assert!(!sessions.contains(&deleted_tmux));
    assert!(sessions.contains(&"unrelated-e2e".to_string()));
    assert_ne!(tui.sessions[tui.selected].branch, "feature/e2e-delete");
    sandbox.assert_clean_adapters();
}

#[test]
fn create_with_multiline_prompt_delivers_literal_input_once_to_generic_harness() {
    let sandbox = E2eSandbox::new("create-prompt");
    let mut tui = e2e_tui(&sandbox);
    let mut terminal = ScriptedTerminal::default();
    terminal.queue_text_dialog("feature/e2e-prompt");
    let marker = sandbox.state.join("shell-evaluated");
    let prompt = format!(
        "first line\nsecond line; $(touch {}) 'quoted'",
        marker.display()
    );
    terminal.queue_create_session_form(&prompt);
    let mut state = CommandState::default();

    tui.dispatch_command(&mut terminal, DashboardCommand::Create, &mut state)
        .unwrap();

    let event = wait_until(Duration::from_secs(8), "generic harness prompt", || {
        tui.tick_tui_action_jobs();
        read_events(&sandbox.events_path())
            .into_iter()
            .find(|event| {
                event["tool"] == "harness"
                    && event["argv"]
                        .as_array()
                        .is_some_and(|argv| argv.iter().any(|arg| arg.as_str() == Some(&prompt)))
            })
    });
    let argv = event["argv"].as_array().unwrap();
    assert_eq!(
        argv.iter()
            .filter(|arg| arg.as_str() == Some(&prompt))
            .count(),
        1,
        "the initial prompt must be transported exactly once"
    );
    assert_eq!(
        event["cwd"].as_str(),
        Some(tui.sessions[tui.selected].path.to_str().unwrap())
    );
    assert!(
        !marker.exists(),
        "prompt content was evaluated as shell code"
    );

    let target = sandbox
        .prism_tmux_sessions()
        .into_iter()
        .find(|name| name.contains("e2e-prompt"))
        .expect("created worktree tmux session");
    let windows = run_tmux(
        &sandbox.real_tmux,
        &sandbox.prism_socket,
        &[
            "list-windows",
            "-t",
            &target,
            "-F",
            "#{window_index}:#{window_name}",
        ],
    );
    assert!(windows.status.success());
    assert!(
        String::from_utf8_lossy(&windows.stdout)
            .lines()
            .any(|window| window == "1:e2e")
    );
    sandbox.assert_clean_adapters();
}

#[test]
fn scripted_terminal_supports_lifecycle_failures_and_frame_summaries() {
    let mut tui = super::support::test_tui();
    let mut terminal = ScriptedTerminal::with_size(90, 24);
    terminal.push_focus_lost();
    terminal.push_resize(100, 30);
    terminal.push_focus_gained();
    terminal.push_modified_key(KeyCode::Enter, KeyModifiers::NONE);
    let mut state = CommandState::default();

    tui.dispatch_command(&mut terminal, DashboardCommand::Help, &mut state)
        .unwrap();
    assert!(terminal.frames.iter().any(|frame| {
        frame.dialog_title.as_deref() == Some("Help") && frame.focus == PanelFocus::Repos
    }));

    terminal.fail_next_suspend("injected suspend failure");
    let error = crate::tui_runtime::suspend_for(&mut terminal, || Ok(())).unwrap_err();
    assert_eq!(error, "injected suspend failure");
    terminal.fail_next_resume("injected resume failure");
    let error = crate::tui_runtime::suspend_for(&mut terminal, || Ok(())).unwrap_err();
    assert_eq!(error, "injected resume failure");

    let mut draw_failure = ScriptedTerminal::default();
    draw_failure.fail_next_draw("injected draw failure");
    draw_failure.push_key(KeyCode::Enter);
    let error = tui
        .dispatch_command(&mut draw_failure, DashboardCommand::Help, &mut state)
        .unwrap_err();
    assert_eq!(error, "injected draw failure");

    let mut poll_failure = ScriptedTerminal::default();
    poll_failure.fail_next_poll("injected poll failure");
    let error = tui
        .dispatch_command(&mut poll_failure, DashboardCommand::Help, &mut state)
        .unwrap_err();
    assert_eq!(error, "injected poll failure");
}
