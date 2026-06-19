#[derive(Default)]
pub struct KeyInput {
    state: KeyInputState,
}

#[derive(Default)]
enum KeyInputState {
    #[default]
    Normal,
    Escape,
    Csi,
    Leader,
    LeaderG,
}

impl KeyInput {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        let mut keys = Vec::new();
        for byte in bytes {
            match self.state {
                KeyInputState::Normal => match byte {
                    b'\x1b' => self.state = KeyInputState::Escape,
                    b'\x03' => keys.push(Key::Quit),
                    b'q' => keys.push(Key::Quit),
                    b' ' => {
                        self.state = KeyInputState::Leader;
                        keys.push(Key::Leader);
                    }
                    b'\t' => keys.push(Key::FocusNext),
                    b'\r' | b'\n' => keys.push(Key::OpenTmuxSession),
                    31 => keys.push(Key::Terminal),
                    b'k' => keys.push(Key::Up),
                    b'j' => keys.push(Key::Down),
                    b'h' => keys.push(Key::Left),
                    b'l' => keys.push(Key::Right),
                    b'G' => keys.push(Key::Bottom),
                    b'g' => keys.push(Key::G),
                    b'r' => keys.push(Key::Refresh),
                    b'1' => keys.push(Key::FocusStatus),
                    b'2' => keys.push(Key::FocusRepos),
                    b'3' => keys.push(Key::FocusWorktrees),
                    b'4'..=b'9' => keys.push(Key::Other),
                    b'p' => keys.push(Key::PullDefault),
                    b'P' => keys.push(Key::PlanMode),
                    b'c' => keys.push(Key::Create),
                    b'x' => keys.push(Key::AbortOpencode),
                    b'A' => keys.push(Key::AddRepo),
                    b'R' => keys.push(Key::ManageRepos),
                    b'e' => keys.push(Key::EditConfig),
                    b'D' => keys.push(Key::Delete),
                    b'?' => keys.push(Key::Help),
                    b'/' => keys.push(Key::Search),
                    _ => keys.push(Key::Other),
                },
                KeyInputState::Escape => {
                    if *byte == b'[' {
                        self.state = KeyInputState::Csi;
                    } else {
                        self.state = KeyInputState::Normal;
                        keys.push(Key::Other);
                    }
                }
                KeyInputState::Csi => {
                    self.state = KeyInputState::Normal;
                    match byte {
                        b'A' => keys.push(Key::Up),
                        b'B' => keys.push(Key::Down),
                        b'C' => keys.push(Key::Right),
                        b'D' => keys.push(Key::Left),
                        _ => keys.push(Key::Other),
                    }
                }
                KeyInputState::Leader => match byte {
                    b' ' => {
                        self.state = KeyInputState::Normal;
                        keys.push(Key::OpenTmuxSession);
                    }
                    b'\r' | b'\n' => {
                        self.state = KeyInputState::Normal;
                        keys.push(Key::Terminal);
                    }
                    b'g' => {
                        self.state = KeyInputState::LeaderG;
                        keys.push(Key::LeaderGit);
                    }
                    b'1'..=b'9' => {
                        self.state = KeyInputState::Normal;
                        keys.push(Key::RepoShortcut(*byte as char));
                    }
                    _ => {
                        self.state = KeyInputState::Normal;
                        keys.push(Key::Other);
                    }
                },
                KeyInputState::LeaderG => {
                    self.state = KeyInputState::Normal;
                    match byte {
                        b'g' => keys.push(Key::LazyGit),
                        b'o' => keys.push(Key::OpenPr),
                        b'P' => keys.push(Key::Push),
                        b'M' => keys.push(Key::Merge),
                        b'f' => keys.push(Key::ReviewFix),
                        b'p' => keys.push(Key::PullDefault),
                        _ => keys.push(Key::Other),
                    }
                }
            }
        }
        keys
    }
}

