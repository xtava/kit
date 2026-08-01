//! deploy — interactive, config-driven deployment launcher with version history and rollback.

mod annotations;
mod artifact;
mod cloudflare;
mod config;
mod headless;
mod journal;
mod layout;
mod orchestration;
mod runner;
mod source;
mod state;
mod tui;

use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Args, Command, CommandFactory, FromArgMatches, Parser, Subcommand};

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
    about = "Deployment launcher with history and rollback",
    long_about = "Browse deployment Targets interactively, or run one exact production Target headlessly with the same configuration, secret injection, process supervision, and Journal."
)]
struct DeployArgs {
    /// Load this deployment plan instead of project-local or XDG configuration.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<DeployCommand>,
}

#[derive(Subcommand)]
enum DeployCommand {
    /// Run one exact production Target without opening the TUI.
    Run(DeployRunArgs),
}

#[derive(Args)]
struct DeployRunArgs {
    /// Exact Target id from the loaded deployment plan.
    #[arg(long, value_name = "ID")]
    target: String,

    /// Confirm that this command may mutate the Target's production destination.
    #[arg(long)]
    confirm_production: bool,
}

#[async_trait]
impl Tool for DeployTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "deploy",
            about: "Deployment launcher with history and rollback",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        DeployArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let DeployArgs { config, command } = DeployArgs::from_arg_matches(matches)?;
        let project_dir = std::env::current_dir().context("resolve current directory")?;
        let loaded = LoadedPlan::load(config, project_dir, cx.config.path("deploy"))?;
        if let Some(DeployCommand::Run(args)) = command {
            if !args.confirm_production {
                bail!(
                    "headless production deploy requires --confirm-production for Target '{}'",
                    args.target
                );
            }
            return match headless::run(cx, loaded, &args.target).await? {
                RunOutcome::Succeeded => Ok(()),
                RunOutcome::Failed => bail!("deploy failed"),
                RunOutcome::Cancelled => bail!("deploy cancelled"),
            };
        }
        if cx.out.is_json() {
            bail!("kit deploy --json requires a headless subcommand such as 'run'");
        }
        if !cx.term.interactive() {
            bail!("kit deploy requires an interactive terminal (stdin and stdout must be TTYs)");
        }

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
            processes: cx.processes.clone(),
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
