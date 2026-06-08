use std::io::IsTerminal;

/// Where stdin/stdout point — decides whether a tool opens its TUI or runs headless.
pub struct Terminal {
    pub stdin_tty: bool,
    pub stdout_tty: bool,
}

impl Terminal {
    pub fn detect() -> Self {
        Self {
            stdin_tty: std::io::stdin().is_terminal(),
            stdout_tty: std::io::stdout().is_terminal(),
        }
    }

    /// Both ends are a terminal — the precondition for an interactive TUI.
    pub fn interactive(&self) -> bool {
        self.stdin_tty && self.stdout_tty
    }
}