pub enum Key {
    Up,
    Down,
    Left,
    Right,
    FocusNext,
    FocusStatus,
    FocusRepos,
    FocusWorktrees,
    Bottom,
    G,
    Leader,
    LeaderGit,
    OpenTmuxSession,
    LazyGit,
    OpenPr,
    Terminal,
    Help,
    Refresh,
    RepoShortcut(char),
    AddRepo,
    ManageRepos,
    ReviewFix,
    Push,
    Merge,
    PullDefault,
    PlanMode,
    Create,
    AbortOpencode,
    Delete,
    EditConfig,
    Search,
    Quit,
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_input_handles_batched_keys() {
        let mut input = KeyInput::default();
        let keys = input.feed(b"j\x03");
        assert!(matches!(keys.as_slice(), [Key::Down, Key::Quit]));
    }

    #[test]
    fn key_input_handles_horizontal_vim_motions() {
        let mut input = KeyInput::default();
        let keys = input.feed(b"hl");
        assert!(matches!(keys.as_slice(), [Key::Left, Key::Right]));
    }

    #[test]
    fn key_input_uses_top_digits_for_panel_focus() {
        let mut input = KeyInput::default();
        let keys = input.feed(b"123\t4");
        assert!(matches!(
            keys.as_slice(),
            [
                Key::FocusStatus,
                Key::FocusRepos,
                Key::FocusWorktrees,
                Key::FocusNext,
                Key::Other
            ]
        ));
    }

    #[test]
    fn key_input_uses_leader_digits_for_repo_shortcuts() {
        let mut input = KeyInput::default();
        let keys = input.feed(b" 1 9");
        assert!(matches!(
            keys.as_slice(),
            [
                Key::Leader,
                Key::RepoShortcut('1'),
                Key::Leader,
                Key::RepoShortcut('9')
            ]
        ));
    }

    #[test]
    fn key_input_quits_on_q() {
        let mut input = KeyInput::default();
        let keys = input.feed(b"q");
        assert!(matches!(keys.as_slice(), [Key::Quit]));
    }

    #[test]
    fn key_input_handles_open_tmux_session_keys() {
        let mut input = KeyInput::default();
        let keys = input.feed(b"i  ");
        assert!(matches!(
            keys.as_slice(),
            [Key::Other, Key::Leader, Key::OpenTmuxSession]
        ));
    }

    #[test]
    fn key_input_handles_leader_lazygit() {
        let mut input = KeyInput::default();
        let keys = input.feed(b" gg");
        assert!(matches!(
            keys.as_slice(),
            [Key::Leader, Key::LeaderGit, Key::LazyGit]
        ));
    }

    #[test]
    fn key_input_handles_leader_open_pr() {
        let mut input = KeyInput::default();
        let keys = input.feed(b" go");
        assert!(matches!(
            keys.as_slice(),
            [Key::Leader, Key::LeaderGit, Key::OpenPr]
        ));
    }

    #[test]
    fn key_input_handles_enter_open_tmux_session_and_help_keys() {
        let mut input = KeyInput::default();
        let keys = input.feed(b"\n?");
        assert!(matches!(keys.as_slice(), [Key::OpenTmuxSession, Key::Help]));
    }

    #[test]
    fn key_input_handles_terminal_key() {
        let mut input = KeyInput::default();
        let keys = input.feed(&[31]);
        assert!(matches!(keys.as_slice(), [Key::Terminal]));
    }

    #[test]
    fn key_input_handles_leader_terminal_key() {
        let mut input = KeyInput::default();
        let keys = input.feed(b" \n?");
        assert!(matches!(
            keys.as_slice(),
            [Key::Leader, Key::Terminal, Key::Help]
        ));
    }

    #[test]
    fn key_input_handles_cleanup_keys() {
        let mut input = KeyInput::default();
        let keys = input.feed(b"DP");
        assert!(matches!(keys.as_slice(), [Key::Delete, Key::PlanMode]));
    }

    #[test]
    fn key_input_uses_lazygit_style_branch_actions() {
        let mut input = KeyInput::default();
        let keys = input.feed(b" gPMnRxmua");
        assert!(matches!(
            keys.as_slice(),
            [
                Key::Leader,
                Key::LeaderGit,
                Key::Push,
                Key::Other,
                Key::Other,
                Key::ManageRepos,
                Key::AbortOpencode,
                Key::Other,
                Key::Other,
                Key::Other,
            ]
        ));
    }
}
