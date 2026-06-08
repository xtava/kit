//! scout — live memory recon for Electron app fleets.
//!
//! `top` lies about Electron (RSS over-counts shared pages ~3×); scout reports honest **PSS** by
//! process role, grouped per instance, and (when an instance exposes a debug port) ties renderers
//! to the windows/webviews they render via CDP. Headless table or `--json`; a live TUI otherwise.
//! `scout dive` hands a window to memlab for deep heap forensics.

mod cdp;
mod dive;
mod format;
mod model;
mod proc;
mod report;
mod survey;
mod system;
mod tui;

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::framework::{Context, Tool, ToolMeta};

pub fn tool() -> ScoutTool {
    ScoutTool
}

pub struct ScoutTool;

#[derive(Parser)]
#[command(
    name = "scout",
    about = "Live memory recon for Electron app fleets",
    long_about = "Surveys every Electron instance on the machine: honest PSS by process role, grouped per instance, with CDP target attribution where a debug port is exposed."
)]
struct ScoutArgs {
    #[command(subcommand)]
    command: Option<ScoutCommand>,

    /// Scope the survey to processes whose cmdline contains this marker.
    #[arg(long, default_value = "electron", global = true)]
    app: String,

    /// Take one survey and print it, instead of opening the live TUI.
    #[arg(long, global = true)]
    once: bool,
}

#[derive(Subcommand)]
enum ScoutCommand {
    /// Capture a window's heap snapshot and hand it to memlab for deep forensics.
    Dive(DiveArgs),
}

#[derive(Parser)]
pub struct DiveArgs {
    /// CDP port to dive. Defaults to the heaviest instance that exposes one.
    #[arg(long)]
    pub port: Option<u16>,

    /// Where to write the `.heapsnapshot`. Defaults to a temp file.
    #[arg(long)]
    pub out: Option<PathBuf>,

    /// The Playwright module to require — a name on `NODE_PATH`, or an absolute path.
    #[arg(long, default_value = "playwright")]
    pub playwright: String,
}

#[async_trait]
impl Tool for ScoutTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "scout",
            about: "Live memory recon for Electron app fleets",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        ScoutArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = ScoutArgs::from_arg_matches(matches)?;

        if let Some(ScoutCommand::Dive(dive_args)) = &args.command {
            return dive::run(&args.app, dive_args).await;
        }

        if !args.once && !cx.out.is_json() && cx.term.interactive() {
            return tui::run(args.app).await;
        }

        let survey = survey::collect(&args.app).await;
        if cx.out.is_json() {
            cx.out.json(&survey)?;
        } else {
            report::print_table(&survey);
        }
        Ok(())
    }
}
