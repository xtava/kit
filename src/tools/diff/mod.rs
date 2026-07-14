//! `diff` — Git working-tree comparison and index review.

mod config;
mod git;
mod model;
mod tui;

use std::env;

use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser, ValueEnum};

use crate::framework::{Context, SettingsSection, Tool, ToolMeta};
use config::Config;

pub use git::load_repository;
pub use model::{ChangeGroup, ChangeKind, DiffContext, DiffDocument, DiffInput, SpecialState};

pub fn tool() -> DiffTool {
    DiffTool
}

pub struct DiffTool;

#[derive(Parser)]
#[command(
    name = "diff",
    about = "Review staged and unstaged Git changes",
    long_about = "Opens a terminal viewer for the current repository's staged, unstaged, and untracked changes, with selected-file staging controls."
)]
struct DiffArgs {
    /// Initial projection. Auto uses split when the content pane is wide enough.
    #[arg(long, value_enum, default_value_t = ModeArg::Auto)]
    mode: ModeArg,

    /// Theme name (nord or terminal) or a custom theme TOML path.
    #[arg(long, value_name = "THEME", default_value = "nord")]
    theme: String,

    /// Unchanged lines around each change, or "all" for the complete file.
    #[arg(long, value_name = "LINES|all", default_value = "3")]
    context: DiffContext,

    /// Keep terminal mouse reporting disabled; keyboard controls remain available.
    #[arg(long)]
    no_mouse: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ModeArg {
    Auto,
    Inline,
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

    fn settings(&self) -> Option<SettingsSection> {
        Some(config::settings())
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = DiffArgs::from_arg_matches(matches)?;
        if !cx.term.interactive() {
            bail!("kit diff requires an interactive terminal");
        }
        let cwd = env::current_dir().context("resolve current directory")?;
        let config = Config::load(cx.config.clone())?;
        let documents = load_repository(&cwd, args.context)?;
        let (_, theme) = crate::tui::theme::resolve(&args.theme)
            .with_context(|| format!("load diff theme {:?}", args.theme))?;
        let mode = match args.mode {
            ModeArg::Auto => tui::ViewMode::Auto,
            ModeArg::Inline => tui::ViewMode::Inline,
            ModeArg::Split => tui::ViewMode::Split,
        };
        tui::run(cwd, documents, theme, !args.no_mouse, mode, args.context, config.line_numbers())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_accepts_view_modes_and_context_policies() {
        let default = DiffArgs::try_parse_from(["diff"]).unwrap();
        let inline = DiffArgs::try_parse_from(["diff", "--mode", "inline"]).unwrap();
        let split = DiffArgs::try_parse_from(["diff", "--mode", "split"]).unwrap();
        let zero = DiffArgs::try_parse_from(["diff", "--context", "0"]).unwrap();
        let all = DiffArgs::try_parse_from(["diff", "--context", "all"]).unwrap();

        assert!(matches!(default.mode, ModeArg::Auto));
        assert!(matches!(inline.mode, ModeArg::Inline));
        assert!(matches!(split.mode, ModeArg::Split));
        assert_eq!(default.context, DiffContext::Lines(3));
        assert_eq!(zero.context, DiffContext::Lines(0));
        assert_eq!(all.context, DiffContext::All);
        assert!(DiffArgs::try_parse_from(["diff", "--context", "-1"]).is_err());
    }
}
