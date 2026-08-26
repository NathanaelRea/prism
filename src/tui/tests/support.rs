use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::layout::Rect;

use crate::agent::AgentState;
use crate::config::Config;
use crate::remote::{PrCache, PrSummary};
use crate::repo::Repository;
use crate::session::Session;
use crate::tui_runtime::{DrawTiming, RuntimeEvent, TerminalDriver};

use super::super::{ManagedRepo, Tui};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FrameSummary {
    pub(super) focus: super::super::PanelFocus,
    pub(super) selected_branch: Option<String>,
    pub(super) dialog_title: Option<String>,
    pub(super) status_message: Option<String>,
}

struct ScriptedEvent {
    event: RuntimeEvent,
    dialog_title: Option<String>,
}

pub(super) struct ScriptedTerminal {
    events: VecDeque<ScriptedEvent>,
    pub(super) draws: usize,
    pub(super) suspends: usize,
    pub(super) resumes: usize,
    pub(super) frames: Vec<FrameSummary>,
    area: Rect,
    draw_failure: Option<String>,
    poll_failure: Option<String>,
    suspend_failure: Option<String>,
    resume_failure: Option<String>,
}

impl Default for ScriptedTerminal {
    fn default() -> Self {
        Self {
            events: VecDeque::new(),
            draws: 0,
            suspends: 0,
            resumes: 0,
            frames: Vec::new(),
            area: Rect::new(0, 0, 120, 40),
            draw_failure: None,
            poll_failure: None,
            suspend_failure: None,
            resume_failure: None,
        }
    }
}

#[allow(dead_code)]
impl ScriptedTerminal {
    pub(super) fn with_size(cols: u16, rows: u16) -> Self {
        Self {
            area: Rect::new(0, 0, cols, rows),
            ..Self::default()
        }
    }

    pub(super) fn push_event(&mut self, event: RuntimeEvent) {
        self.events.push_back(ScriptedEvent {
            event,
            dialog_title: None,
        });
    }

    pub(super) fn push_event_for_dialog(
        &mut self,
        dialog_title: impl Into<String>,
        event: RuntimeEvent,
    ) {
        self.events.push_back(ScriptedEvent {
            event,
            dialog_title: Some(dialog_title.into()),
        });
    }

    pub(super) fn push_key(&mut self, code: KeyCode) {
        self.push_modified_key(code, KeyModifiers::NONE);
    }

    pub(super) fn push_modified_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.push_event(RuntimeEvent::Key(KeyEvent::new(code, modifiers)));
    }

    pub(super) fn push_resize(&mut self, cols: u16, rows: u16) {
        self.area = Rect::new(0, 0, cols, rows);
        self.push_event(RuntimeEvent::Resize);
    }

    pub(super) fn push_focus_gained(&mut self) {
        self.push_event(RuntimeEvent::FocusGained);
    }

    pub(super) fn push_focus_lost(&mut self) {
        self.push_event(RuntimeEvent::FocusLost);
    }

    pub(super) fn push_mouse(&mut self, event: MouseEvent) {
        self.push_event(RuntimeEvent::Mouse(event));
    }

    pub(super) fn queue_text_dialog(&mut self, text: &str) {
        self.queue_text(text);
        self.push_key(KeyCode::Enter);
    }

    pub(super) fn queue_text_dialog_named(&mut self, title: &str, text: &str) {
        for character in text.chars() {
            let code = if character == '\n' {
                KeyCode::Enter
            } else {
                KeyCode::Char(character)
            };
            self.push_event_for_dialog(
                title,
                RuntimeEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            );
        }
        self.push_event_for_dialog(
            title,
            RuntimeEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
    }

    pub(super) fn queue_choice_dialog(&mut self, key: char) {
        self.push_key(KeyCode::Char(key));
    }

    pub(super) fn queue_choice_dialog_named(&mut self, title: &str, key: char) {
        self.push_event_for_dialog(
            title,
            RuntimeEvent::Key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)),
        );
    }

    pub(super) fn queue_confirmation(&mut self, confirmed: bool) {
        self.push_key(KeyCode::Char(if confirmed { 'y' } else { 'n' }));
        self.push_key(KeyCode::Enter);
    }

    pub(super) fn queue_create_session_form(&mut self, prompt: &str) {
        self.queue_text(prompt);
        self.push_modified_key(KeyCode::Enter, KeyModifiers::CONTROL);
    }

    pub(super) fn queue_text(&mut self, text: &str) {
        for character in text.chars() {
            if character == '\n' {
                self.push_key(KeyCode::Enter);
            } else {
                self.push_key(KeyCode::Char(character));
            }
        }
    }

    pub(super) fn fail_next_draw(&mut self, message: impl Into<String>) {
        self.draw_failure = Some(message.into());
    }

    pub(super) fn fail_next_poll(&mut self, message: impl Into<String>) {
        self.poll_failure = Some(message.into());
    }

    pub(super) fn fail_next_suspend(&mut self, message: impl Into<String>) {
        self.suspend_failure = Some(message.into());
    }

    pub(super) fn fail_next_resume(&mut self, message: impl Into<String>) {
        self.resume_failure = Some(message.into());
    }
}

