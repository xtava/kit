//! `diff` — read-only Git working-tree comparison and interactive review.

mod git;
mod model;
mod tui;

use std::env;

use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser, ValueEnum};

use crate::framework::{Context, Tool, ToolMeta};

pub use git::load_repository;
pub use model::{ChangeGroup, ChangeKind, DiffDocument, DiffInput, SpecialState};

pub fn tool() -> DiffTool {
    DiffTool
}

pub struct DiffTool;

#[derive(Parser)]
#[command(
    name = "diff",
    about = "Review staged and unstaged Git changes",
    long_about = "Opens a read-only terminal viewer for the current repository's staged, unstaged, and untracked changes."
)]
struct DiffArgs {
    /// Initial projection. Auto uses split when the content pane is wide enough.
    #[arg(long, value_enum, default_value_t = ModeArg::Auto)]
    mode: ModeArg,

    /// Theme name (nord or terminal) or a custom theme TOML path.
    #[arg(long, value_name = "THEME", default_value = "nord")]
    theme: String,

    /// Keep terminal mouse reporting disabled; keyboard controls remain available.
    #[arg(long)]
    no_mouse: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ModeArg {
    Auto,
    Unified,
    Split,
}

#[async_trait]
impl Tool for DiffTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "diff",
            about: "Review staged and unstaged Git changes",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        DiffArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = DiffArgs::from_arg_matches(matches)?;
        if !cx.term.interactive() {
            bail!("kit diff requires an interactive terminal");
        }
        let cwd = env::current_dir().context("resolve current directory")?;
        let documents = load_repository(&cwd)?;
        let (_, theme) = crate::tui::theme::resolve(&args.theme)
            .with_context(|| format!("load diff theme {:?}", args.theme))?;
        let mode = match args.mode {
            ModeArg::Auto => tui::ViewMode::Auto,
            ModeArg::Unified => tui::ViewMode::Unified,
            ModeArg::Split => tui::ViewMode::Split,
        };
        tui::run(documents, theme, !args.no_mouse, mode).await
    }
}
