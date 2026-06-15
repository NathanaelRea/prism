use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, ErrorKind, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crate::agent::AgentState;
use crate::config::Config;
use crate::github::{PrCache, PrSummary};
use crate::input::{Key, KeyInput};
use crate::repo::Repository;
use crate::session::{Session, append_runtime_log};
use crate::terminal::{RawTerminal, stdin_is_tty, terminal_size, write_stdout};
use crate::tmux::TmuxWindow;
use crate::util::{strip_ansi, truncate_line, yes};
use crate::view;

pub struct Tui {
    pub(crate) repo: Repository,
    pub(crate) config: Config,
    pub(crate) sessions: Vec<Session>,
    pub(crate) allow_dirty: bool,
    pub(crate) selected: usize,
    pub(crate) pr_poll_tx: Sender<PrPollResult>,
    pub(crate) pr_poll_rx: Receiver<PrPollResult>,
    pub(crate) pr_polls_in_flight: BTreeSet<PrPollKey>,
    pub(crate) pr_summary_poll_in_flight: bool,
    pub(crate) pr_summary_last_polled: Option<std::time::Instant>,
    pub(crate) tmux_warmup_tx: Sender<TmuxWarmupResult>,
    pub(crate) tmux_warmup_rx: Receiver<TmuxWarmupResult>,
    pub(crate) tmux_warmups_in_flight: BTreeSet<TmuxWarmupKey>,
    pub(crate) tmux_generations: BTreeMap<TmuxSlotKey, u64>,
    pub(crate) wt_poll_tx: Sender<WtPollResult>,
    pub(crate) wt_poll_rx: Receiver<WtPollResult>,
    pub(crate) wt_poll_in_flight: bool,
    pub(crate) default_branch_poll_tx: Sender<DefaultBranchPollResult>,
    pub(crate) default_branch_poll_rx: Receiver<DefaultBranchPollResult>,
    pub(crate) default_branch_poll_in_flight: bool,
    pub(crate) default_branch_last_polled: Option<std::time::Instant>,
    pub(crate) session_filter: String,
    pub(crate) leader_hint: Option<LeaderHint>,
    status_message: Option<String>,
    status_message_until: Option<Instant>,
}

const STATUS_MESSAGE_DURATION: Duration = Duration::from_secs(5);

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeaderHint {
    Root,
    Git,
}

pub(crate) struct WtPollResult {
    pub columns: Result<BTreeMap<PathBuf, BTreeMap<String, String>>, String>,
}

