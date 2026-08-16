use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::DashboardCommand;

#[derive(Default)]
pub struct KeyInput {
    state: KeyInputState,
}

#[derive(Default)]
enum KeyInputState {
    #[default]
    Normal,
    Leader,
    LeaderG,
    LeaderWorkflow,
}

impl KeyInput {
    pub fn map_event(&mut self, event: KeyEvent) -> Option<DashboardCommand> {
        if event.kind != KeyEventKind::Press {
            return None;
        }

        Some(match self.state {
            KeyInputState::Normal => self.map_normal(event),
            KeyInputState::Leader => self.map_leader(event),
            KeyInputState::LeaderG => self.map_leader_git(event),
            KeyInputState::LeaderWorkflow => self.map_leader_workflow(event),
        })
    }

    fn map_normal(&mut self, event: KeyEvent) -> DashboardCommand {
        if is_ctrl_char(event, 'c') {
            return DashboardCommand::Quit;
        }
        if is_terminal_key(event) {
            return DashboardCommand::Terminal;
        }

        match event.code {
            KeyCode::Char('q') if plain_char(event) => DashboardCommand::Quit,
            KeyCode::Char(' ') if plain_char(event) => {
                self.state = KeyInputState::Leader;
                DashboardCommand::Leader
            }
            KeyCode::Tab => DashboardCommand::FocusNext,
            KeyCode::BackTab => DashboardCommand::FocusPrevious,
            KeyCode::Enter => DashboardCommand::OpenTmuxSession,
            KeyCode::Up => DashboardCommand::Up,
            KeyCode::Down => DashboardCommand::Down,
            KeyCode::Left => DashboardCommand::Left,
            KeyCode::Right => DashboardCommand::Right,
            KeyCode::Char('k') if plain_char(event) => DashboardCommand::Up,
            KeyCode::Char('j') if plain_char(event) => DashboardCommand::Down,
            KeyCode::Char('h') if plain_char(event) => DashboardCommand::Left,
            KeyCode::Char('l') if plain_char(event) => DashboardCommand::Right,
            KeyCode::Char('G') if plain_char(event) => DashboardCommand::Bottom,
            KeyCode::Char('g') if plain_char(event) => DashboardCommand::G,
            KeyCode::Char('{') if plain_char(event) => DashboardCommand::PreviousBlock,
            KeyCode::Char('}') if plain_char(event) => DashboardCommand::NextBlock,
            KeyCode::Char('[') if plain_char(event) => DashboardCommand::PreviousView,
            KeyCode::Char(']') if plain_char(event) => DashboardCommand::NextView,
            KeyCode::Char('r') if plain_char(event) => DashboardCommand::Refresh,
            KeyCode::Char('o') if plain_char(event) => DashboardCommand::OpenDevelopmentUrl,
            KeyCode::Char('L') if plain_char(event) => DashboardCommand::WorktrunkLogs,
            KeyCode::Char('>') if plain_char(event) => DashboardCommand::VisibilityUp,
            KeyCode::Char('<') if plain_char(event) => DashboardCommand::VisibilityDown,
            KeyCode::Char('0') if plain_char(event) => DashboardCommand::FocusMain,
            KeyCode::Char('1') if plain_char(event) => DashboardCommand::FocusStatus,
            KeyCode::Char('2') if plain_char(event) => DashboardCommand::FocusRepos,
            KeyCode::Char('3') if plain_char(event) => DashboardCommand::FocusWorktrees,
            KeyCode::Char('4'..='9') if plain_char(event) => DashboardCommand::Other,
            KeyCode::Char('p') if plain_char(event) => DashboardCommand::PullDefault,
            KeyCode::Char('W') if plain_char(event) => DashboardCommand::WorkflowLauncher,
            KeyCode::Char('u') if plain_char(event) => DashboardCommand::WorkflowPauseResume,
            KeyCode::Char('f') if plain_char(event) => DashboardCommand::WorkflowRetry,
            KeyCode::Char('c') if plain_char(event) => DashboardCommand::Create,
            KeyCode::Char('x') if plain_char(event) => DashboardCommand::AbortOpencode,
            KeyCode::Char('X') if plain_char(event) => DashboardCommand::DeletePermanent,
            KeyCode::Char('C') if plain_char(event) => DashboardCommand::OpenRemotePrs,
            KeyCode::Char('D') if plain_char(event) => DashboardCommand::Delete,
            KeyCode::Char('U') if plain_char(event) => DashboardCommand::Unarchive,
            KeyCode::Char('M') if plain_char(event) => DashboardCommand::MigrateHarness,
            KeyCode::Char('?') if plain_char(event) => DashboardCommand::Help,
            KeyCode::Char('/') if plain_char(event) => DashboardCommand::Search,
            _ => DashboardCommand::Other,
        }
    }

