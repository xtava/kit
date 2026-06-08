use anyhow::Result;
use serde::Serialize;

/// How a tool renders its result: aligned text for humans, JSON for machines.
///
/// Set once from the global `--json` flag. Text rendering is each tool's own concern (tables,
/// trees); the framework only owns the JSON path and the color/tty signal text rendering reads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
}

pub struct Output {
    format: OutputFormat,
    color: bool,
}

impl Output {
    pub fn new(format: OutputFormat, stdout_tty: bool) -> Self {
        let color = stdout_tty && format == OutputFormat::Text;
        Self { format, color }
    }

    pub fn is_json(&self) -> bool {
        self.format == OutputFormat::Json
    }

    /// Whether text rendering should emit ANSI color (a tty in text mode).
    pub fn color(&self) -> bool {
        self.color
    }

    pub fn json<T: Serialize>(&self, value: &T) -> Result<()> {
        println!("{}", serde_json::to_string_pretty(value)?);
        Ok(())
    }
}
