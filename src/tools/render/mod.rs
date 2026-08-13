//! `render` — an interactive source and Markdown viewer with a fuzzy workspace file switcher.

mod actions;
mod config;
mod search;
mod tui;

use std::{env, path::PathBuf};

use anyhow::{bail, Context as AnyhowContext, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser};

use crate::framework::{Context, SettingsSection, Tool, ToolMeta};
use config::Config;

pub fn tool() -> RenderTool {
    RenderTool
}

pub struct RenderTool;

#[derive(Parser)]
#[command(
    name = "render",
    about = "Read source and Markdown files in an interactive terminal viewer",
    long_about = "Renders source files with syntax highlighting and Markdown files with rich formatting in a scrollable terminal viewer. The bottom prompt fuzzy-searches supported files under the current directory, visibly labels Git-ignored results, and supports /configure discovery settings."
)]
struct RenderArgs {
    /// Source or Markdown file to open. Omit it to start with the fuzzy workspace picker.
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    /// Theme name (nord or terminal) or a custom theme TOML path.
    #[arg(long, value_name = "THEME")]
    theme: Option<String>,
}

#[async_trait]
impl Tool for RenderTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "render",
            about: "Read source and Markdown files in an interactive terminal viewer",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        RenderArgs::command()
    }

    fn settings(&self) -> Option<SettingsSection> {
        Some(config::settings())
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = RenderArgs::from_arg_matches(matches)?;
        if !cx.term.interactive() {
            bail!("kit render requires an interactive terminal");
        }

        let root = env::current_dir().context("resolve current directory")?;
        let config = Config::load(cx.config.clone())?;
        let requested_theme = args.theme.as_deref().unwrap_or(config.theme());
        let (theme_spec, theme) = crate::tui::theme::resolve(requested_theme)
            .with_context(|| format!("load render theme {requested_theme:?}"))?;
        tui::run(root, args.file, config, theme_spec, theme).await
    }
}
