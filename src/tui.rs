use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, ErrorKind, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

use crate::agent::AgentState;
use crate::config::Config;
use crate::github::{PrCache, PrSummary};
use crate::input::{Key, KeyInput};
use crate::repo::Repository;
use crate::session::{Session, append_runtime_log};
use crate::terminal::{RawTerminal, stdin_is_tty, terminal_size};
use crate::tmux::TmuxWindow;
use crate::util::{strip_ansi, truncate_line, yes};
use crate::view;

pub struct Tui {
    pub(crate) repo: Repository,
    pub(crate) config: Config,
    pub(crate) sessions: Vec<Session>,
    pub(crate) selected: usize,
    pub(crate) allow_dirty: bool,
    pub(crate) pr_poll_tx: Sender<PrPollResult>,
    pub(crate) pr_poll_rx: Receiver<PrPollResult>,
    pub(crate) pr_polls_in_flight: BTreeSet<PrPollKey>,
    pub(crate) pr_summary_poll_in_flight: bool,
    pub(crate) pr_summary_last_polled: Option<std::time::Instant>,
    pub(crate) tmux_warmup_tx: Sender<TmuxWarmupResult>,
    pub(crate) tmux_warmup_rx: Receiver<TmuxWarmupResult>,
    pub(crate) tmux_warmups_in_flight: BTreeSet<TmuxWarmupKey>,
    pub(crate) tmux_generations: BTreeMap<TmuxSlotKey, u64>,
    status_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PrPollKey {
    pub branch: String,
    pub path: PathBuf,
}

pub(crate) enum PrPollResult {
    Summary {
        summaries: Result<Vec<PrSummary>, String>,
    },
    Details {
        key: PrPollKey,
        cache: Box<PrCache>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TmuxSlotKey {
    pub branch: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TmuxWarmupKey {
    pub slot: TmuxSlotKey,
    pub generation: u64,
}

pub(crate) struct TmuxWarmupResult {
    pub key: TmuxWarmupKey,
    pub running: Option<bool>,
    pub error: Option<String>,
}

impl Tui {
    pub fn new(
        repo: Repository,
        config: Config,
        sessions: Vec<Session>,
        allow_dirty: bool,
    ) -> Self {
        let (pr_poll_tx, pr_poll_rx) = mpsc::channel();
        let (tmux_warmup_tx, tmux_warmup_rx) = mpsc::channel();
        Self {
            repo,
            config,
            sessions,
            selected: 0,
            allow_dirty,
            pr_poll_tx,
            pr_poll_rx,
            pr_polls_in_flight: BTreeSet::new(),
            pr_summary_poll_in_flight: false,
            pr_summary_last_polled: None,
            tmux_warmup_tx,
            tmux_warmup_rx,
            tmux_warmups_in_flight: BTreeSet::new(),
            tmux_generations: BTreeMap::new(),
            status_message: None,
        }
    }

    pub fn run(&mut self) -> Result<(), String> {
        if !stdin_is_tty() {
            return Err("TUI requires an interactive terminal".to_string());
        }

        let mut raw = RawTerminal::enter()?;
        self.start_tmux_agent_warmup();
        self.draw()?;
        let mut stdin = io::stdin();
        let mut buffer = [0_u8; 64];
        let mut key_input = KeyInput::default();
        let mut pending_g = false;
        let mut last_size = terminal_size();

        loop {
            let agents_changed = self.poll_agents();
            let tmux_changed = self.poll_tmux_agent_warmup();
            let prs_changed = self.poll_pull_requests(false);
            let current_size = terminal_size();
            let resized = current_size != last_size;
            if resized {
                last_size = current_size;
            }
            if agents_changed || tmux_changed || prs_changed || resized {
                self.draw()?;
            }
            let count = match stdin.read(&mut buffer) {
                Ok(count) => count,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    continue;
                }
                Err(error) => return Err(error.to_string()),
            };
            if count == 0 {
                continue;
            }

            let mut should_quit = false;
            for key in key_input.feed(&buffer[..count]) {
                match key {
                    Key::Quit => {
                        pending_g = false;
                        should_quit = self.confirm_quit()?;
                    }
                    Key::Down => {
                        pending_g = false;
                        self.move_down();
                    }
                    Key::Up => {
                        pending_g = false;
                        self.move_up();
                    }
                    Key::Bottom => {
                        pending_g = false;
                        self.selected = self.sessions.len().saturating_sub(1);
                    }
                    Key::G => {
                        if pending_g {
                            self.selected = 0;
                            pending_g = false;
                        } else {
                            pending_g = true;
                        }
                    }
                    Key::AgentMode => {
                        pending_g = false;
                        self.enter_agent_mode(&mut raw)?;
                    }
                    Key::LazyGit => {
                        pending_g = false;
                        if let Err(error) = self.open_tmux_window(&mut raw, TmuxWindow::LazyGit) {
                            self.show_error("lazygit failed", &error)?;
                        }
                    }
                    Key::Terminal => {
                        pending_g = false;
                        if let Err(error) = self.open_tmux_window(&mut raw, TmuxWindow::Terminal) {
                            self.show_error("terminal failed", &error)?;
                        }
                    }
                    Key::Help => {
                        pending_g = false;
                        self.show_keybindings_dialog()?;
                    }
                    Key::Refresh => {
                        pending_g = false;
                        self.refresh_sessions()?;
                        self.start_tmux_agent_warmup();
                        self.poll_pull_requests(true);
                    }
                    Key::ReviewFix => {
                        pending_g = false;
                        if let Err(error) = self.start_review_fix() {
                            self.show_error("review fix failed", &error)?;
                        }
                    }
                    Key::Push => {
                        pending_g = false;
                        if let Err(error) = self.push_selected_branch() {
                            self.show_error("push failed", &error)?;
                        }
                    }
                    Key::Merge => {
                        pending_g = false;
                        if let Err(error) = self.merge_selected_pr() {
                            self.show_error("merge failed", &error)?;
                        }
                    }
                    Key::Create => {
                        pending_g = false;
                        if let Err(error) = self.create_session() {
                            self.show_error("create session failed", &error)?;
                        }
                    }
                    Key::Delete => {
                        pending_g = false;
                        if let Err(error) = self.delete_session() {
                            self.show_error("delete failed", &error)?;
                        }
                    }
                    Key::Other => pending_g = false,
                }
                if should_quit {
                    break;
                }
            }
            if should_quit {
                break;
            }
            self.draw()?;
        }
        Ok(())
    }

    fn confirm_quit(&self) -> Result<bool, String> {
        if !self
            .sessions
            .iter()
            .any(|session| session.agent_state == AgentState::Running)
        {
            return Ok(true);
        }
        let answer = self.prompt_line("Agents are running. Quit Prism? [y/N] ")?;
        Ok(yes(&answer))
    }

    fn enter_agent_mode(&mut self, raw: &mut RawTerminal) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        raw.suspend()?;
        let result = self.attach_selected_agent_terminal();
        let resume_result = raw.resume();
        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
        resume_result?;
        if let Err(error) = result {
            self.show_error("agent terminal failed", &error)?;
        }
        Ok(())
    }

    fn open_tmux_window(
        &mut self,
        raw: &mut RawTerminal,
        window: TmuxWindow,
    ) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        raw.suspend()?;
        let result = self.attach_selected_tmux_window(window);
        let resume_result = raw.resume();
        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
        resume_result?;
        result
    }

