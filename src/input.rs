use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

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
}

impl KeyInput {
    pub fn map_event(&mut self, event: KeyEvent) -> Option<Key> {
        if event.kind != KeyEventKind::Press {
            return None;
        }

        Some(match self.state {
            KeyInputState::Normal => self.map_normal(event),
            KeyInputState::Leader => self.map_leader(event),
            KeyInputState::LeaderG => self.map_leader_git(event),
        })
    }

    fn map_normal(&mut self, event: KeyEvent) -> Key {
        if is_ctrl_char(event, 'c') {
            return Key::Quit;
        }
        if is_terminal_key(event) {
            return Key::Terminal;
        }

        match event.code {
            KeyCode::Char('q') if plain_char(event) => Key::Quit,
            KeyCode::Char(' ') if plain_char(event) => {
                self.state = KeyInputState::Leader;
                Key::Leader
            }
            KeyCode::Tab => Key::FocusNext,
            KeyCode::Enter => Key::OpenTmuxSession,
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Left => Key::Left,
            KeyCode::Right => Key::Right,
            KeyCode::Char('k') if plain_char(event) => Key::Up,
            KeyCode::Char('j') if plain_char(event) => Key::Down,
            KeyCode::Char('h') if plain_char(event) => Key::Left,
            KeyCode::Char('l') if plain_char(event) => Key::Right,
            KeyCode::Char('G') if plain_char(event) => Key::Bottom,
            KeyCode::Char('g') if plain_char(event) => Key::G,
            KeyCode::Char('{') if plain_char(event) => Key::PreviousBlock,
            KeyCode::Char('}') if plain_char(event) => Key::NextBlock,
            KeyCode::Char('r') if plain_char(event) => Key::Refresh,
            KeyCode::Char('0') if plain_char(event) => Key::FocusMain,
            KeyCode::Char('1') if plain_char(event) => Key::FocusStatus,
            KeyCode::Char('2') if plain_char(event) => Key::FocusRepos,
            KeyCode::Char('3') if plain_char(event) => Key::FocusWorktrees,
            KeyCode::Char('4'..='9') if plain_char(event) => Key::Other,
            KeyCode::Char('p') if plain_char(event) => Key::PullDefault,
            KeyCode::Char('P') if plain_char(event) => Key::PlanMode,
            KeyCode::Char('c') if plain_char(event) => Key::Create,
            KeyCode::Char('x') if plain_char(event) => Key::AbortOpencode,
            KeyCode::Char('X') if plain_char(event) => Key::DeletePermanent,
            KeyCode::Char('A') if plain_char(event) => Key::AutoFlow,
            KeyCode::Char('C') if plain_char(event) => Key::EditWorktreeColumns,
            KeyCode::Char('R') if plain_char(event) => Key::ManageRepos,
            KeyCode::Char('e') if plain_char(event) => Key::EditConfig,
            KeyCode::Char('E') if plain_char(event) => Key::EditUserConfig,
            KeyCode::Char('D') if plain_char(event) => Key::Delete,
            KeyCode::Char('?') if plain_char(event) => Key::Help,
            KeyCode::Char('/') if plain_char(event) => Key::Search,
            _ => Key::Other,
        }
    }

    fn map_leader(&mut self, event: KeyEvent) -> Key {
        match event.code {
            KeyCode::Char(' ') if plain_char(event) => {
                self.state = KeyInputState::Normal;
                Key::OpenTmuxSession
            }
            KeyCode::Char('p') if plain_char(event) => {
                self.state = KeyInputState::Normal;
                Key::PlanActions
            }
            KeyCode::Enter => {
                self.state = KeyInputState::Normal;
                Key::Terminal
            }
            KeyCode::Char('g') if plain_char(event) => {
                self.state = KeyInputState::LeaderG;
                Key::LeaderGit
            }
            KeyCode::Char(key @ '1'..='9') if plain_char(event) => {
                self.state = KeyInputState::Normal;
                Key::RepoShortcut(key)
            }
            _ => {
                self.state = KeyInputState::Normal;
                Key::Other
            }
        }
    }

