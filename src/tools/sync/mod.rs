//! sync — bidirectional source synchronization across Kit machines.

mod config;
mod contributions;
mod controller;
mod engine;
mod model;
mod remote;
mod tui;

use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde::Serialize;

use crate::framework::{Context, Tool, ToolMeta};

use controller::{AddRequest, DoctorReport, ProjectReport, SyncController};
use model::SyncedProject;

pub fn tool() -> SyncTool {
    SyncTool
}

pub struct SyncTool;

#[derive(Parser)]
#[command(name = "sync", about = "Keep source code aligned across two Kit machines")]
struct SyncArgs {
    #[command(subcommand)]
    command: Option<SyncCommand>,
}

#[derive(Subcommand)]
enum SyncCommand {
    /// Create a Synced Project.
    Add {
        /// Stable local name for the project.
        name: String,
        /// Exact Tailscale peer selector.
        machine: String,
        /// Absolute project path on the remote machine.
        remote_root: PathBuf,
        /// Unix user used for the remote OpenSSH endpoint.
        #[arg(long)]
        user: String,
        /// Local project root. Defaults to the current directory.
        #[arg(long)]
        local_root: Option<PathBuf>,
        /// Add an engine ignore pattern.
        #[arg(long = "exclude")]
        excludes: Vec<String>,
        /// Re-include a path excluded by an earlier pattern.
        #[arg(long = "include")]
        includes: Vec<String>,
    },
    /// Report configured projects and live synchronization state.
    Status {
        /// Project name or UUID. Omit to report every project.
        project: Option<String>,
        /// Continuously project live engine state until interrupted.
        #[arg(long)]
        watch: bool,
    },
    /// Pause synchronization without removing project intent.
    Pause {
        /// Project name or UUID.
        project: String,
    },
    /// Resume a paused project.
    Resume {
        /// Project name or UUID.
        project: String,
    },
    /// Flush pending changes through the synchronization engine.
    Flush {
        /// Project name or UUID.
        project: String,
    },
    /// Remove project intent and its exact engine session, preserving user files.
    Remove {
        /// Project name or UUID.
        project: String,
    },
    /// Diagnose engine, Tailscale, peer, and session readiness.
    Doctor {
        /// Project name or UUID. Omit for global setup diagnostics.
        project: Option<String>,
    },
}

#[async_trait]
impl Tool for SyncTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "sync",
            about: "Keep source code aligned across two Kit machines",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        SyncArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = SyncArgs::from_arg_matches(matches)?;
        let working_directory = std::env::current_dir().context("resolve current directory")?;
        let controller =
            SyncController::new(cx.processes.clone(), cx.config.clone(), working_directory)?;

        match args.command {
            Some(SyncCommand::Add {
                name,
                machine,
                remote_root,
                user,
                local_root,
                excludes,
                includes,
            }) => {
                let report = controller
                    .add(AddRequest {
                        name,
                        machine,
                        remote_root,
                        user,
                        local_root,
                        excludes,
                        includes,
                    })
                    .await?;
                print_reports(cx, vec![report])?;
            }
            Some(SyncCommand::Status { project, watch }) => {
                if watch {
                    if cx.out.is_json() {
                        bail!("`kit sync status --watch` does not support `--json`");
                    }
                    let project =
                        project.context("`kit sync status --watch` requires a project selector")?;
                    watch_project(cx, &controller, &project).await?;
                } else {
                    print_reports(cx, controller.status(project.as_deref()).await?)?;
                }
            }
            Some(SyncCommand::Pause { project }) => {
                print_reports(cx, vec![controller.pause(&project).await?])?;
            }
            Some(SyncCommand::Resume { project }) => {
                print_reports(cx, vec![controller.resume(&project).await?])?;
            }
            Some(SyncCommand::Flush { project }) => {
                print_reports(cx, vec![controller.flush(&project).await?])?;
            }
            Some(SyncCommand::Remove { project }) => {
                print_removed(cx, &controller.remove(&project).await?)?;
            }
            Some(SyncCommand::Doctor { project }) => {
                print_doctor(cx, controller.doctor(project.as_deref()).await?)?;
            }
            None => {
                if cx.out.is_json() {
                    print_reports(cx, controller.status(None).await?)?;
                } else {
                    tui::run(controller).await?;
                }
            }
        }
        Ok(())
    }
}

async fn watch_project(cx: &Context, controller: &SyncController, selector: &str) -> Result<()> {
    let mut monitor = controller.monitor(selector).await?;
    loop {
        tokio::select! {
            report = monitor.next() => {
                let Some(report) = report? else { return Ok(()) };
                print_reports(cx, vec![report])?;
            }
            signal = tokio::signal::ctrl_c() => {
                signal.context("listen for sync monitor interruption")?;
                break;
            }
        }
    }
    monitor.stop().await?;
    Ok(())
}

fn print_reports(cx: &Context, reports: Vec<ProjectReport>) -> Result<()> {
    if cx.out.is_json() {
        return cx.out.json(&reports);
    }
    if reports.is_empty() {
        println!("No Synced Projects.");
        return Ok(());
    }
    for report in reports {
        let state = report.state.label();
        println!(
            "{:<20} {:<12} {}",
            report.project.name(),
            state,
            report.project.remote().root().display()
        );
    }
    Ok(())
}

fn print_removed(cx: &Context, project: &SyncedProject) -> Result<()> {
    #[derive(Serialize)]
    struct Removed<'a> {
        removed: &'a SyncedProject,
        files_preserved: bool,
    }
    if cx.out.is_json() {
        cx.out.json(&Removed { removed: project, files_preserved: true })
    } else {
        println!("Removed Synced Project {:?}; synchronized files were preserved.", project.name());
        Ok(())
    }
}

fn print_doctor(cx: &Context, report: DoctorReport) -> Result<()> {
    if cx.out.is_json() {
        return cx.out.json(&report);
    }
    println!("mutagen   {}", report.mutagen.detail);
    println!("tailscale {}", report.tailscale.detail);
    if let Some(remote) = report.remote {
        println!("remote    {}", remote.detail);
    }
    if let Some(project) = report.project {
        println!("project   {}", project.detail);
    }
    if let Some(next_action) = report.next_action {
        println!("next      {next_action}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_surface_has_one_canonical_lifecycle_vocabulary() {
        assert!(matches!(
            SyncArgs::try_parse_from([
                "sync",
                "add",
                "kit",
                "remote-node",
                "/workspace/project",
                "--user",
                "remote-user"
            ])
            .unwrap()
            .command,
            Some(SyncCommand::Add { name, machine, .. })
                if name == "kit" && machine == "remote-node"
        ));
        assert!(matches!(
            SyncArgs::try_parse_from(["sync", "status", "kit"]).unwrap().command,
            Some(SyncCommand::Status { project: Some(project), watch: false }) if project == "kit"
        ));
    }
}