    fn show_keybindings_dialog(&self) -> Result<(), String> {
        let lines = [
            "Keybindings",
            "",
            "Enter        open selected agent",
            "Space g g    open tmux window 2: lazygit",
            "Ctrl-/       open tmux window 3: terminal",
            "c            create worktree session",
            "P            push selected branch",
            "M            merge selected PR",
            "f            stage review-fix prompt",
            "D            delete worktree/session",
            "j/k, arrows   move selection",
            "g g / G      top / bottom",
            "r            refresh",
            "q, Ctrl-C    quit",
            "",
            "Press any key to close",
        ];
        let (cols, rows) = terminal_size();
        let available_width = (cols as usize).saturating_sub(2).max(4);
        let width = lines
            .iter()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0)
            .saturating_add(4)
            .max(24)
            .min(available_width);
        let height = lines.len() + 2;
        let left = ((cols as usize).saturating_sub(width) / 2).saturating_add(1);
        let top = ((rows as usize).saturating_sub(height) / 2).saturating_add(1);

        print!("\x1b[?25l");
        print!(
            "\x1b[{top};{left}H+{}+",
            "-".repeat(width.saturating_sub(2))
        );
        for (index, line) in lines.iter().enumerate() {
            let y = top + index + 1;
            let text_width = width.saturating_sub(4);
            let text = truncate_line(line, text_width);
            print!(
                "\x1b[{y};{left}H| {:<text_width$} |",
                text,
                text_width = text_width
            );
        }
        print!(
            "\x1b[{};{}H+{}+",
            top + height - 1,
            left,
            "-".repeat(width.saturating_sub(2))
        );
        io::stdout().flush().map_err(|error| error.to_string())?;

        let mut stdin = io::stdin();
        let mut byte = [0_u8; 1];
        loop {
            match stdin.read(&mut byte) {
                Ok(1) => return Ok(()),
                Ok(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(error) => return Err(error.to_string()),
            }
        }
    }

