use std::io;
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result};
use async_channel::Receiver;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event as CrosstermEvent};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

pub(crate) type ShellTerminal = Terminal<CrosstermBackend<io::Stdout>>;

pub(crate) struct Session {
    terminal: ShellTerminal,
    mouse_capture: bool,
    context: &'static str,
}

impl Session {
    pub(crate) fn open(mouse_capture: bool, context: &'static str) -> Result<Self> {
        enable_raw_mode().with_context(|| format!("failed to enable raw mode for {context}"))?;
        let mut stdout = io::stdout();
        if mouse_capture {
            execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
                .with_context(|| format!("failed to enter {context} screen"))?;
        } else {
            execute!(stdout, EnterAlternateScreen)
                .with_context(|| format!("failed to enter {context} screen"))?;
        }
        let terminal = Terminal::new(CrosstermBackend::new(stdout))
            .with_context(|| format!("failed to create terminal for {context}"))?;
        Ok(Self {
            terminal,
            mouse_capture,
            context,
        })
    }

    pub(crate) fn terminal(&mut self) -> &mut ShellTerminal {
        &mut self.terminal
    }

    pub(crate) fn close(mut self) -> Result<()> {
        restore_terminal(&mut self.terminal, self.mouse_capture, self.context)?;
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = restore_terminal(&mut self.terminal, self.mouse_capture, self.context);
    }
}

pub(crate) fn spawn_events() -> Receiver<CrosstermEvent> {
    let (tx, rx) = async_channel::unbounded();
    thread::spawn(move || loop {
        if event::poll(Duration::from_millis(100)).unwrap_or(false) {
            match event::read() {
                Ok(event) => {
                    if tx.send_blocking(event).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

fn restore_terminal(
    terminal: &mut ShellTerminal,
    mouse_capture: bool,
    context: &str,
) -> Result<()> {
    disable_raw_mode().with_context(|| format!("failed to disable raw mode for {context}"))?;
    if mouse_capture {
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )
        .with_context(|| format!("failed to leave {context} screen"))?;
    } else {
        execute!(terminal.backend_mut(), LeaveAlternateScreen)
            .with_context(|| format!("failed to leave {context} screen"))?;
    }
    terminal
        .show_cursor()
        .with_context(|| format!("failed to restore cursor after {context}"))
}
