//! console — persistent terminal sessions backed by the embedded WezTerm mux.

mod activity;
mod agent;
mod authorization;
mod client;
mod config;
mod connection;
mod control_center;
mod interaction;
mod invalidation;
mod notification;
mod panels;
mod perf_trace;
mod remote;
mod runtime;
mod scroll;
mod service;
mod transport;
mod tui;

use std::{
    ffi::{OsStr, OsString},
    sync::Arc,
};

use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::framework::{Context, SettingsSection, Tool, ToolMeta};

pub fn tool() -> ConsoleTool {
    ConsoleTool
}

pub struct ConsoleTool;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HiddenEntry {
    Agent,
}

#[derive(Parser)]
#[command(name = "console", about = "Persistent terminal sessions across your tailnet")]
struct ConsoleArgs {
    /// Tailnet machine, or `this-machine` for the direct local path.
    #[arg(value_name = "MACHINE", value_parser = parse_machine_selector)]
    machine: Option<String>,
    /// Create a new session immediately after connecting.
    #[arg(long)]
    new: bool,
    #[command(subcommand)]
    command: Option<ConsoleCommand>,
}

#[derive(Subcommand)]
enum ConsoleCommand {
    /// Install or repair the native per-user Console service.
    Setup,
    /// Restart the native Console service without losing sessions by default.
    Restart {
        /// Close every remaining session before restarting the service.
        #[arg(long)]
        force: bool,
    },
    /// Report the native service, socket, and mux state.
    Status,
    /// Stop the native Console service without losing sessions by default.
    Stop {
        /// Close every remaining session before stopping the service.
        #[arg(long)]
        force: bool,
    },
}

fn parse_machine_selector(value: &str) -> Result<String, String> {
    if value == "start" {
        Err("`console start` was removed; use `console setup`".to_owned())
    } else if value.contains('@') {
        Err("Console machine selectors cannot contain '@'".to_owned())
    } else {
        Ok(value.to_owned())
    }
}

/// Run the exact hidden agent entry before Kit constructs its ordinary application runtime.
///
/// The agent builds a small signal runtime on the process main thread; the embedded mux itself
/// remains on the dedicated owner thread in `agent`.
pub fn run_hidden_entry_if_requested() -> Option<Result<()>> {
    let entry = hidden_entry(std::env::args_os())?;

    Some(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("starting the Console agent signal runtime")
            .and_then(|runtime| match entry {
                HiddenEntry::Agent => runtime.block_on(agent::run()),
            }),
    )
}

fn hidden_entry(args: impl IntoIterator<Item = OsString>) -> Option<HiddenEntry> {
    let mut args = args.into_iter();
    let _executable = args.next()?;
    if args.next().as_deref() != Some(OsStr::new("console")) {
        return None;
    }
    let entry = match args.next().as_deref() {
        Some(value) if value == OsStr::new("__agent") => HiddenEntry::Agent,
        _ => return None,
    };
    args.next().is_none().then_some(entry)
}

pub(crate) fn build_identity() -> Result<wezterm_codec::BuildIdentity> {
    let source_revision = env!("KIT_SOURCE_REVISION");
    if source_revision.len() != 40 || !source_revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("Kit was built without an exact source revision");
    }
    let source_dirty = match env!("KIT_SOURCE_DIRTY") {
        "true" => true,
        "false" => false,
        _ => bail!("Kit was built without an exact source dirty state"),
    };
    let wezterm_revision = env!("KIT_WEZTERM_REVISION");
    if wezterm_revision.len() != 40
        || !wezterm_revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("Kit was built without an exact embedded WezTerm revision");
    }
    Ok(wezterm_codec::BuildIdentity {
        product: "kit-console".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        source_revision: Some(source_revision.to_owned()),
        source_dirty: Some(source_dirty),
        embedded_wezterm_revision: Some(wezterm_revision.to_owned()),
    })
}

