use std::{
    io,
    panic::{self, PanicHookInfo},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use crossterm::{
    cursor, execute,
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
    pub fn open() -> Result<Self> {
        let previous_hook = install_panic_restore_hook();
        enable_raw_mode().context("enable raw terminal mode")?;

        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, cursor::Hide).context("enter alternate screen")?;

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
    let _ = execute!(stdout, cursor::Show, LeaveAlternateScreen);
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