impl TerminalDriver for ScriptedTerminal {
    fn draw(&mut self, model: &crate::view::FrameModel<'_>) -> Result<DrawTiming, String> {
        if let Some(error) = self.draw_failure.take() {
            return Err(error);
        }
        self.draws += 1;
        let dialog_title = model.dialog.as_ref().map(|dialog| match dialog {
            crate::view::DialogModel::Help { .. } => "Help".to_string(),
            crate::view::DialogModel::Confirm { title, .. }
            | crate::view::DialogModel::Notice { title, .. }
            | crate::view::DialogModel::Prompt { title, .. }
            | crate::view::DialogModel::Form { title, .. }
            | crate::view::DialogModel::OrderedToggle { title, .. }
            | crate::view::DialogModel::Progress { title, .. } => title.clone(),
            crate::view::DialogModel::Choice { choices } => choices.title.clone(),
        });
        self.frames.push(FrameSummary {
            focus: model.focus,
            selected_branch: model
                .selected_session
                .and_then(|index| model.sessions.get(index))
                .map(|session| session.branch.clone()),
            dialog_title,
            status_message: model.status_message.map(str::to_string),
        });
        Ok(DrawTiming {
            render: Duration::ZERO,
            terminal: Duration::ZERO,
        })
    }

    fn area(&self) -> Result<Rect, String> {
        Ok(self.area)
    }

    fn poll_event(&mut self, _timeout: Duration) -> Result<Option<RuntimeEvent>, String> {
        if let Some(error) = self.poll_failure.take() {
            return Err(error);
        }
        let expected_dialog = self
            .events
            .front()
            .and_then(|event| event.dialog_title.as_deref());
        let current_dialog = self
            .frames
            .last()
            .and_then(|frame| frame.dialog_title.as_deref());
        if expected_dialog.is_some() && expected_dialog != current_dialog {
            return Ok(None);
        }
        Ok(self.events.pop_front().map(|event| event.event))
    }

    fn suspend(&mut self) -> Result<(), String> {
        if let Some(error) = self.suspend_failure.take() {
            return Err(error);
        }
        self.suspends += 1;
        Ok(())
    }

    fn resume(&mut self) -> Result<(), String> {
        if let Some(error) = self.resume_failure.take() {
            return Err(error);
        }
        self.resumes += 1;
        Ok(())
    }
}

pub(super) fn test_tui() -> Tui {
    let repos = vec![
        ManagedRepo::new(
            Repository {
                root: PathBuf::from("/repo-one"),
            },
            test_config(),
            Some('1'),
        ),
        ManagedRepo::new(
            Repository {
                root: PathBuf::from("/repo-two"),
            },
            test_config(),
            Some('2'),
        ),
    ];
    let sessions = vec![
        test_session(0, "/repo-one", "main"),
        test_session(0, "/repo-one", "feature-one"),
        test_session(1, "/repo-two", "main"),
        test_session(1, "/repo-two", "feature-two"),
    ];
    Tui::new(repos, 0, sessions)
}

pub(super) fn test_session(repo_index: usize, root: &str, branch: &str) -> Session {
    let path = PathBuf::from(format!("{root}/{branch}"));
    let _ = fs::create_dir_all(&path);
    Session {
        repo_index,
        repo_label: format!("repo-{repo_index}"),
        repo_key: None,
        path: path.clone(),
        incarnation: String::new(),
        path_display: path.display().to_string(),
        branch: branch.to_string(),
        prompt_summary: String::new(),
        classification: crate::session::SessionClassification::Work,
        visibility: 0,
        adopted: false,
        hidden: false,
        status_label: "clean".to_string(),
        agent_state: AgentState::Idle,
        opencode_status: None,
        pr: PrCache::default(),
        wt_columns: BTreeMap::new(),
        unseen_comments: false,
    }
}

pub(super) fn test_config() -> Config {
    let mut config = crate::test_support::test_config();
    config.default_agent = "opencode".to_string();
    config.default_base = Some("main".to_string());
    config
}

pub(super) fn test_pr_summary(merged: bool) -> PrSummary {
    PrSummary {
        number: 1,
        change_request_identity: None,
        native_state_evidence: crate::remote::NativeStateEvidence::default(),
        title: "PR".to_string(),
        author: "author".to_string(),
        body: String::new(),
        url: "https://example.test/pr/1".to_string(),
        state: if merged { "MERGED" } else { "OPEN" }.to_string(),
        review_decision: String::new(),
        requested_reviewers: Vec::new(),
        head_ref: "feature".to_string(),
        base_ref: "main".to_string(),
        head_sha: "abc123".to_string(),
        updated_at: String::new(),
        check_status: String::new(),
        merge_state_status: String::new(),
        queue_state: String::new(),
        comment_count: 0,
        merged,
        draft: false,
    }
}

pub(super) fn test_change_request_identity(
    provider: crate::remote::ProviderKind,
) -> crate::remote::CanonicalChangeRequestIdentity {
    test_change_request_identity_for(provider, "example/repo", "change-request-1")
}

pub(super) fn test_change_request_identity_for(
    provider: crate::remote::ProviderKind,
    project_path: &str,
    native_id: &str,
) -> crate::remote::CanonicalChangeRequestIdentity {
    let host = match provider {
        crate::remote::ProviderKind::GitHub => "github.com",
        crate::remote::ProviderKind::GitLab => "gitlab.com",
        crate::remote::ProviderKind::Forgejo => "codeberg.org",
    };
    let host = crate::remote::HostIdentity::new(host, None).unwrap();
    let repository = crate::remote::RemoteRepositoryId::new(provider, host, project_path).unwrap();
    crate::remote::CanonicalChangeRequestIdentity::new(
        &repository,
        &crate::remote::NativeChangeRequestId::new(native_id).unwrap(),
        &repository,
        &repository,
    )
}

pub(super) fn unique_temp_dir(prefix: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{unique}", std::process::id()))
}