#[async_trait]
impl Tool for ConsoleTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "console",
            about: "Persistent terminal sessions across your tailnet",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        ConsoleArgs::command()
    }

    fn settings(&self) -> Option<SettingsSection> {
        Some(config::settings())
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = ConsoleArgs::from_arg_matches(matches)?;
        if let Some(command) = args.command {
            return match command {
                ConsoleCommand::Setup => print_status(cx, &service::setup(&cx.processes).await?),
                ConsoleCommand::Restart { force } => {
                    print_status(cx, &service::restart(&cx.processes, force).await?)
                }
                ConsoleCommand::Status => print_status(cx, &service::status(&cx.processes).await?),
                ConsoleCommand::Stop { force } => {
                    print_status(cx, &service::stop(&cx.processes, force).await?)
                }
            };
        }
        if !cx.term.interactive() {
            bail!("kit console requires an interactive terminal");
        }
        if cx.out.is_json() {
            bail!("kit console is an interactive TUI and does not emit JSON");
        }

        let connection_owner = Arc::new(client::connection_owner()?);
        if let Some(machine) = args.machine {
            if machine == "this-machine" {
                let request =
                    control_center::MachineConnectionRequest::Local { create_session: args.new };
                let _ = run_connection(cx, &connection_owner, request).await?;
            } else {
                let _ =
                    run_direct_remote_connection(cx, &connection_owner, &machine, args.new).await?;
            }
            return Ok(());
        }

        let mut control_center_notice = None;
        loop {
            let config = config::Config::load(cx.config.clone())?;
            match control_center::run(
                cx,
                config,
                Arc::clone(&connection_owner),
                control_center_notice.take(),
            )
            .await?
            {
                control_center::ControlCenterOutcome::Connect(request) => {
                    let remote_selector = match &request {
                        control_center::MachineConnectionRequest::Remote { machine, .. } => {
                            Some(machine.selector.clone())
                        }
                        control_center::MachineConnectionRequest::Local { .. } => None,
                    };
                    match run_connection(cx, &connection_owner, request).await {
                        Ok(control_center::ConnectedSessionOutcome::ReturnToControlCenter) => {}
                        Ok(control_center::ConnectedSessionOutcome::Quit) => return Ok(()),
                        Err(error) => {
                            let Some(remote_selector) = remote_selector else {
                                return Err(error);
                            };
                            control_center_notice =
                                Some(format!("Could not connect to {remote_selector}: {error:#}"));
                        }
                    }
                }
                control_center::ControlCenterOutcome::Updated => {
                    println!("Kit was updated. Run kit console again to use the replacement.");
                    return Ok(());
                }
                control_center::ControlCenterOutcome::Quit => return Ok(()),
            }
        }
    }
}

async fn run_connection(
    cx: &Context,
    connection_owner: &connection::ConnectionOwner,
    request: control_center::MachineConnectionRequest,
) -> Result<control_center::ConnectedSessionOutcome> {
    let config = config::Config::load(cx.config.clone())?;
    match request {
        control_center::MachineConnectionRequest::Local { create_session } => {
            let client = client::ConsoleClient::connect(connection_owner).await?;
            if create_session {
                client.create_session(120, 32).await?;
            }
            tui::run(client, config, "this-machine".to_owned()).await
        }
        control_center::MachineConnectionRequest::Remote { machine, create_session } => {
            let mut resolution =
                remote::resolve_identity(&cx.processes, &machine.stable_node_id).await?;
            if matches!(
                &resolution,
                remote::Resolution::Status(service::ConsoleStatus::NeedsTailscaleLogin)
            ) {
                remote::login(&cx.processes).await?;
                resolution =
                    remote::resolve_identity(&cx.processes, &machine.stable_node_id).await?;
            }
            run_resolved_remote_connection(
                cx,
                connection_owner,
                config,
                resolution,
                machine.selector,
                create_session,
            )
            .await
        }
    }
}

async fn run_direct_remote_connection(
    cx: &Context,
    connection_owner: &connection::ConnectionOwner,
    selector: &str,
    create_session: bool,
) -> Result<control_center::ConnectedSessionOutcome> {
    let config = config::Config::load(cx.config.clone())?;
    let mut resolution = remote::resolve(&cx.processes, selector).await?;
    if matches!(
        &resolution,
        remote::Resolution::Status(service::ConsoleStatus::NeedsTailscaleLogin)
    ) {
        remote::login(&cx.processes).await?;
        resolution = remote::resolve(&cx.processes, selector).await?;
    }
    run_resolved_remote_connection(
        cx,
        connection_owner,
        config,
        resolution,
        selector.to_owned(),
        create_session,
    )
    .await
}