    fn map_leader(&mut self, event: KeyEvent) -> DashboardCommand {
        match event.code {
            KeyCode::Char(' ') if plain_char(event) => {
                self.state = KeyInputState::Normal;
                DashboardCommand::OpenTmuxSession
            }
            KeyCode::Char('w') if plain_char(event) => {
                self.state = KeyInputState::LeaderWorkflow;
                DashboardCommand::LeaderWorkflow
            }
            KeyCode::Char('c') if plain_char(event) => {
                self.state = KeyInputState::Normal;
                DashboardCommand::Configuration
            }
            KeyCode::Enter => {
                self.state = KeyInputState::Normal;
                DashboardCommand::Terminal
            }
            KeyCode::Char('g') if plain_char(event) => {
                self.state = KeyInputState::LeaderG;
                DashboardCommand::LeaderGit
            }
            KeyCode::Char(key @ '1'..='9') if plain_char(event) => {
                self.state = KeyInputState::Normal;
                DashboardCommand::RepoShortcut(key)
            }
            _ => {
                self.state = KeyInputState::Normal;
                DashboardCommand::Other
            }
        }
    }

    fn map_leader_workflow(&mut self, event: KeyEvent) -> DashboardCommand {
        self.state = KeyInputState::Normal;
        match event.code {
            KeyCode::Char('a') if plain_char(event) => DashboardCommand::WorkflowAi,
            KeyCode::Char('w') if plain_char(event) => DashboardCommand::WorkflowLauncher,
            _ => DashboardCommand::Other,
        }
    }

    fn map_leader_git(&mut self, event: KeyEvent) -> DashboardCommand {
        self.state = KeyInputState::Normal;
        match event.code {
            KeyCode::Char('g') if plain_char(event) => DashboardCommand::LazyGit,
            KeyCode::Char('a') if plain_char(event) => DashboardCommand::Other,
            KeyCode::Char('o') if plain_char(event) => DashboardCommand::OpenPr,
            KeyCode::Char('v') if plain_char(event) => DashboardCommand::SubmitReview,
            KeyCode::Char('P') if plain_char(event) => DashboardCommand::Push,
            KeyCode::Char('M') if plain_char(event) => DashboardCommand::Merge,
            KeyCode::Char('c') if plain_char(event) => DashboardCommand::CiFix,
            KeyCode::Char('f') if plain_char(event) => DashboardCommand::ReviewFix,
            KeyCode::Char('R') if plain_char(event) => DashboardCommand::ResolveAllComments,
            KeyCode::Char('p') if plain_char(event) => DashboardCommand::PullDefault,
            _ => DashboardCommand::Other,
        }
    }
}

fn plain_char(event: KeyEvent) -> bool {
    event
        .modifiers
        .intersection(KeyModifiers::CONTROL | KeyModifiers::ALT)
        .is_empty()
}

fn is_ctrl_char(event: KeyEvent, ch: char) -> bool {
    matches!(event.code, KeyCode::Char(value) if value.eq_ignore_ascii_case(&ch))
        && event.modifiers.contains(KeyModifiers::CONTROL)
}

