use std::io;
use std::thread;
use std::time::Duration;

use async_channel::Receiver;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event as CrosstermEvent};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub(crate) type ShellTerminal = Terminal<CrosstermBackend<io::Stdout>>;

/// Why a terminal view could not be opened or given back.
///
/// A restore that fails is as much a fault as an open that fails: it
/// leaves the operator's terminal in raw mode on the alternate screen,
/// so it is reported with the same detail rather than swallowed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TerminalError {
    #[error("failed to enable raw mode for {view}: {source}")]
    EnableRawMode {
        view: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to enter {view} screen: {source}")]
    EnterScreen {
        view: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to create terminal for {view}: {source}")]
    CreateTerminal {
        view: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to disable raw mode for {view}: {source}")]
    DisableRawMode {
        view: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to leave {view} screen: {source}")]
    LeaveScreen {
        view: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to restore cursor after {view}: {source}")]
    RestoreCursor {
        view: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to draw the {view}: {source}")]
    Draw {
        view: &'static str,
        #[source]
        source: io::Error,
    },
}

pub(crate) struct Session {
    terminal: ShellTerminal,
    mouse_capture: bool,
    context: &'static str,
}

impl Session {
    pub(crate) fn open(mouse_capture: bool, context: &'static str) -> Result<Self, TerminalError> {
        enable_raw_mode().map_err(|source| TerminalError::EnableRawMode {
            view: context,
            source,
        })?;
        let mut stdout = io::stdout();
        let entered = if mouse_capture {
            execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        } else {
            execute!(stdout, EnterAlternateScreen)
        };
        entered.map_err(|source| TerminalError::EnterScreen {
            view: context,
            source,
        })?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout)).map_err(|source| {
            TerminalError::CreateTerminal {
                view: context,
                source,
            }
        })?;
        Ok(Self {
            terminal,
            mouse_capture,
            context,
        })
    }

    pub(crate) fn terminal(&mut self) -> &mut ShellTerminal {
        &mut self.terminal
    }

    pub(crate) fn close(mut self) -> Result<(), TerminalError> {
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
    thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(100)) {
                Ok(false) => {}
                Ok(true) => {
                    let event = event::read()
                        .unwrap_or_else(|error| panic!("failed to read TUI event: {error}"));
                    if tx.send_blocking(event).is_err() {
                        break;
                    }
                }
                Err(error) => panic!("failed to poll TUI events: {error}"),
            }
        }
    });
    rx
}

fn restore_terminal(
    terminal: &mut ShellTerminal,
    mouse_capture: bool,
    context: &'static str,
) -> Result<(), TerminalError> {
    disable_raw_mode().map_err(|source| TerminalError::DisableRawMode {
        view: context,
        source,
    })?;
    let left = if mouse_capture {
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )
    } else {
        execute!(terminal.backend_mut(), LeaveAlternateScreen)
    };
    left.map_err(|source| TerminalError::LeaveScreen {
        view: context,
        source,
    })?;
    terminal
        .show_cursor()
        .map_err(|source| TerminalError::RestoreCursor {
            view: context,
            source,
        })
}
