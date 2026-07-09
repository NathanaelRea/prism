use std::{io, time::Duration};

use crossterm::{
    cursor::{Hide, Show},
    event::{self, EnableMouseCapture, Event, KeyEvent, MouseEvent},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
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
}

impl TerminalRuntime {
    pub(crate) fn enter() -> Result<Self, String> {
        enable_raw_mode().map_err(|error| error.to_string())?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture, Hide)
            .map_err(|error| error.to_string())
        {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::new(backend).map_err(|error| error.to_string()) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
                let _ = disable_raw_mode();
                return Err(error);
            }
        };
        Ok(Self {
            active: true,
            terminal,
        })
    }

    pub(crate) fn draw(&mut self, model: &view::FrameModel<'_>) -> Result<(), String> {
        self.terminal
            .draw(|frame| crate::view::render(frame, model))
            .map(|_| ())
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
        self.leave_active_terminal()?;
        self.active = false;
        Ok(())
    }

    pub(crate) fn resume(&mut self) -> Result<(), String> {
        if self.active {
            return Ok(());
        }
        enable_raw_mode().map_err(|error| error.to_string())?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide)
            .map_err(|error| error.to_string())?;
        self.active = true;
        self.terminal.clear().map_err(|error| error.to_string())?;
        Ok(())
    }

    pub(crate) fn poll_event(&mut self, timeout: Duration) -> Result<Option<RuntimeEvent>, String> {
        if !event::poll(timeout).map_err(|error| error.to_string())? {
            return Ok(None);
        }
        match event::read().map_err(|error| error.to_string())? {
            Event::Key(event) => Ok(Some(RuntimeEvent::Key(event))),
            Event::Mouse(event) => Ok(Some(RuntimeEvent::Mouse(event))),
            Event::Resize(_, _) => Ok(Some(RuntimeEvent::Resize)),
            Event::FocusGained => Ok(Some(RuntimeEvent::FocusGained)),
            Event::FocusLost | Event::Paste(_) => Ok(None),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn suspend_for<T>(
        &mut self,
        f: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        self.suspend()?;
        let result = f();
        let resume_result = self.resume();
        resume_result?;
        result
    }

    fn leave_active_terminal(&mut self) -> Result<(), String> {
        execute!(
            io::stdout(),
            crossterm::event::DisableMouseCapture,
            LeaveAlternateScreen,
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
            LeaveAlternateScreen,
            Show
        );
        let _ = disable_raw_mode();
    }
}