async fn run_resolved_remote_connection(
    cx: &Context,
    connection_owner: &connection::ConnectionOwner,
    config: config::Config,
    resolution: remote::Resolution,
    target_label: String,
    create_session: bool,
) -> Result<control_center::ConnectedSessionOutcome> {
    let target = match resolution {
        remote::Resolution::Ready(target) => target,
        remote::Resolution::Status(status) => bail!("{}", status.text()),
    };
    let relay = remote::start_relay(&cx.processes, &target).await?;
    let relay_socket = relay.socket_path().to_owned();
    let relay_status = relay.status_receiver();
    let tui_target = target_label.clone();
    let result = async {
        let client =
            client::ConsoleClient::connect_to_relay(connection_owner, relay_socket, relay_status)
                .await?;
        if create_session {
            client.create_session(120, 32).await?;
        }
        tui::run(client, config, tui_target).await
    }
    .await;
    let gateway_build = relay.gateway_build();
    let failure_status = if result.is_err() { relay.failure_status().await } else { None };
    let shutdown = relay.shutdown().await;
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            if let Some(status) = failure_status {
                bail!("{}", status.text());
            }
            if let Some(incompatible) =
                error.downcast_ref::<wezterm_client::client::IncompatibleVersionError>()
            {
                let build = gateway_build
                    .and_then(|build| serde_json::to_string(&build).ok())
                    .unwrap_or_else(|| "unavailable".to_owned());
                let status = service::ConsoleStatus::TailnetProtocolIncompatible {
                    machine: target_label,
                    detail: format!(
                        "target mux version {} uses codec {}; gateway build: {build}",
                        incompatible.version, incompatible.codec_vers
                    ),
                };
                bail!("{}", status.text());
            }
            return Err(error);
        }
    };
    shutdown?;
    Ok(outcome)
}

fn print_status(cx: &Context, status: &service::ConsoleStatus) -> Result<()> {
    if cx.out.is_json() {
        cx.out.json(status)
    } else {
        println!("{}", status.text());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{hidden_entry, ConsoleArgs, ConsoleCommand, HiddenEntry};
    use clap::Parser;

    #[test]
    fn console_cli_distinguishes_machine_from_lifecycle_commands() {
        let local = ConsoleArgs::try_parse_from(["console"]).unwrap();
        assert!(local.machine.is_none());
        assert!(local.command.is_none());

        let remote = ConsoleArgs::try_parse_from(["console", "workstation", "--new"]).unwrap();
        assert_eq!(remote.machine.as_deref(), Some("workstation"));
        assert!(remote.new);

        let local = ConsoleArgs::try_parse_from(["console", "this-machine"]).unwrap();
        assert_eq!(local.machine.as_deref(), Some("this-machine"));

        let setup = ConsoleArgs::try_parse_from(["console", "setup"]).unwrap();
        assert!(matches!(setup.command, Some(ConsoleCommand::Setup)));
        assert!(ConsoleArgs::try_parse_from(["console", "setup", "workstation"]).is_err());

        let stop = ConsoleArgs::try_parse_from(["console", "stop", "--force"]).unwrap();
        assert!(matches!(stop.command, Some(ConsoleCommand::Stop { force: true })));

        let restart = ConsoleArgs::try_parse_from(["console", "restart", "--force"]).unwrap();
        assert!(matches!(restart.command, Some(ConsoleCommand::Restart { force: true })));

        let gentle = ConsoleArgs::try_parse_from(["console", "restart"]).unwrap();
        assert!(matches!(gentle.command, Some(ConsoleCommand::Restart { force: false })));

        assert!(ConsoleArgs::try_parse_from(["console", "start"]).is_err());
        assert!(ConsoleArgs::try_parse_from(["console", "operator@workstation"]).is_err());
    }

    #[test]
    fn hidden_entries_require_the_exact_private_argv() {
        assert_eq!(
            hidden_entry(["kit", "console", "__agent"].map(Into::into)),
            Some(HiddenEntry::Agent)
        );
        assert_eq!(hidden_entry(["kit", "console", "bridge"].map(Into::into)), None);
    }
}
