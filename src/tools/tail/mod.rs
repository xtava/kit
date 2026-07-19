//! tail — a local TUI over the official Tailscale CLI and Taildrop.

mod cache;
mod client;
mod config;
mod contributions;
mod file_input;
mod model;
mod tui;

use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command};

use crate::framework::{Context, SettingsSection, Tool, ToolMeta};

pub fn tool() -> TailTool {
    TailTool
}

pub struct TailTool;

#[async_trait]
impl Tool for TailTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "tail",
            about: "Share text and files across your Tailscale devices",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        Command::new("tail").about("Share text and files across your Tailscale devices")
    }

    fn settings(&self) -> Option<SettingsSection> {
        Some(config::settings())
    }

    async fn run(&self, cx: &Context, _matches: &ArgMatches) -> Result<()> {
        if !cx.term.interactive() {
            bail!("kit tail requires an interactive terminal");
        }
        if cx.out.is_json() {
            bail!("kit tail is an interactive TUI and does not emit JSON");
        }
        let working_directory = std::env::current_dir().context("resolve current directory")?;
        let client = client::TailClient::new(cx.processes.clone(), working_directory);
        let readiness = client.readiness().await?;
        let cache = cache::ReceiveCache::discover()?;
        let config = config::Config::load(cx.config.clone())?;
        tui::run(client, cache, readiness, config).await
    }
}
