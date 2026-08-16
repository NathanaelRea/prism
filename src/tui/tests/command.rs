use crossterm::event::KeyCode;

use super::super::PanelFocus;
use super::super::command::{CommandOutcome, CommandState, DashboardCommand};
use super::support::{ScriptedTerminal, test_tui};

#[test]
fn semantic_commands_dispatch_without_the_crossterm_adapter() {
    let mut tui = test_tui();
    let mut terminal = ScriptedTerminal::default();
    let mut state = CommandState::default();

    let outcome = tui
        .dispatch_command(&mut terminal, DashboardCommand::FocusRepos, &mut state)
        .unwrap();

    assert_eq!(outcome, CommandOutcome::Continue);
    assert_eq!(tui.focused_panel, PanelFocus::Repos);
    assert_eq!(terminal.draws, 0);

    tui.dispatch_command(&mut terminal, DashboardCommand::FocusMerges, &mut state)
        .expect("focus merges");
    assert_eq!(tui.focused_panel, PanelFocus::Merges);
}

#[test]
fn command_state_is_retained_across_dispatches() {
    let mut tui = test_tui();
    let mut terminal = ScriptedTerminal::default();
    let mut state = CommandState::default();

    for command in [
        DashboardCommand::FocusWorktrees,
        DashboardCommand::PreviousView,
        DashboardCommand::Bottom,
    ] {
        tui.dispatch_command(&mut terminal, command, &mut state)
            .unwrap();
    }
    assert_eq!(tui.selected_worktree_index(), Some(3));

    tui.dispatch_command(&mut terminal, DashboardCommand::G, &mut state)
        .unwrap();
    assert_eq!(tui.selected_worktree_index(), Some(3));

    tui.dispatch_command(&mut terminal, DashboardCommand::G, &mut state)
        .unwrap();
    assert_eq!(tui.selected_worktree_index(), Some(1));
}

#[test]
fn interactive_commands_use_the_terminal_driver_seam() {
    let mut tui = test_tui();
    let mut terminal = ScriptedTerminal::default();
    terminal.push_key(KeyCode::Enter);
    let mut state = CommandState::default();

    let outcome = tui
        .dispatch_command(&mut terminal, DashboardCommand::Help, &mut state)
        .unwrap();

    assert_eq!(outcome, CommandOutcome::Continue);
    assert!(terminal.draws >= 2);
    assert!(tui.dialog.is_none());
}

#[test]
fn suspended_operations_use_the_terminal_driver_seam() {
    let mut terminal = ScriptedTerminal::default();

    let value = crate::tui_runtime::suspend_for(&mut terminal, || Ok(42)).unwrap();

    assert_eq!(value, 42);
    assert_eq!(terminal.suspends, 1);
    assert_eq!(terminal.resumes, 1);
}
