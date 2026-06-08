use anyhow::Result;
use async_trait::async_trait;
use clap::{ArgMatches, Command};

use super::Context;

/// A tool's identity — drives dispatch (`name`), the tool list, and help text.
pub struct ToolMeta {
    pub name: &'static str,
    pub about: &'static str,
    pub version: &'static str,
}

/// The plug-in contract. A tool is one value implementing this; registering it is one line.
///
/// A tool decides headless-vs-interactive *inside* [`Tool::run`] from [`Context::term`] — the
/// framework owns no `tui()` method, only the harness a tool calls when it chooses to go
/// interactive. The binary never learns a tool's flags: it asks for [`Tool::command`] and hands
/// back the parsed [`ArgMatches`].
#[async_trait]
pub trait Tool: Send + Sync {
    fn meta(&self) -> ToolMeta;

    /// The tool's clap subcommand, mounted under `kit <name>`. Build it from a derived
    /// `Args` struct via `Args::command().name(self.meta().name)` to keep derive ergonomics.
    fn command(&self) -> Command;

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()>;
}
