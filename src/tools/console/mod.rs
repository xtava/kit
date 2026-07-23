//! console — persistent terminal sessions backed by the embedded WezTerm mux.

mod activity;
mod agent;
mod authorization;
mod bridge;
mod client;
mod config;
mod connection;
mod interaction;
mod invalidation;
mod perf_trace;
mod remote;
mod runtime;
mod service;
mod transport;
mod tui;

use std::ffi::{OsStr, OsString};

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
    Bridge,
}

#[derive(Parser)]
#[command(name = "console", about = "Persistent terminal sessions across your tailnet")]
struct ConsoleArgs {
    /// Tailnet machine. Omit it to use this machine.
    #[arg(value_name = "MACHINE")]
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
    Setup {
        /// Tailnet machine. Omit it to configure this machine.
        machine: Option<String>,
    },
    /// Report the native service, socket, and mux state.
    Status {
        /// Tailnet machine. Omit it to inspect this machine.
        machine: Option<String>,
    },
    /// Stop the native Console service without losing sessions by default.
    Stop {
        /// Tailnet machine. Omit it to stop Console on this machine.
        machine: Option<String>,
        /// Close every remaining session before stopping the service.
        #[arg(long)]
        force: bool,
    },
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
                HiddenEntry::Bridge => runtime.block_on(bridge::run()),
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
        Some(value) if value == OsStr::new("__bridge") => HiddenEntry::Bridge,
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
            about: "Persistent terminal sessions on this machine",
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
                ConsoleCommand::Setup { machine } => {
                    if let Some(machine) = machine {
                        let mut config = config::Config::load(cx.config.clone())?;
                        let status =
                            match remote::resolve(&cx.processes, &mut config, &machine).await? {
                                remote::Resolution::Ready(target) => {
                                    remote::setup(&cx.processes, &target).await?
                                }
                                remote::Resolution::Status(status) => status,
                            };
                        print_status(cx, &status)
                    } else {
                        print_status(cx, &service::setup(&cx.processes).await?)
                    }
                }
                ConsoleCommand::Status { machine } => {
                    if let Some(machine) = machine {
                        let mut config = config::Config::load(cx.config.clone())?;
                        let status =
                            match remote::resolve(&cx.processes, &mut config, &machine).await? {
                                remote::Resolution::Ready(target) => {
                                    remote::status(&cx.processes, &target).await?
                                }
                                remote::Resolution::Status(status) => status,
                            };
                        print_status(cx, &status)
                    } else {
                        print_status(cx, &service::status(&cx.processes).await?)
                    }
                }
                ConsoleCommand::Stop { machine, force } => {
                    if let Some(machine) = machine {
                        let mut config = config::Config::load(cx.config.clone())?;
                        let status =
                            match remote::resolve(&cx.processes, &mut config, &machine).await? {
                                remote::Resolution::Ready(target) => {
                                    remote::stop(&cx.processes, &target, force).await?
                                }
                                remote::Resolution::Status(status) => status,
                            };
                        print_status(cx, &status)
                    } else {
                        print_status(cx, &service::stop(&cx.processes, force).await?)
                    }
                }
            };
        }
        if !cx.term.interactive() {
            bail!("kit console requires an interactive terminal");
        }
        if cx.out.is_json() {
            bail!("kit console is an interactive TUI and does not emit JSON");
        }

        let mut config = config::Config::load(cx.config.clone())?;
        if let Some(machine) = args.machine {
            let mut resolution = remote::resolve(&cx.processes, &mut config, &machine).await?;
            if matches!(
                &resolution,
                remote::Resolution::Status(service::ConsoleStatus::NeedsTailscaleLogin { .. })
            ) {
                remote::login(&cx.processes).await?;
                resolution = remote::resolve(&cx.processes, &mut config, &machine).await?;
            }
            let target = match resolution {
                remote::Resolution::Ready(target) => target,
                remote::Resolution::Status(status) => bail!("{}", status.text()),
            };
            let relay = remote::start_relay(&cx.processes, &target).await?;
            let relay_socket = relay.socket_path().to_owned();
            let relay_status = relay.status_receiver();
            let result = async {
                let connection_owner = client::remote_connection_owner(relay_socket)?;
                let client =
                    client::ConsoleClient::connect_to_relay(&connection_owner, relay_status)
                        .await?;
                if args.new {
                    client.create_session(120, 32).await?;
                }
                tui::run(client, config).await
            }
            .await;
            let latest_status = relay.latest_status();
            let shutdown = relay.shutdown().await;
            if let Err(error) = result {
                if let Some(status) = latest_status {
                    bail!("{}", status.text());
                }
                return Err(error);
            }
            shutdown?;
            return Ok(());
        }

        let connection_owner = client::local_connection_owner()?;
        let client = client::ConsoleClient::connect(&connection_owner).await?;
        if args.new {
            client.create_session(120, 32).await?;
        }
        tui::run(client, config).await
    }
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

        let remote = ConsoleArgs::try_parse_from(["console", "tvxm", "--new"]).unwrap();
        assert_eq!(remote.machine.as_deref(), Some("tvxm"));
        assert!(remote.new);

        let setup = ConsoleArgs::try_parse_from(["console", "setup", "tvxm"]).unwrap();
        assert!(matches!(
            setup.command,
            Some(ConsoleCommand::Setup { machine }) if machine.as_deref() == Some("tvxm")
        ));

        let stop = ConsoleArgs::try_parse_from(["console", "stop", "--force"]).unwrap();
        assert!(matches!(stop.command, Some(ConsoleCommand::Stop { machine: None, force: true })));
    }

    #[test]
    fn hidden_entries_require_the_exact_private_argv() {
        assert_eq!(
            hidden_entry(["kit", "console", "__agent"].map(Into::into)),
            Some(HiddenEntry::Agent)
        );
        assert_eq!(
            hidden_entry(["kit", "console", "__bridge"].map(Into::into)),
            Some(HiddenEntry::Bridge)
        );
        assert_eq!(hidden_entry(["kit", "console", "__bridge", "extra"].map(Into::into)), None);
        assert_eq!(hidden_entry(["kit", "console", "bridge"].map(Into::into)), None);
    }
}
