//! stream — a Linux-first control plane for Sunshine, Moonlight, and Hyprland.

mod command;
mod config;
mod controller;
mod linux;
mod model;

use std::fmt::Write as _;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::framework::{Context, SettingsSection, Tool, ToolMeta};

use controller::StreamController;
use model::{
    DiagnosticSeverity, DoctorCheckStatus, StreamDoctorReport, StreamInspection, StreamStatusReport,
};

pub fn tool() -> StreamTool {
    StreamTool
}

pub struct StreamTool;

#[derive(Parser)]
#[command(name = "stream", about = "Stream a Linux workspace or window through Sunshine")]
struct StreamArgs {
    #[command(subcommand)]
    command: Option<StreamCommand>,
}

#[derive(Subcommand)]
enum StreamCommand {
    /// Inspect Stream hosts, sources, and dependencies without changing state.
    Inspect {
        /// Host selector. CLI overrides Stream config; omit for configured or local host.
        host: Option<String>,
    },
    /// Report Stream readiness and active owned session state.
    Status {
        /// Host selector. CLI overrides Stream config; omit for configured or local host.
        host: Option<String>,
    },
    /// Diagnose setup, compatibility, and ownership conflicts.
    Doctor {
        /// Host selector. CLI overrides Stream config; omit for configured or local host.
        host: Option<String>,
    },
    /// Configure Stream authentication and remote host identity.
    Setup {
        #[command(subcommand)]
        command: StreamSetupCommand,
    },
    #[command(name = "__host-inspect", hide = true)]
    HostInspect,
}

#[derive(Subcommand)]
enum StreamSetupCommand {
    /// Authenticate the local Tailscale client.
    Tailscale,
    /// Store an SSH user against one exact stable Tailscale node.
    Host {
        /// Exact Tailscale peer selector.
        host: String,
        /// Unix user for OpenSSH on the selected host.
        #[arg(long)]
        user: String,
        /// Make this node the default Stream host.
        #[arg(long)]
        preferred: bool,
    },
}

#[async_trait]
impl Tool for StreamTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "stream",
            about: "Stream a Linux workspace or window through Sunshine",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn settings(&self) -> Option<SettingsSection> {
        Some(config::settings())
    }

    fn command(&self) -> Command {
        StreamArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let arguments = StreamArgs::from_arg_matches(matches)?;
        let working_directory = std::env::current_dir().context("resolve current directory")?;
        let config = config::Config::load(cx.config.clone())?;
        let mut controller = StreamController::new(cx.processes.clone(), working_directory, config);
        match arguments.command {
            Some(StreamCommand::Inspect { host }) => {
                print_inspection(cx, &controller.inspect(host.as_deref()).await?)
            }
            Some(StreamCommand::Status { host }) => {
                print_status(cx, &controller.status(host.as_deref()).await?)
            }
            Some(StreamCommand::Doctor { host }) => {
                print_doctor(cx, &controller.doctor(host.as_deref()).await?)
            }
            Some(StreamCommand::Setup { command }) => {
                let report = match command {
                    StreamSetupCommand::Tailscale => controller.authenticate_tailscale().await?,
                    StreamSetupCommand::Host { host, user, preferred } => {
                        controller.configure_host(&host, &user, preferred).await?
                    }
                };
                if cx.out.is_json() {
                    cx.out.json(&report)
                } else {
                    println!("{} · {:?}", report.action, report.state);
                    Ok(())
                }
            }
            Some(StreamCommand::HostInspect) => {
                let inspection = controller.inspect(Some("local")).await?;
                cx.out.json(&inspection)
            }
            None => print_inspection(cx, &controller.inspect(None).await?),
        }
    }
}

fn print_inspection(cx: &Context, inspection: &StreamInspection) -> Result<()> {
    if cx.out.is_json() {
        return cx.out.json(inspection);
    }
    let mut output = String::new();
    writeln!(
        output,
        "{} · {:?} · {:?}",
        inspection.target.display_name, inspection.target.kind, inspection.readiness
    )?;
    if let Some(hyprland) = &inspection.hyprland {
        writeln!(
            output,
            "Hyprland  {} outputs · {} workspaces · {} windows",
            hyprland.outputs.len(),
            hyprland.workspaces.len(),
            hyprland.windows.len()
        )?;
        for display in &hyprland.outputs {
            let ownership = if display.managed_by_kit { "Kit" } else { "existing" };
            writeln!(
                output,
                "  {:<20} {:>4}x{:<4} @ {:>6.2} Hz · {}",
                display.name, display.width, display.height, display.refresh_hz, ownership
            )?;
        }
    }
    writeln!(
        output,
        "Sunshine  {}{}",
        availability(inspection.sunshine.available),
        version_suffix(inspection.sunshine.version.as_deref())
    )?;
    writeln!(
        output,
        "Moonlight {}{}",
        availability(inspection.moonlight.available),
        version_suffix(inspection.moonlight.version.as_deref())
    )?;
    if !inspection.diagnostics.is_empty() {
        writeln!(output, "Next:")?;
        for diagnostic in &inspection.diagnostics {
            writeln!(output, "  {} {}", severity_marker(diagnostic.severity), diagnostic.summary)?;
            if let Some(action) = &diagnostic.action {
                writeln!(output, "    {}", action.label)?;
            }
        }
    }
    print!("{output}");
    Ok(())
}

fn print_status(cx: &Context, report: &StreamStatusReport) -> Result<()> {
    if cx.out.is_json() {
        return cx.out.json(report);
    }
    println!(
        "{} · session {:?} · host {:?}",
        report.inspection.target.display_name, report.session, report.inspection.readiness
    );
    Ok(())
}

fn print_doctor(cx: &Context, report: &StreamDoctorReport) -> Result<()> {
    if cx.out.is_json() {
        return cx.out.json(report);
    }
    println!(
        "{} · {}",
        report.target.display_name,
        if report.ready { "ready" } else { "attention required" }
    );
    for check in &report.checks {
        let marker = match check.status {
            DoctorCheckStatus::Pass => "✓",
            DoctorCheckStatus::Attention => "!",
            DoctorCheckStatus::Fail => "×",
        };
        println!("  {marker} {}", check.summary);
        if let Some(action) = &check.action {
            println!("    {}", action.label);
        }
    }
    Ok(())
}

fn availability(available: bool) -> &'static str {
    if available {
        "available"
    } else {
        "unavailable"
    }
}

fn version_suffix(version: Option<&str>) -> String {
    version.map(|version| format!(" · {version}")).unwrap_or_default()
}

fn severity_marker(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Info => "·",
        DiagnosticSeverity::Warning => "!",
        DiagnosticSeverity::Error => "×",
    }
}