    pub(crate) fn confirm_delete_dialog(
        &self,
        branch: &str,
        path: &str,
        warnings: &[String],
    ) -> Result<bool, String> {
        let mut lines = vec![
            "Delete Session".to_string(),
            String::new(),
            format!("branch: {branch}"),
            format!("path: {path}"),
            String::new(),
        ];
        if warnings.is_empty() {
            lines.push("No warnings detected.".to_string());
        } else {
            lines.push("Warnings".to_string());
            for warning in warnings {
                lines.push(format!("\x1b[31m•\x1b[0m {warning}"));
            }
        }
        lines.extend([
            String::new(),
            "Enter confirms delete. Esc/q cancels.".to_string(),
        ]);

        let (cols, rows) = terminal_size();
        let available_width = (cols as usize).saturating_sub(2).max(4);
        let width = lines
            .iter()
            .map(|line| strip_ansi(line).chars().count())
            .max()
            .unwrap_or(0)
            .saturating_add(4)
            .max(42)
            .min(available_width);
        let height = lines.len() + 2;
        let left = ((cols as usize).saturating_sub(width) / 2).saturating_add(1);
        let top = ((rows as usize).saturating_sub(height) / 2).saturating_add(1);

        print!("\x1b[?25l");
        print!(
            "\x1b[{top};{left}H+{}+",
            "-".repeat(width.saturating_sub(2))
        );
        for (index, line) in lines.iter().enumerate() {
            let y = top + index + 1;
            let text_width = width.saturating_sub(4);
            let text = truncate_line(&strip_ansi(line), text_width);
            let text = if line.contains("\x1b[") {
                truncate_ansi_dialog_line(line, text_width)
            } else {
                text
            };
            print!(
                "\x1b[{y};{left}H| {}{} |",
                text,
                " ".repeat(text_width.saturating_sub(strip_ansi(&text).chars().count()))
            );
        }
        print!(
            "\x1b[{};{}H+{}+",
            top + height - 1,
            left,
            "-".repeat(width.saturating_sub(2))
        );
        io::stdout().flush().map_err(|error| error.to_string())?;

        let mut stdin = io::stdin();
        let mut byte = [0_u8; 1];
        loop {
            match stdin.read(&mut byte) {
                Ok(1) => match byte[0] {
                    b'\r' | b'\n' => return Ok(true),
                    3 | 27 | b'q' => return Ok(false),
                    _ => {}
                },
                Ok(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(error) => return Err(error.to_string()),
            }
        }
    }

    pub(crate) fn prompt_line(&self, prompt: &str) -> Result<String, String> {
        self.prompt_line_with_initial(prompt, "")
    }

    fn prompt_line_with_initial(&self, prompt: &str, initial: &str) -> Result<String, String> {
        print!("\x1b[{};1H\x1b[2K\x1b[?25h{}", terminal_size().1, prompt);
        let mut input = initial.to_string();
        print!("{input}");
        io::stdout().flush().map_err(|error| error.to_string())?;
        let mut stdin = io::stdin();
        let mut byte = [0_u8; 1];
        loop {
            match stdin.read(&mut byte) {
                Ok(1) => {}
                Ok(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    continue;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    continue;
                }
                Err(error) => return Err(error.to_string()),
            }
            match byte[0] {
                b'\r' | b'\n' => {
                    print!("\r\n\x1b[?25l");
                    io::stdout().flush().map_err(|error| error.to_string())?;
                    return Ok(input);
                }
                3 | 27 => {
                    print!("\r\n\x1b[?25l");
                    io::stdout().flush().map_err(|error| error.to_string())?;
                    return Ok(String::new());
                }
                8 | 127 => {
                    if input.pop().is_some() {
                        print!("\x08 \x08");
                        io::stdout().flush().map_err(|error| error.to_string())?;
                    }
                }
                byte if !byte.is_ascii_control() => {
                    let ch = byte as char;
                    input.push(ch);
                    print!("{ch}");
                    io::stdout().flush().map_err(|error| error.to_string())?;
                }
                _ => {}
            }
        }
    }

    pub(crate) fn show_message(&mut self, message: &str) -> Result<(), String> {
        self.status_message = Some(message.to_string());
        print!(
            "\x1b[{};1H\x1b[2K{}",
            terminal_size().1,
            truncate_line(message, terminal_size().0 as usize)
        );
        io::stdout().flush().map_err(|error| error.to_string())
    }

    fn show_error(&mut self, context: &str, error: &str) -> Result<(), String> {
        let message = format!("{context}: {error}");
        let _ = append_runtime_log(&self.repo, &message);
        self.show_message(&message)
    }

    fn move_down(&mut self) {
        if self.selected + 1 < self.sessions.len() {
            self.selected += 1;
        }
    }

    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn draw(&self) -> Result<(), String> {
        view::draw(
            &self.repo,
            &self.config,
            &self.sessions,
            self.selected,
            "normal",
            self.status_message.as_deref(),
        )
    }
}

fn truncate_ansi_dialog_line(text: &str, max_chars: usize) -> String {
    if strip_ansi(text).chars().count() <= max_chars {
        return text.to_string();
    }
    truncate_line(&strip_ansi(text), max_chars)
}
