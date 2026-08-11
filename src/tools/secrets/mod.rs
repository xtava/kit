//! secrets — local 1Password TUI backed exclusively by the official `op` CLI.
//!
//! 1Password owns encryption, synchronization, authorization, and vault data. This leaf tool owns
//! only process orchestration, ephemeral interaction state, and presentation.

mod actions;
mod model;
pub(crate) mod op;
mod sensitive;
mod tui;

use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser};

use crate::framework::{Context, Tool, ToolMeta};

pub fn tool() -> SecretsTool {
    SecretsTool
}

pub struct SecretsTool;

#[derive(Parser)]
#[command(
    name = "secrets",
    about = "Browse and manage 1Password from a local TUI",
    long_about = "A local terminal client for 1Password. The official op CLI remains the vault and authentication owner; Kit persists no secrets or metadata."
)]
struct SecretsArgs {
    /// Select a 1Password account by ID, sign-in address, or shorthand.
    #[arg(long, value_name = "ACCOUNT")]
    account: Option<String>,
}

#[async_trait]
impl Tool for SecretsTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "secrets",
            about: "Browse and manage 1Password from a local TUI",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        SecretsArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = SecretsArgs::from_arg_matches(matches)?;
        if !cx.term.interactive() {
            bail!("kit secrets requires an interactive terminal");
        }
        if cx.out.is_json() {
            bail!("kit secrets does not expose secrets through JSON output");
        }

        let client = op::OpClient::new();
        client.version().await.context("1Password CLI preflight failed")?;
        let accounts = client.accounts().await.context("discover 1Password accounts")?;
        if accounts.is_empty() {
            bail!(
                "no 1Password accounts are available to the CLI; enable Settings > Developer > Integrate with 1Password CLI"
            );
        }

        tui::run(client, accounts, args.account).await
    }
}
