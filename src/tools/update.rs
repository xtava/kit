//! `update` — safely advance the registered Kit source and replace the managed binary.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::{
    framework::{Context, Tool, ToolMeta},
    update::{build_identity, SourceDisposition, SourceUpdater, UpdateReceipt},
};

pub fn tool() -> UpdateTool {
    UpdateTool
}

pub struct UpdateTool;

#[derive(Parser)]
#[command(
    name = "update",
    about = "Pull, install, and activate the latest Kit source",
    long_about = "Fetches the registered canonical Kit upstream, performs only a safe fast-forward, runs the checkout's canonical installer, verifies the replacement binary, and safely restarts Console."
)]
struct UpdateArgs {
    #[command(subcommand)]
    command: Option<UpdateCommand>,
}

#[derive(Subcommand)]
enum UpdateCommand {
    /// Record the canonical source checkout used by install.sh.
    #[command(name = "__register-source", hide = true)]
    RegisterSource { checkout: PathBuf },
    /// Report the source identity embedded in this binary.
    #[command(name = "__build-identity", hide = true)]
    BuildIdentity,
}

#[async_trait]
impl Tool for UpdateTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "update",
            about: "Pull, install, and activate the latest Kit source",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        UpdateArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = UpdateArgs::from_arg_matches(matches)?;
        let updater = SourceUpdater::new(cx.processes.clone())?;
        match args.command {
            None => {
                let receipt =
                    updater.install_managed(cx.term.stdout_tty && !cx.out.is_json()).await?;
                if cx.out.is_json() {
                    cx.out.json(&receipt)
                } else {
                    print_receipt(&receipt);
                    Ok(())
                }
            }
            Some(UpdateCommand::RegisterSource { checkout }) => {
                let receipt = updater.register_source(&checkout).await?;
                if cx.out.is_json() {
                    cx.out.json(&receipt)
                } else {
                    println!("Registered Kit source at {}", receipt.checkout.display());
                    Ok(())
                }
            }
            Some(UpdateCommand::BuildIdentity) => cx.out.json(&build_identity()),
        }
    }
}

fn print_receipt(receipt: &UpdateReceipt) {
    match receipt.source_disposition {
        SourceDisposition::FastForwarded => println!(
            "Kit updated {} → {} and replaced {}",
            short_revision(&receipt.before_revision),
            short_revision(&receipt.installed_revision),
            receipt.installed_executable.display()
        ),
        SourceDisposition::Current => println!(
            "Kit source is current at {}; reinstalled {}",
            short_revision(&receipt.installed_revision),
            receipt.installed_executable.display()
        ),
        SourceDisposition::LocalAhead => println!(
            "Kit source is locally ahead at {}; reinstalled {} without rewriting history",
            short_revision(&receipt.installed_revision),
            receipt.installed_executable.display()
        ),
    }
    let console_state = receipt
        .console_status
        .get("state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    println!("Console: {console_state}");
}

fn short_revision(revision: &str) -> &str {
    &revision[..revision.len().min(8)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_revision_is_safe_for_unknown_or_full_identities() {
        assert_eq!(short_revision("unknown"), "unknown");
        assert_eq!(short_revision("0123456789abcdef"), "01234567");
    }

    #[test]
    fn hidden_protocol_commands_stay_out_of_help() {
        let help = UpdateArgs::command().render_long_help().to_string();
        assert!(!help.contains("__register-source"));
        assert!(!help.contains("__build-identity"));
    }
}