    fn map_leader_git(&mut self, event: KeyEvent) -> Key {
        self.state = KeyInputState::Normal;
        match event.code {
            KeyCode::Char('g') if plain_char(event) => Key::LazyGit,
            KeyCode::Char('a') if plain_char(event) => Key::Other,
            KeyCode::Char('o') if plain_char(event) => Key::OpenPr,
            KeyCode::Char('P') if plain_char(event) => Key::Push,
            KeyCode::Char('M') if plain_char(event) => Key::Merge,
            KeyCode::Char('c') if plain_char(event) => Key::CiFix,
            KeyCode::Char('f') if plain_char(event) => Key::ReviewFix,
            KeyCode::Char('p') if plain_char(event) => Key::PullDefault,
            _ => Key::Other,
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

#[derive(Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    FocusNext,
    FocusMain,
    FocusStatus,
    FocusRepos,
    FocusWorktrees,
    Bottom,
    G,
    PreviousBlock,
    NextBlock,
    Leader,
    LeaderGit,
    OpenTmuxSession,
    PlanActions,
    LazyGit,
    AutoFlow,
    OpenPr,
    Terminal,
    Help,
    Refresh,
    RepoShortcut(char),
    ManageRepos,
    EditWorktreeColumns,
    CiFix,
    ReviewFix,
    Push,
    Merge,
    PullDefault,
    PlanMode,
    Create,
    AbortOpencode,
    Delete,
    DeletePermanent,
    EditConfig,
    EditUserConfig,
    Search,
    Quit,
    Other,
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

    fn map(input: &mut KeyInput, event: KeyEvent) -> Key {
        input.map_event(event).expect("press event should map")
    }

    #[test]
    fn key_input_handles_basic_keys() {
        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, key(KeyCode::Char('j'))), Key::Down);
        assert_eq!(map(&mut input, ctrl_key(KeyCode::Char('c'))), Key::Quit);
    }

