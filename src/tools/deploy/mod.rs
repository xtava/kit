//! deploy — interactive, config-driven deployment launcher with version history and rollback.

mod annotations;
mod cloudflare;
mod config;
mod environment;
mod journal;
mod layout;
mod runner;
mod state;
mod tui;

use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser};

use crate::framework::{Context, Tool, ToolMeta};
use annotations::AnnotationStore;
use config::LoadedPlan;
use journal::JournalStore;
use layout::{DeployLayout, LayoutStore};
use runner::RunOutcome;

pub fn tool() -> DeployTool {
    DeployTool
}

pub struct DeployTool;

#[derive(Parser)]
#[command(
    name = "deploy",
    about = "Interactive deployment launcher with history and rollback",
    long_about = "Browse config-defined deployment Targets, run their ordered Steps with streamed output, inspect version history, and roll back to a recorded Version."
)]
struct DeployArgs {
    /// Load this deployment plan instead of project-local or XDG configuration.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[async_trait]
impl Tool for DeployTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "deploy",
            about: "Interactive deployment launcher with history and rollback",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        DeployArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = DeployArgs::from_arg_matches(matches)?;
        if cx.out.is_json() {
            bail!("kit deploy is interactive and does not support --json");
        }
        if !cx.term.interactive() {
            bail!("kit deploy requires an interactive terminal (stdin and stdout must be TTYs)");
        }

        let project_dir = std::env::current_dir().context("resolve current directory")?;
        let loaded = LoadedPlan::load(args.config, project_dir, cx.config.path("deploy"))?;
        let journal_store = JournalStore::bootstrap()?;
        let journal = journal_store.load()?;
        let annotation_store = AnnotationStore::bootstrap()?;
        let annotations = annotation_store.load()?;
        let layout_store = LayoutStore::bootstrap()?;
        let (layout, layout_warning) = match layout_store.load() {
            Ok(layout) => (layout, None),
            Err(error) => (
                DeployLayout::default(),
                Some(format!(
                    "Could not load saved panel layout: {error}. Using defaults; press = to save a valid layout."
                )),
            ),
        };

        match tui::run(tui::Startup {
            loaded,
            journal_store,
            journal,
            annotation_store,
            annotations,
            layout_store,
            layout,
            layout_warning,
        })
        .await?
        {
            None | Some(RunOutcome::Succeeded) => Ok(()),
            Some(RunOutcome::Failed) => bail!("deploy failed"),
            Some(RunOutcome::Cancelled) => bail!("deploy cancelled"),
        }
    }
}
