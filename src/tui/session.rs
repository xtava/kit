use std::{
    io::{self, Write},
    panic::{self, PanicHookInfo},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use crossterm::{
    cursor,
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
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

#[derive(Clone, Copy, Debug, Default)]
pub struct SessionOptions {
    pub mouse_capture: bool,
    pub bracketed_paste: bool,
}

impl Session {
    pub fn open(options: SessionOptions) -> Result<Self> {
        let previous_hook = install_panic_restore_hook();
        let terminal = (|| {
            enable_raw_mode().context("enable raw terminal mode")?;

            let mut stdout = io::stdout();
            execute!(stdout, EnterAlternateScreen, cursor::Hide)
                .context("enter alternate screen")?;
            if options.mouse_capture {
                execute!(stdout, EnableMouseCapture).context("enable mouse capture")?;
            }
            if options.bracketed_paste {
                execute!(stdout, EnableBracketedPaste).context("enable bracketed paste")?;
            }

            let backend = CrosstermBackend::new(stdout);
            let mut terminal = Terminal::new(backend)?;
            terminal.clear()?;
            Ok::<_, anyhow::Error>(terminal)
        })();
        let terminal = match terminal {
            Ok(terminal) => terminal,
            Err(error) => {
                restore_terminal();
                restore_previous_hook(&previous_hook);
                return Err(error);
            }
        };

        Ok(Self { terminal, previous_hook })
    }

    pub fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> Result<()> {
        self.terminal.draw(render)?;
        Ok(())
    }

    /// Copy `text` to the system clipboard. The OSC 52 escape goes through the *same* backend ratatui
    /// draws on (which is itself a `Write`), so it's ordered with frames and never races a redraw.
    pub fn copy(&mut self, text: &str) -> Result<()> {
        let backend = self.terminal.backend_mut();
        let escape = super::clipboard::osc52(text);
        backend.write_all(escape.as_bytes()).context("write clipboard escape")?;
        backend.flush().context("flush clipboard escape")?;
        drop(escape);
        // The escape bytes desync ratatui's diff buffer; force the next draw to repaint in full.
        self.terminal.clear().context("repaint after clipboard write")
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
    let _ = execute!(
        stdout,
        DisableBracketedPaste,
        DisableMouseCapture,
        cursor::Show,
        LeaveAlternateScreen
    );
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