    #[test]
    fn key_input_handles_horizontal_vim_motions() {
        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, key(KeyCode::Char('h'))), Key::Left);
        assert_eq!(map(&mut input, key(KeyCode::Char('l'))), Key::Right);
        assert_eq!(map(&mut input, key(KeyCode::Left)), Key::Left);
        assert_eq!(map(&mut input, key(KeyCode::Right)), Key::Right);
    }

    #[test]
    fn key_input_uses_top_digits_for_panel_focus() {
        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, key(KeyCode::Char('1'))), Key::FocusStatus);
        assert_eq!(map(&mut input, key(KeyCode::Char('0'))), Key::FocusMain);
        assert_eq!(map(&mut input, key(KeyCode::Char('2'))), Key::FocusRepos);
        assert_eq!(
            map(&mut input, key(KeyCode::Char('3'))),
            Key::FocusWorktrees
        );
        assert_eq!(map(&mut input, key(KeyCode::Tab)), Key::FocusNext);
        assert_eq!(map(&mut input, key(KeyCode::Char('4'))), Key::Other);
    }

    #[test]
    fn key_input_uses_leader_digits_for_repo_shortcuts() {
        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, key(KeyCode::Char(' '))), Key::Leader);
        assert_eq!(
            map(&mut input, key(KeyCode::Char('1'))),
            Key::RepoShortcut('1')
        );
        assert_eq!(map(&mut input, key(KeyCode::Char(' '))), Key::Leader);
        assert_eq!(
            map(&mut input, key(KeyCode::Char('9'))),
            Key::RepoShortcut('9')
        );
    }

    #[test]
    fn key_input_quits_on_q() {
        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, key(KeyCode::Char('q'))), Key::Quit);
    }

    #[test]
    fn key_input_handles_open_tmux_session_keys() {
        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, key(KeyCode::Char('i'))), Key::Other);
        assert_eq!(map(&mut input, key(KeyCode::Char(' '))), Key::Leader);
        assert_eq!(
            map(&mut input, key(KeyCode::Char(' '))),
            Key::OpenTmuxSession
        );
        assert_eq!(map(&mut input, key(KeyCode::Enter)), Key::OpenTmuxSession);
    }

    #[test]
    fn key_input_handles_leader_lazygit() {
        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, key(KeyCode::Char(' '))), Key::Leader);
        assert_eq!(map(&mut input, key(KeyCode::Char('g'))), Key::LeaderGit);
        assert_eq!(map(&mut input, key(KeyCode::Char('g'))), Key::LazyGit);
    }

    #[test]
    fn key_input_handles_auto_flow() {
        let mut input = KeyInput::default();
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('A'))),
            Key::AutoFlow
        );
    }

    #[test]
    fn key_input_handles_leader_plan_actions() {
        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, key(KeyCode::Char(' '))), Key::Leader);
        assert_eq!(map(&mut input, key(KeyCode::Char('p'))), Key::PlanActions);

        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, key(KeyCode::Char(' '))), Key::Leader);
        assert_eq!(map(&mut input, shift_key(KeyCode::Char('P'))), Key::Other);
    }

    #[test]
    fn key_input_does_not_keep_leader_auto_flow_alias() {
        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, key(KeyCode::Char(' '))), Key::Leader);
        assert_eq!(map(&mut input, key(KeyCode::Char('g'))), Key::LeaderGit);
        assert_eq!(map(&mut input, key(KeyCode::Char('a'))), Key::Other);
    }

    #[test]
    fn key_input_handles_leader_open_pr() {
        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, key(KeyCode::Char(' '))), Key::Leader);
        assert_eq!(map(&mut input, key(KeyCode::Char('g'))), Key::LeaderGit);
        assert_eq!(map(&mut input, key(KeyCode::Char('o'))), Key::OpenPr);
    }

    #[test]
    fn key_input_handles_leader_ci_fix() {
        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, key(KeyCode::Char(' '))), Key::Leader);
        assert_eq!(map(&mut input, key(KeyCode::Char('g'))), Key::LeaderGit);
        assert_eq!(map(&mut input, key(KeyCode::Char('c'))), Key::CiFix);
    }

    #[test]
    fn key_input_handles_enter_open_tmux_session_and_help_keys() {
        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, key(KeyCode::Enter)), Key::OpenTmuxSession);
        assert_eq!(map(&mut input, shift_key(KeyCode::Char('?'))), Key::Help);
    }

    #[test]
    fn key_input_handles_terminal_key() {
        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, ctrl_key(KeyCode::Char('/'))), Key::Terminal);
        assert_eq!(map(&mut input, ctrl_key(KeyCode::Char('_'))), Key::Terminal);
    }

    #[test]
    fn key_input_handles_leader_terminal_key() {
        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, key(KeyCode::Char(' '))), Key::Leader);
        assert_eq!(map(&mut input, key(KeyCode::Enter)), Key::Terminal);
        assert_eq!(map(&mut input, shift_key(KeyCode::Char('?'))), Key::Help);
    }

    #[test]
    fn key_input_handles_cleanup_keys() {
        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, shift_key(KeyCode::Char('D'))), Key::Delete);
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('X'))),
            Key::DeletePermanent
        );
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('C'))),
            Key::EditWorktreeColumns
        );
        assert_eq!(map(&mut input, key(KeyCode::Char('e'))), Key::EditConfig);
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('E'))),
            Key::EditUserConfig
        );
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('P'))),
            Key::PlanMode
        );
    }

    #[test]
    fn key_input_uses_lazygit_style_branch_actions() {
        let mut input = KeyInput::default();
        assert_eq!(map(&mut input, key(KeyCode::Char(' '))), Key::Leader);
        assert_eq!(map(&mut input, key(KeyCode::Char('g'))), Key::LeaderGit);
        assert_eq!(map(&mut input, shift_key(KeyCode::Char('P'))), Key::Push);
        assert_eq!(map(&mut input, shift_key(KeyCode::Char('M'))), Key::Other);
        assert_eq!(map(&mut input, key(KeyCode::Char('n'))), Key::Other);
        assert_eq!(
            map(&mut input, shift_key(KeyCode::Char('R'))),
            Key::ManageRepos
        );
        assert_eq!(map(&mut input, key(KeyCode::Char('x'))), Key::AbortOpencode);
        assert_eq!(map(&mut input, key(KeyCode::Char('m'))), Key::Other);
        assert_eq!(map(&mut input, key(KeyCode::Char('u'))), Key::Other);
        assert_eq!(map(&mut input, key(KeyCode::Char('a'))), Key::Other);
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
        assert_eq!(map(&mut input, key(KeyCode::Char(' '))), Key::Leader);
        assert_eq!(map(&mut input, key(KeyCode::Char('z'))), Key::Other);
        assert_eq!(map(&mut input, key(KeyCode::Char('g'))), Key::G);

        assert_eq!(map(&mut input, key(KeyCode::Char(' '))), Key::Leader);
        assert_eq!(map(&mut input, key(KeyCode::Char('g'))), Key::LeaderGit);
        assert_eq!(map(&mut input, key(KeyCode::Char('z'))), Key::Other);
        assert_eq!(map(&mut input, key(KeyCode::Char('g'))), Key::G);
    }
}
