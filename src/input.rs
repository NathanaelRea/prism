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
                    b' ' => self.state = KeyInputState::Leader,
                    b'\r' | b'\n' => keys.push(Key::AgentMode),
                    31 => keys.push(Key::Terminal),
                    b'k' => keys.push(Key::Up),
                    b'j' => keys.push(Key::Down),
                    b'G' => keys.push(Key::Bottom),
                    b'g' => keys.push(Key::G),
                    b'r' => keys.push(Key::Refresh),
                    b'f' => keys.push(Key::ReviewFix),
                    b'P' => keys.push(Key::Push),
                    b'M' => keys.push(Key::Merge),
                    b'c' => keys.push(Key::Create),
                    b'D' => keys.push(Key::Delete),
                    b'?' => keys.push(Key::Help),
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
                        _ => keys.push(Key::Other),
                    }
                }
                KeyInputState::Leader => match byte {
                    b'g' => self.state = KeyInputState::LeaderG,
                    _ => {
                        self.state = KeyInputState::Normal;
                        keys.push(Key::Other);
                    }
                },
                KeyInputState::LeaderG => {
                    self.state = KeyInputState::Normal;
                    match byte {
                        b'g' => keys.push(Key::LazyGit),
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
    Bottom,
    G,
    AgentMode,
    LazyGit,
    Terminal,
    Help,
    Refresh,
    ReviewFix,
    Push,
    Merge,
    Create,
    Delete,
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
    fn key_input_quits_on_q() {
        let mut input = KeyInput::default();
        let keys = input.feed(b"q");
        assert!(matches!(keys.as_slice(), [Key::Quit]));
    }

    #[test]
    fn key_input_handles_agent_mode_keys() {
        let mut input = KeyInput::default();
        let keys = input.feed(b"i\n");
        assert!(matches!(keys.as_slice(), [Key::Other, Key::AgentMode]));
    }

    #[test]
    fn key_input_handles_leader_lazygit() {
        let mut input = KeyInput::default();
        let keys = input.feed(b" gg");
        assert!(matches!(keys.as_slice(), [Key::LazyGit]));
    }

    #[test]
    fn key_input_handles_terminal_and_help_keys() {
        let mut input = KeyInput::default();
        let keys = input.feed(&[31, b'?']);
        assert!(matches!(keys.as_slice(), [Key::Terminal, Key::Help]));
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
        let keys = input.feed(b"PMnRxmua");
        assert!(matches!(
            keys.as_slice(),
            [
                Key::Push,
                Key::Merge,
                Key::Other,
                Key::Other,
                Key::Other,
                Key::Other,
                Key::Other,
                Key::Other,
            ]
        ));
    }
}
