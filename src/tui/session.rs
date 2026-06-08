use std::{
    io,
    panic::{self, PanicHookInfo},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use crossterm::{
    cursor, event, execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Frame, Terminal};

type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

/// An owned terminal in raw mode + alternate screen, restored on `Drop` — including on a panic,
/// via a hook that runs before the previous one. This is the single place that knows how to put
/// the terminal back; no tool can leak raw mode.
pub struct Session {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    previous_hook: Arc<Mutex<Option<PanicHook>>>,
}

impl Session {
    /// A keyboard-only session (the default for list/tree TUIs).
    pub fn open() -> Result<Self> {
        Self::open_inner(false)
    }

    /// A session that also captures the mouse, so the wheel scrolls the app instead of the
    /// terminal's own scrollback. Restore (including the panic hook) releases it like everything else.
    pub fn open_with_mouse() -> Result<Self> {
        Self::open_inner(true)
    }

    fn open_inner(mouse: bool) -> Result<Self> {
        let previous_hook = install_panic_restore_hook();
        enable_raw_mode().context("enable raw terminal mode")?;

        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, cursor::Hide).context("enter alternate screen")?;
        if mouse {
            execute!(stdout, event::EnableMouseCapture).context("enable mouse capture")?;
        }

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;

        Ok(Self {
            terminal,
            previous_hook,
        })
    }

    pub fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> Result<()> {
        self.terminal.draw(render)?;
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        restore_terminal();
        restore_previous_hook(&self.previous_hook);
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    // DisableMouseCapture is a no-op for tools that never enabled it, and undoes the interactive
    // cdp session's capture even on a panic — so the wheel never strays as escape codes.
    let _ = execute!(stdout, event::DisableMouseCapture, cursor::Show, LeaveAlternateScreen);
}

fn install_panic_restore_hook() -> Arc<Mutex<Option<PanicHook>>> {
    let previous = Arc::new(Mutex::new(Some(panic::take_hook())));
    let hook_previous = Arc::clone(&previous);

    panic::set_hook(Box::new(move |info| {
        restore_terminal();
        if let Ok(previous) = hook_previous.lock() {
            if let Some(previous) = previous.as_ref() {
                previous(info);
            }
        }
    }));

    previous
}

fn restore_previous_hook(previous_hook: &Arc<Mutex<Option<PanicHook>>>) {
    let _replaced = panic::take_hook();
    if let Ok(mut previous) = previous_hook.lock() {
        if let Some(previous) = previous.take() {
            panic::set_hook(previous);
        }
    }
}