pub(crate) struct DefaultBranchPollResult {
    pub branch: String,
    pub path: PathBuf,
    pub status_label: Result<String, String>,
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
        let (wt_poll_tx, wt_poll_rx) = mpsc::channel();
        let (default_branch_poll_tx, default_branch_poll_rx) = mpsc::channel();
        Self {
            repo,
            config,
            sessions,
            allow_dirty,
            selected: 0,
            pr_poll_tx,
            pr_poll_rx,
            pr_polls_in_flight: BTreeSet::new(),
            pr_summary_poll_in_flight: false,
            pr_summary_last_polled: None,
            tmux_warmup_tx,
            tmux_warmup_rx,
            tmux_warmups_in_flight: BTreeSet::new(),
            tmux_generations: BTreeMap::new(),
            wt_poll_tx,
            wt_poll_rx,
            wt_poll_in_flight: false,
            default_branch_poll_tx,
            default_branch_poll_rx,
            default_branch_poll_in_flight: false,
            default_branch_last_polled: None,
            session_filter: String::new(),
            leader_hint: None,
            status_message: None,
            status_message_until: None,
        }
    }

    pub fn run(&mut self) -> Result<(), String> {
        if !stdin_is_tty() {
            return Err("TUI requires an interactive terminal".to_string());
        }

        let mut raw = RawTerminal::enter()?;
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        self.start_default_branch_status_poll(true);
        self.draw()?;
        let mut stdin = io::stdin();
        let mut buffer = [0_u8; 64];
        let mut key_input = KeyInput::default();
        let mut pending_g = false;
        let mut last_size = terminal_size();

        loop {
            let agents_changed = self.poll_agents();
            let tmux_changed = self.poll_tmux_agent_warmup();
            let wt_changed = self.poll_wt_columns();
            let default_branch_changed = self.poll_default_branch_status();
            let prs_changed = self.poll_pull_requests(false);
            self.start_default_branch_status_poll(false);
            let status_changed = self.expire_status_message();
            let current_size = terminal_size();
            let resized = current_size != last_size;
            if resized {
                last_size = current_size;
            }
            if agents_changed
                || tmux_changed
                || wt_changed
                || default_branch_changed
                || prs_changed
                || status_changed
                || resized
            {
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
                        self.clear_leader_hint();
                        pending_g = false;
                        should_quit = self.confirm_quit()?;
                    }
                    Key::Down => {
                        self.clear_leader_hint();
                        self.move_down();
                        pending_g = false;
                    }
                    Key::Up => {
                        self.clear_leader_hint();
                        self.move_up();
                        pending_g = false;
                    }
                    Key::Bottom => {
                        self.clear_leader_hint();
                        pending_g = false;
                        self.select_bottom_visible();
                    }
                    Key::G => {
                        self.clear_leader_hint();
                        if pending_g {
                            self.select_top_visible();
                            pending_g = false;
                        } else {
                            pending_g = true;
                        }
                    }
                    Key::Leader => {
                        self.leader_hint = Some(LeaderHint::Root);
                    }
                    Key::LeaderGit => {
                        self.leader_hint = Some(LeaderHint::Git);
                    }
                    Key::AgentMode => {
                        self.clear_leader_hint();
                        pending_g = false;
                        self.enter_agent_mode(&mut raw)?;
                    }
                    Key::LazyGit => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if let Err(error) = self.open_tmux_window(&mut raw, TmuxWindow::LazyGit) {
                            self.show_error("lazygit failed", &error)?;
                        }
                    }
                    Key::Terminal => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if let Err(error) = self.open_tmux_window(&mut raw, TmuxWindow::Terminal) {
                            self.show_error("terminal failed", &error)?;
                        }
                    }
                    Key::Help => {
                        self.clear_leader_hint();
                        pending_g = false;
                        self.show_keybindings_dialog()?;
                    }
                    Key::Refresh => {
                        self.clear_leader_hint();
                        pending_g = false;
                        self.refresh_sessions()?;
                        self.start_tmux_agent_warmup();
                        self.start_wt_column_poll();
                        self.start_default_branch_status_poll(true);
                        self.poll_pull_requests(true);
                    }
                    Key::ReviewFix => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if let Err(error) = self.start_review_fix() {
                            self.show_error("review fix failed", &error)?;
                        }
                    }
                    Key::Push => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if let Err(error) = self.push_selected_branch() {
                            self.show_error("push failed", &error)?;
                        }
                    }
                    Key::Merge => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if let Err(error) = self.merge_selected_pr() {
                            self.show_error("merge failed", &error)?;
                        }
                    }
                    Key::PullDefault => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if let Err(error) = self.pull_default_branch() {
                            self.show_error("pull failed", &error)?;
                        }
                    }
                    Key::Create => {
                        self.clear_leader_hint();
                        pending_g = false;
                        match self.create_session() {
                            Ok(true) => self.enter_agent_mode(&mut raw)?,
                            Ok(false) => {}
                            Err(error) => self.show_error("create session failed", &error)?,
                        }
                    }
                    Key::Delete => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if let Err(error) = self.delete_session() {
                            self.show_error("delete failed", &error)?;
                        }
                    }
                    Key::EditConfig => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if let Err(error) = self.edit_config(&mut raw) {
                            self.show_error("edit config failed", &error)?;
                        }
                    }
                    Key::Search => {
                        self.clear_leader_hint();
                        pending_g = false;
                        self.search_sessions()?;
                    }
                    Key::Other => {
                        self.clear_leader_hint();
                        pending_g = false;
                    }
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
        let items = [
            "Space Space  open selected agent",
            "Enter        open selected agent",
            "Space Enter  open tmux window 3: terminal",
            "Space g g    open tmux window 2: lazygit",
            "Ctrl-/       open tmux window 3: terminal",
            "Space g P    push branch, create PR if needed",
            "Space g M    merge selected PR",
            "Space g f    stage review-fix prompt",
            "Space g p    pull default branch",
            "p            pull default branch",
            "c            create worktree session",
            "e            edit Prism repo config, then reload",
            "/            search/filter sessions",
            "?            show keybindings; / filters this dialog",
            "D            delete worktree/session",
            "j/k, arrows  move selection",
            "g g / G      top / bottom",
            "r            refresh",
            "q, Ctrl-C    quit",
        ];
        let mut filter = String::new();
        let mut editing_filter = false;
        self.draw_keybindings_dialog(&items, &filter)?;

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
                b'/' if !editing_filter => {
                    editing_filter = true;
                    filter.clear();
                    self.draw_keybindings_dialog(&items, &filter)?;
                }
                b'\r' | b'\n' if editing_filter => editing_filter = false,
                8 | 127 if editing_filter => {
                    filter.pop();
                    self.draw_keybindings_dialog(&items, &filter)?;
                }
                3 | 27 | b'q' => return Ok(()),
                byte if editing_filter && !byte.is_ascii_control() => {
                    filter.push(byte as char);
                    self.draw_keybindings_dialog(&items, &filter)?;
                }
                _ if !editing_filter => return Ok(()),
                _ => {}
            }
        }
    }

    fn draw_keybindings_dialog(&self, items: &[&str], filter: &str) -> Result<(), String> {
        let query = filter.trim().to_ascii_lowercase();
        let mut lines = vec!["Keybindings".to_string()];
        lines.push(if filter.is_empty() {
            "Filter: /".to_string()
        } else {
            format!("Filter: /{filter}")
        });
        lines.push(String::new());
        lines.extend(
            items
                .iter()
                .filter(|line| query.is_empty() || line.to_ascii_lowercase().contains(&query))
                .map(|line| (*line).to_string()),
        );
        if lines.len() == 3 {
            lines.push("No matching keybindings".to_string());
        }
        lines.extend([String::new(), "Esc/q closes. / searches.".to_string()]);
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

        let mut frame = format!(
            "\x1b[?25l\x1b[{top};{left}H+{}+",
            "-".repeat(width.saturating_sub(2))
        );
        for (index, line) in lines.iter().enumerate() {
            let y = top + index + 1;
            let text_width = width.saturating_sub(4);
            let text = truncate_line(line, text_width);
            frame.push_str(&format!(
                "\x1b[{y};{left}H| {:<text_width$} |",
                text,
                text_width = text_width
            ));
        }
        frame.push_str(&format!(
            "\x1b[{};{}H+{}+",
            top + height - 1,
            left,
            "-".repeat(width.saturating_sub(2))
        ));
        write_stdout(&frame)
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

    pub(crate) fn prompt_line_dialog(
        &self,
        title: &str,
        prompt: &str,
        initial: &str,
    ) -> Result<Option<String>, String> {
        let mut input = initial.to_string();
        self.draw()?;
        self.draw_prompt_dialog(title, prompt, &input)?;
        let mut stdin = io::stdin();
        let mut byte = [0_u8; 1];
        loop {
            match stdin.read(&mut byte) {
                Ok(1) => {}
                Ok(_) => {
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }
                Err(error) => return Err(error.to_string()),
            }
            match byte[0] {
                b'\r' | b'\n' => {
                    write_stdout("\x1b[?25l")?;
                    return Ok(Some(input));
                }
                3 | 27 => {
                    write_stdout("\x1b[?25l")?;
                    return Ok(None);
                }
                8 | 127 => {
                    input.pop();
                    self.draw_prompt_dialog(title, prompt, &input)?;
                }
                byte if !byte.is_ascii_control() => {
                    input.push(byte as char);
                    self.draw_prompt_dialog(title, prompt, &input)?;
                }
                _ => {}
            }
        }
    }

    pub(crate) fn show_loading_dialog(&self, title: &str, message: &str) -> Result<(), String> {
        self.draw()?;
        self.draw_static_dialog(title, &["[*] Please wait", message])
    }

    fn draw_prompt_dialog(&self, title: &str, prompt: &str, input: &str) -> Result<(), String> {
        let (cols, rows) = terminal_size();
        let prompt_len = prompt.chars().count();
        let requested_width = title
            .chars()
            .count()
            .max(prompt_len.saturating_add(input.chars().count()))
            .saturating_add(4)
            .max(44);
        let width = requested_width.min((cols as usize).saturating_sub(2).max(12));
        let text_width = width.saturating_sub(4);
        let max_input_width = text_width.saturating_sub(prompt_len);
        let input_display = tail_chars(input, max_input_width);
        let input_line = format!("{prompt}{input_display}");
        let cursor_col = prompt_len
            .saturating_add(input_display.chars().count())
            .min(text_width);
        self.draw_dialog_frame(
            title,
            &[
                String::new(),
                input_line,
                "Enter to continue, Esc to cancel".to_string(),
            ],
            Some((1, cursor_col)),
            width,
            rows,
            cols,
        )
    }

    fn draw_static_dialog(&self, title: &str, lines: &[&str]) -> Result<(), String> {
        let (cols, rows) = terminal_size();
        let requested_width = lines
            .iter()
            .map(|line| line.chars().count())
            .chain(std::iter::once(title.chars().count()))
            .max()
            .unwrap_or(0)
            .saturating_add(4)
            .max(44);
        let width = requested_width.min((cols as usize).saturating_sub(2).max(12));
        let owned_lines = lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
        self.draw_dialog_frame(title, &owned_lines, None, width, rows, cols)
    }

    fn draw_dialog_frame(
        &self,
        title: &str,
        lines: &[String],
        cursor: Option<(usize, usize)>,
        width: usize,
        rows: u16,
        cols: u16,
    ) -> Result<(), String> {
        let height = lines.len() + 3;
        let left = ((cols as usize).saturating_sub(width) / 2).saturating_add(1);
        let top = ((rows as usize).saturating_sub(height) / 3).saturating_add(1);
        let text_width = width.saturating_sub(4);
        let mut frame = format!(
            "\x1b[?25l\x1b[{top};{left}H+{}+",
            "-".repeat(width.saturating_sub(2))
        );
        let title_line = truncate_line(title, text_width);
        frame.push_str(&format!(
            "\x1b[{};{}H| {:<text_width$} |",
            top + 1,
            left,
            title_line,
            text_width = text_width
        ));
        for (index, line) in lines.iter().enumerate() {
            let y = top + index + 2;
            let text = truncate_line(line, text_width);
            frame.push_str(&format!(
                "\x1b[{y};{left}H| {:<text_width$} |",
                text,
                text_width = text_width
            ));
        }
        frame.push_str(&format!(
            "\x1b[{};{}H+{}+",
            top + height - 1,
            left,
            "-".repeat(width.saturating_sub(2))
        ));
        if let Some((line_index, cursor_col)) = cursor {
            frame.push_str(&format!(
                "\x1b[{};{}H\x1b[?25h",
                top + line_index + 2,
                left + 2 + cursor_col
            ));
        }
        write_stdout(&frame)
    }

    fn prompt_line_with_initial(&self, prompt: &str, initial: &str) -> Result<String, String> {
        let mut input = initial.to_string();
        write_stdout(&format!(
            "\x1b[{};1H\x1b[2K\x1b[?25h{}{}",
            terminal_size().1,
            prompt,
            input
        ))?;
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
                    write_stdout("\r\n\x1b[?25l")?;
                    return Ok(input);
                }
                3 | 27 => {
                    write_stdout("\r\n\x1b[?25l")?;
                    return Ok(String::new());
                }
                8 | 127 => {
                    if input.pop().is_some() {
                        write_stdout("\x08 \x08")?;
                    }
                }
                byte if !byte.is_ascii_control() => {
                    let ch = byte as char;
                    input.push(ch);
                    write_stdout(&ch.to_string())?;
                }
                _ => {}
            }
        }
    }

    pub(crate) fn show_message(&mut self, message: &str) -> Result<(), String> {
        self.status_message = Some(message.to_string());
        self.status_message_until = Some(Instant::now() + STATUS_MESSAGE_DURATION);
        let _ = append_runtime_log(&self.repo, message);
        self.draw()
    }

    fn show_error(&mut self, context: &str, error: &str) -> Result<(), String> {
        let message = format!("{context}: {error}");
        self.show_message(&message)
    }

    fn move_down(&mut self) {
        let indices = self.visible_session_indices();
        let current = indices
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        if let Some(next) = indices.get(current + 1).copied() {
            self.selected = next;
            self.mark_selected_seen();
        }
    }

    fn move_up(&mut self) {
        let indices = self.visible_session_indices();
        let current = indices
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        if current > 0
            && let Some(next) = indices.get(current - 1).copied()
        {
            self.selected = next;
            self.mark_selected_seen();
        }
    }

    pub(crate) fn select_top_visible(&mut self) {
        if let Some(index) = self.visible_session_indices().first().copied() {
            self.selected = index;
            self.mark_selected_seen();
        }
    }

    fn select_bottom_visible(&mut self) {
        if let Some(index) = self.visible_session_indices().last().copied() {
            self.selected = index;
            self.mark_selected_seen();
        }
    }

    pub(crate) fn visible_session_indices(&self) -> Vec<usize> {
        let filter = self.session_filter.trim().to_ascii_lowercase();
        self.sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| {
                (filter.is_empty()
                    || session.branch.to_ascii_lowercase().contains(&filter)
                    || session
                        .prompt_summary
                        .to_ascii_lowercase()
                        .contains(&filter)
                    || session.path_display.to_ascii_lowercase().contains(&filter)
                    || session
                        .wt_columns
                        .values()
                        .any(|value| value.to_ascii_lowercase().contains(&filter)))
                .then_some(index)
            })
            .collect()
    }

    fn mark_selected_seen(&mut self) {
        if let Some(session) = self.sessions.get_mut(self.selected) {
            session.unseen_comments = false;
        }
    }

    fn clear_leader_hint(&mut self) {
        self.leader_hint = None;
    }

    fn search_sessions(&mut self) -> Result<(), String> {
        let Some(input) = self.prompt_line_dialog(
            "Search Sessions",
            "Filter (empty clears): ",
            &self.session_filter,
        )?
        else {
            return Ok(());
        };
        self.session_filter = input;
        if !self.visible_session_indices().contains(&self.selected) {
            self.select_top_visible();
        }
        Ok(())
    }

    fn expire_status_message(&mut self) -> bool {
        if self
            .status_message_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.status_message = None;
            self.status_message_until = None;
            return true;
        }
        false
    }

    fn draw(&self) -> Result<(), String> {
        view::draw(
            &self.repo,
            &self.config,
            &self.sessions,
            self.selected,
            "normal",
            self.status_message.as_deref(),
            &self.session_filter,
            self.leader_hint_label(),
        )
    }

    fn leader_hint_label(&self) -> Option<&'static str> {
        match self.leader_hint {
            Some(LeaderHint::Root) => Some("g: git  enter: terminal  space: agent"),
            Some(LeaderHint::Git) => {
                Some("g: lazygit  P: push/create PR  M: merge  f: review fix  p: pull")
            }
            None => None,
        }
    }
}

fn truncate_ansi_dialog_line(text: &str, max_chars: usize) -> String {
    let stripped = strip_ansi(text);
    if stripped.chars().count() <= max_chars {
        return text.to_string();
    }
    let warning_prefix = "\x1b[31m•\x1b[0m ";
    if let Some(rest) = text.strip_prefix(warning_prefix) {
        let visible_prefix = "• ";
        let prefix_width = visible_prefix.chars().count();
        if max_chars > prefix_width {
            return format!(
                "{warning_prefix}{}",
                truncate_line(rest, max_chars - prefix_width)
            );
        }
    }
    truncate_line(&stripped, max_chars)
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return "~".to_string();
    }
    let mut out = String::from("~");
    out.extend(text.chars().skip(count - max_chars + 1));
    out
}

#[cfg(test)]
mod tests {
    use super::truncate_ansi_dialog_line;

    #[test]
    fn truncated_warning_line_keeps_colored_bullet_prefix() {
        assert_eq!(
            truncate_ansi_dialog_line("\x1b[31m•\x1b[0m dirty worktree", 8),
            "\x1b[31m•\x1b[0m dirty~"
        );
    }
}
