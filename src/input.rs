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
                    b'\r' | b'\n' => keys.push(Key::OpenTmuxSession),
                    31 => keys.push(Key::Terminal),
                    b'k' => keys.push(Key::Up),
                    b'j' => keys.push(Key::Down),
                    b'h' => keys.push(Key::Left),
                    b'l' => keys.push(Key::Right),
                    b'G' => keys.push(Key::Bottom),
                    b'g' => keys.push(Key::G),
                    b'r' => keys.push(Key::Refresh),
                    b'1'..=b'9' => keys.push(Key::RepoShortcut(*byte as char)),
                    b'p' => keys.push(Key::PullDefault),
                    b'c' => keys.push(Key::Create),
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
                    _ => {
                        self.state = KeyInputState::Normal;
                        keys.push(Key::Other);
                    }
                },
                KeyInputState::LeaderG => {
                    self.state = KeyInputState::Normal;
                    match byte {
                        b'g' => keys.push(Key::LazyGit),
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
    Bottom,
    G,
    Leader,
    LeaderGit,
    OpenTmuxSession,
    LazyGit,
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
    Create,
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
        let keys = input.feed(b"D");
        assert!(matches!(keys.as_slice(), [Key::Delete]));
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
                Key::Other,
                Key::Other,
                Key::Other,
                Key::Other,
            ]
        ));
    }
}
