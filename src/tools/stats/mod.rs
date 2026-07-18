//! stats — an interactive, process-aware system monitor.

mod actions;
mod app;
mod contributions;
mod history;
mod host;
mod model;
mod render;
mod report;
mod sampler;
mod tree;
mod tui;

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser};

use crate::framework::{Context, Tool, ToolMeta};

use sampler::Sampler;

pub fn tool() -> StatsTool {
    StatsTool
}

pub struct StatsTool;

#[derive(Parser)]
#[command(name = "stats", about = "Interactive CPU and process monitor")]
struct StatsArgs {
    /// Take one warmed-up snapshot and print it instead of opening the TUI.
    #[arg(long)]
    once: bool,

    /// Sampling interval in milliseconds.
    #[arg(long, default_value_t = 2_000, value_parser = clap::value_parser!(u64).range(250..))]
    interval: u64,

    /// Keep terminal mouse reporting disabled; keyboard controls remain available.
    #[arg(long)]
    no_mouse: bool,
}

#[async_trait]
impl Tool for StatsTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "stats",
            about: "Interactive CPU and process monitor",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        StatsArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = StatsArgs::from_arg_matches(matches)?;
        let interval = Duration::from_millis(args.interval);
        if !args.once && !cx.out.is_json() && cx.term.interactive() {
            return tui::run(interval, !args.no_mouse).await;
        }

        let mut sampler = Sampler::new(interval)?;
        let _ = sampler.sample_overview()?;
        tokio::time::sleep(interval.max(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL)).await;
        let snapshot = sampler.sample_overview()?;
        if cx.out.is_json() {
            cx.out.json(&snapshot)?;
        } else {
            report::print(&snapshot);
        }
        Ok(())
    }
}