fn is_terminal_key(event: KeyEvent) -> bool {
    matches!(event.code, KeyCode::Char('/') | KeyCode::Char('_'))
        && event.modifiers.contains(KeyModifiers::CONTROL)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn map(input: &mut KeyInput, event: KeyEvent) -> DashboardCommand {
        input.map_event(event).expect("press event should map")
    }

    #[test]
    fn key_input_handles_basic_keys() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, key(KeyCode::Char('j'))),
            DashboardCommand::Down
        );
        assert_eq!(
            map(&mut input, ctrl_key(KeyCode::Char('c'))),
            DashboardCommand::Quit
        );
    }

    #[test]
    fn key_input_handles_horizontal_vim_motions() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, key(KeyCode::Char('h'))),
            DashboardCommand::Left
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('l'))),
            DashboardCommand::Right
        );
        assert_eq!(map(&mut input, key(KeyCode::Left)), DashboardCommand::Left);
        assert_eq!(
            map(&mut input, key(KeyCode::Right)),
            DashboardCommand::Right
        );
    }

    #[test]
    fn key_input_maps_brackets_to_view_switching() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, key(KeyCode::Char('['))),
            DashboardCommand::PreviousView
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char(']'))),
            DashboardCommand::NextView
        );
    }

    #[test]
    fn key_input_uses_top_digits_for_panel_focus() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, key(KeyCode::Char('1'))),
            DashboardCommand::FocusStatus
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('0'))),
            DashboardCommand::FocusMain
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('2'))),
            DashboardCommand::FocusRepos
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('3'))),
            DashboardCommand::FocusWorktrees
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Tab)),
            DashboardCommand::FocusNext
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('4'))),
            DashboardCommand::Other
        );
    }

    #[test]
    fn key_input_uses_leader_digits_for_repo_shortcuts() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, key(KeyCode::Char(' '))),
            DashboardCommand::Leader
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('1'))),
            DashboardCommand::RepoShortcut('1')
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char(' '))),
            DashboardCommand::Leader
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('9'))),
            DashboardCommand::RepoShortcut('9')
        );
    }

    #[test]
    fn key_input_quits_on_q() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, key(KeyCode::Char('q'))),
            DashboardCommand::Quit
        );
    }

    #[test]
    fn key_input_handles_open_tmux_session_keys() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, key(KeyCode::Char('i'))),
            DashboardCommand::Other
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char(' '))),
            DashboardCommand::Leader
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char(' '))),
            DashboardCommand::OpenTmuxSession
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Enter)),
            DashboardCommand::OpenTmuxSession
        );
    }

    #[test]
    fn key_input_handles_leader_lazygit() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, key(KeyCode::Char(' '))),
            DashboardCommand::Leader
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('g'))),
            DashboardCommand::LeaderGit
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('g'))),
            DashboardCommand::LazyGit
        );
    }

    #[test]
    fn key_input_maps_unified_workflow_and_configuration_surfaces() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('W'))),
            DashboardCommand::WorkflowLauncher
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char(' '))),
            DashboardCommand::Leader
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('w'))),
            DashboardCommand::LeaderWorkflow
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('a'))),
            DashboardCommand::WorkflowAi
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char(' '))),
            DashboardCommand::Leader
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('w'))),
            DashboardCommand::LeaderWorkflow
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('w'))),
            DashboardCommand::WorkflowLauncher
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char(' '))),
            DashboardCommand::Leader
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('c'))),
            DashboardCommand::Configuration
        );
    }

    #[test]
    fn key_input_handles_leader_open_pr() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, key(KeyCode::Char(' '))),
            DashboardCommand::Leader
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('g'))),
            DashboardCommand::LeaderGit
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('o'))),
            DashboardCommand::OpenPr
        );
    }

    #[test]
    fn plain_o_opens_development_url_without_changing_leader_pr_action() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, key(KeyCode::Char('o'))),
            DashboardCommand::OpenDevelopmentUrl
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char(' '))),
            DashboardCommand::Leader
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('g'))),
            DashboardCommand::LeaderGit
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('o'))),
            DashboardCommand::OpenPr
        );
    }

    #[test]
    fn key_input_handles_restored_git_actions() {
        let mut input = KeyInput::default();
        for (code, expected) in [
            ('P', DashboardCommand::Push),
            ('M', DashboardCommand::Merge),
            ('c', DashboardCommand::CiFix),
            ('f', DashboardCommand::ReviewFix),
        ] {
            assert_eq!(
                map(&mut input, key(KeyCode::Char(' '))),
                DashboardCommand::Leader
            );
            assert_eq!(
                map(&mut input, key(KeyCode::Char('g'))),
                DashboardCommand::LeaderGit
            );
            assert_eq!(map(&mut input, key(KeyCode::Char(code))), expected);
        }
    }

    #[test]
    fn key_input_handles_review_resolution() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, key(KeyCode::Char(' '))),
            DashboardCommand::Leader
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('g'))),
            DashboardCommand::LeaderGit
        );
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('R'))),
            DashboardCommand::ResolveAllComments
        );
    }

    #[test]
    fn key_input_handles_enter_open_tmux_session_and_help_keys() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, key(KeyCode::Enter)),
            DashboardCommand::OpenTmuxSession
        );
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('?'))),
            DashboardCommand::Help
        );
    }

    #[test]
    fn key_input_handles_terminal_key() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, ctrl_key(KeyCode::Char('/'))),
            DashboardCommand::Terminal
        );
        assert_eq!(
            map(&mut input, ctrl_key(KeyCode::Char('_'))),
            DashboardCommand::Terminal
        );
    }

    #[test]
    fn key_input_handles_leader_terminal_key() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, key(KeyCode::Char(' '))),
            DashboardCommand::Leader
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Enter)),
            DashboardCommand::Terminal
        );
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('?'))),
            DashboardCommand::Help
        );
    }

    #[test]
    fn key_input_handles_cleanup_keys() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('D'))),
            DashboardCommand::Delete
        );
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('U'))),
            DashboardCommand::Unarchive
        );
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('X'))),
            DashboardCommand::DeletePermanent
        );
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('C'))),
            DashboardCommand::OpenRemotePrs
        );
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('W'))),
            DashboardCommand::WorkflowLauncher
        );
        for removed in ['e', 'w', 'E', 'H', 'P'] {
            assert_eq!(
                map(&mut input, key(KeyCode::Char(removed))),
                DashboardCommand::Other
            );
        }
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('M'))),
            DashboardCommand::MigrateHarness
        );
    }

    #[test]
    fn key_input_uses_angle_brackets_for_visibility() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('+'))),
            DashboardCommand::Other
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('-'))),
            DashboardCommand::Other
        );
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('>'))),
            DashboardCommand::VisibilityUp
        );
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('<'))),
            DashboardCommand::VisibilityDown
        );
    }

    #[test]
    fn key_input_uses_lazygit_style_branch_actions() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, key(KeyCode::Char(' '))),
            DashboardCommand::Leader
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('g'))),
            DashboardCommand::LeaderGit
        );
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('P'))),
            DashboardCommand::Push
        );
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('M'))),
            DashboardCommand::MigrateHarness
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('n'))),
            DashboardCommand::Other
        );
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('R'))),
            DashboardCommand::Other
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('x'))),
            DashboardCommand::AbortOpencode
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('m'))),
            DashboardCommand::Other
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('u'))),
            DashboardCommand::WorkflowPauseResume
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('a'))),
            DashboardCommand::Other
        );
    }

    #[test]
    fn key_input_ignores_non_press_events() {
        let mut input = KeyInput::default();
        let event = KeyEvent::new_with_kind(
            KeyCode::Char('j'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(input.map_event(event), None);
    }

    #[test]
    fn key_input_cancels_incomplete_leaders_on_unknown_keys() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, key(KeyCode::Char(' '))),
            DashboardCommand::Leader
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('z'))),
            DashboardCommand::Other
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('g'))),
            DashboardCommand::G
        );

        assert_eq!(
            map(&mut input, key(KeyCode::Char(' '))),
            DashboardCommand::Leader
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('g'))),
            DashboardCommand::LeaderGit
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('z'))),
            DashboardCommand::Other
        );
        assert_eq!(
            map(&mut input, key(KeyCode::Char('g'))),
            DashboardCommand::G
        );
    }
}
