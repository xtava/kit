use anyhow::{bail, Context as AnyhowContext, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde::Serialize;

use crate::framework::process::{DetachedLaunchRecovery, PendingDetachedLaunchPhase};
use crate::framework::{Context, Tool, ToolMeta};

pub fn tool() -> ProcessTool {
    ProcessTool
}

pub struct ProcessTool;

#[derive(Parser)]
#[command(name = "process", about = "Inspect and recover Kit-owned detached process launches")]
struct ProcessArgs {
    #[command(subcommand)]
    command: ProcessCommand,
}

#[derive(Subcommand)]
enum ProcessCommand {
    /// List uncommitted detached launches whose exact authority can be recovered.
    Pending,
    /// Terminate and release an uncommitted detached launch.
    RecoverDetached {
        /// Opaque recovery capability printed by a failed detached launch.
        #[arg(long, conflicts_with = "pending")]
        token: Option<String>,
        /// Recover every currently unlocked pending launch discovered in Kit's private state.
        #[arg(long, conflicts_with = "token")]
        pending: bool,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PendingOutput {
    run_id: String,
    phase: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RecoveredOutput {
    recovered_run_ids: Vec<String>,
}

#[async_trait]
impl Tool for ProcessTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "process",
            about: "Inspect and recover Kit-owned detached process launches",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        ProcessArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = ProcessArgs::from_arg_matches(matches)?;
        match args.command {
            ProcessCommand::Pending => {
                let pending = cx
                    .processes
                    .list_pending_detached_launches()
                    .context("list pending detached launches")?
                    .into_iter()
                    .map(|launch| PendingOutput {
                        run_id: launch.run_id().to_string(),
                        phase: match launch.phase() {
                            PendingDetachedLaunchPhase::Prepared => "prepared",
                            PendingDetachedLaunchPhase::AuthorityBound => "authorityBound",
                        },
                    })
                    .collect::<Vec<_>>();
                if cx.out.is_json() {
                    cx.out.json(&pending)?;
                } else if pending.is_empty() {
                    println!("no pending detached launches");
                } else {
                    for launch in pending {
                        println!("{} {}", launch.run_id, launch.phase);
                    }
                }
            }
            ProcessCommand::RecoverDetached { token, pending } => {
                let recovered = match (token, pending) {
                    (Some(token), false) => {
                        let recovery = DetachedLaunchRecovery::decode(&token)
                            .context("decode detached launch recovery token")?;
                        let run_id = recovery.run_id();
                        cx.processes
                            .recover_detached_launch(&recovery)
                            .await
                            .with_context(|| format!("recover detached launch {run_id}"))?;
                        vec![run_id]
                    }
                    (None, true) => cx
                        .processes
                        .recover_pending_detached_launches()
                        .await
                        .context("recover pending detached launches")?,
                    _ => bail!("pass exactly one of --token <TOKEN> or --pending"),
                };
                let output = RecoveredOutput {
                    recovered_run_ids: recovered.iter().map(ToString::to_string).collect(),
                };
                if cx.out.is_json() {
                    cx.out.json(&output)?;
                } else if recovered.is_empty() {
                    println!("no pending detached launches recovered");
                } else {
                    for run_id in recovered {
                        println!("recovered {run_id}");
                    }
                }
            }
        }
        Ok(())
    }
}
