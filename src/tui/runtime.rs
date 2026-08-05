use std::{
    io,
    time::{Duration, Instant},
};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableFocusChange, EnableFocusChange, EnableMouseCapture, Event, KeyEvent,
        KeyboardEnhancementFlags, MouseEvent, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};

use crate::view;

pub(crate) struct TerminalRuntime {
    active: bool,
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

pub(crate) enum RuntimeEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize,
    FocusGained,
    FocusLost,
}

pub(crate) struct DrawTiming {
    pub render: Duration,
    pub terminal: Duration,
}

impl TerminalRuntime {
    pub(crate) fn enter() -> Result<Self, String> {
        enable_raw_mode().map_err(|error| error.to_string())?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableFocusChange,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            Hide
        )
        .map_err(|error| error.to_string())
        {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::new(backend).map_err(|error| error.to_string()) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = execute!(
                    io::stdout(),
                    PopKeyboardEnhancementFlags,
                    DisableFocusChange,
                    LeaveAlternateScreen,
                    Show
                );
                let _ = disable_raw_mode();
                return Err(error);
            }
        };
        Ok(Self {
            active: true,
            terminal,
        })
    }

    pub(crate) fn draw(&mut self, model: &view::FrameModel<'_>) -> Result<DrawTiming, String> {
        let started = Instant::now();
        let mut render = Duration::ZERO;
        self.terminal
            .draw(|frame| {
                let render_started = Instant::now();
                crate::view::render(frame, model);
                render = render_started.elapsed();
            })
            .map(|_| DrawTiming {
                render,
                terminal: started.elapsed(),
            })
            .map_err(|error| error.to_string())
    }

    pub(crate) fn area(&self) -> Result<Rect, String> {
        self.terminal
            .size()
            .map(|size| Rect::new(0, 0, size.width, size.height))
            .map_err(|error| error.to_string())
    }

    pub(crate) fn suspend(&mut self) -> Result<(), String> {
        if !self.active {
            return Ok(());
        }
        let started = Instant::now();
        let result = self.leave_active_terminal();
        crate::flight_recorder::record(
            "lifecycle",
            "terminal_suspend",
            Some(started.elapsed()),
            vec![crate::flight_recorder::boolean("success", result.is_ok())],
        );
        result?;
        self.active = false;
        Ok(())
    }

    pub(crate) fn resume(&mut self) -> Result<(), String> {
        if self.active {
            return Ok(());
        }
        let started = Instant::now();
        let result = (|| {
            enable_raw_mode().map_err(|error| error.to_string())?;
            execute!(
                io::stdout(),
                EnterAlternateScreen,
                EnableMouseCapture,
                EnableFocusChange,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
                Hide
            )
            .map_err(|error| error.to_string())?;
            self.active = true;
            self.terminal.clear().map_err(|error| error.to_string())?;
            Ok(())
        })();
        crate::flight_recorder::record(
            "lifecycle",
            "terminal_resume",
            Some(started.elapsed()),
            vec![crate::flight_recorder::boolean("success", result.is_ok())],
        );
        result
    }

    pub(crate) fn poll_event(&mut self, timeout: Duration) -> Result<Option<RuntimeEvent>, String> {
        let started = Instant::now();
        if !event::poll(timeout).map_err(|error| error.to_string())? {
            crate::flight_recorder::record(
                "input",
                "poll",
                Some(started.elapsed()),
                vec![crate::flight_recorder::text("result", "timeout")],
            );
            crate::flight_recorder::terminal_poll_timed_out();
            return Ok(None);
        }
        let read_started = Instant::now();
        let event = event::read().map_err(|error| error.to_string())?;
        let read_elapsed = read_started.elapsed();
        let poll_elapsed = started.elapsed();
        let event_kind = match &event {
            Event::Key(_) => "key",
            Event::Mouse(_) => "mouse",
            Event::Resize(_, _) => "resize",
            Event::FocusGained => "focus_gained",
            Event::FocusLost => "focus_lost",
            Event::Paste(_) => "paste",
        };
        crate::flight_recorder::terminal_input(event_kind);
        match &event {
            Event::FocusGained => {
                crate::flight_recorder::record("lifecycle", "focus_gained", None, Vec::new())
            }
            Event::FocusLost => {
                crate::flight_recorder::record("lifecycle", "focus_lost", None, Vec::new())
            }
            _ => {}
        }
        let result = match event {
            Event::Key(event) => Ok(Some(RuntimeEvent::Key(event))),
            Event::Mouse(event) => Ok(Some(RuntimeEvent::Mouse(event))),
            Event::Resize(_, _) => Ok(Some(RuntimeEvent::Resize)),
            Event::FocusGained => Ok(Some(RuntimeEvent::FocusGained)),
            Event::FocusLost => Ok(Some(RuntimeEvent::FocusLost)),
            Event::Paste(_) => {
                crate::flight_recorder::finish_pending_input_without_frame();
                Ok(None)
            }
        };
        crate::flight_recorder::record(
            "input",
            "poll",
            Some(poll_elapsed),
            vec![
                crate::flight_recorder::text("result", "event"),
                crate::flight_recorder::unsigned("read_us", read_elapsed.as_micros()),
            ],
        );
        result
    }

    pub(crate) fn suspend_for<T>(
        &mut self,
        f: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        self.suspend()?;
        let away_started = Instant::now();
        let result = f();
        let away = away_started.elapsed();
        let resume_result = self.resume();
        crate::flight_recorder::record(
            "lifecycle",
            "suspended_operation",
            Some(away),
            vec![
                crate::flight_recorder::boolean("operation_success", result.is_ok()),
                crate::flight_recorder::boolean("resume_success", resume_result.is_ok()),
            ],
        );
        resume_result?;
        result
    }

    fn leave_active_terminal(&mut self) -> Result<(), String> {
        execute!(
            io::stdout(),
            crossterm::event::DisableMouseCapture,
            PopKeyboardEnhancementFlags,
            DisableFocusChange,
            LeaveAlternateScreen,
            Clear(ClearType::All),
            MoveTo(0, 0),
            Show
        )
        .map_err(|error| error.to_string())?;
        disable_raw_mode().map_err(|error| error.to_string())
    }
}

impl Drop for TerminalRuntime {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            crossterm::event::DisableMouseCapture,
            PopKeyboardEnhancementFlags,
            DisableFocusChange,
            LeaveAlternateScreen,
            Show
        );
        let _ = disable_raw_mode();
    }
}
