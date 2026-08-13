//! `stream` — a one-shortcut window slot on a BetterDisplay virtual screen.

mod command;
mod contributions;
mod controller;
mod display;
mod model;
mod shortcut;
mod state;
mod sunshine;
mod tui;
mod window;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::framework::{Context, Tool, ToolMeta};

use controller::StreamController;

pub fn tool() -> StreamTool {
    StreamTool
}

pub struct StreamTool;

#[derive(Parser)]
#[command(name = "stream", about = "Send the focused window to a dedicated streaming display")]
struct StreamArgs {
    #[command(subcommand)]
    command: Option<StreamCommand>,
}

#[derive(Subcommand)]
enum StreamCommand {
    /// Send, switch, or recall the focused window.
    Toggle,
    /// Show the durable Stream Slot state.
    Status,
    /// Finish an interrupted recall and restore owned resources.
    Recover,
    /// Manage the Karabiner global shortcut.
    Shortcut {
        #[command(subcommand)]
        command: ShortcutCommand,
    },
}

#[derive(Subcommand)]
enum ShortcutCommand {
    /// Install Cmd+Shift+M into the selected Karabiner profile.
    Install,
    /// Remove only Kit-owned Stream shortcut rules.
    Remove,
    /// Report whether the global shortcut is installed.
    Status,
}

#[async_trait]
impl Tool for StreamTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "stream",
            about: "Send the focused window to a dedicated streaming display",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        StreamArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let arguments = StreamArgs::from_arg_matches(matches)?;
        let working_directory = std::env::current_dir().context("resolve current directory")?;
        let controller = StreamController::new(cx.processes.clone(), working_directory);
        match arguments.command {
            Some(StreamCommand::Toggle) => {
                let report = controller.toggle().await?;
                if cx.out.is_json() {
                    cx.out.json(&report)
                } else {
                    println!("{:?} {} · {}", report.action, report.app_name, report.window_title);
                    Ok(())
                }
            }
            Some(StreamCommand::Status) => print_status(cx, &controller.status().await?),
            Some(StreamCommand::Recover) => {
                let report = controller.recover().await?;
                if cx.out.is_json() {
                    cx.out.json(&report)
                } else if let Some(report) = report {
                    println!("Recovered {} · {}", report.app_name, report.window_title);
                    Ok(())
                } else {
                    println!("Nothing needs recovery");
                    Ok(())
                }
            }
            Some(StreamCommand::Shortcut { command }) => match command {
                ShortcutCommand::Install => {
                    let changed = controller.install_shortcut()?;
                    print_shortcut_change(
                        cx,
                        changed,
                        controller.shortcut_status()?,
                        if changed {
                            "Installed Cmd+Shift+M Stream Slot shortcut"
                        } else {
                            "Cmd+Shift+M Stream Slot shortcut is already installed"
                        },
                    )
                }
                ShortcutCommand::Remove => {
                    let changed = controller.remove_shortcut()?;
                    print_shortcut_change(
                        cx,
                        changed,
                        controller.shortcut_status()?,
                        if changed {
                            "Removed Kit Stream Slot shortcut"
                        } else {
                            "Kit Stream Slot shortcut was not installed"
                        },
                    )
                }
                ShortcutCommand::Status => {
                    let status = controller.shortcut_status()?;
                    if cx.out.is_json() {
                        cx.out.json(&status)
                    } else {
                        println!(
                            "Cmd+Shift+M Stream Slot shortcut: {}",
                            if status == shortcut::ShortcutStatus::Installed {
                                "installed"
                            } else {
                                "not installed"
                            }
                        );
                        Ok(())
                    }
                }
            },
            None if cx.term.interactive() && !cx.out.is_json() => {
                tui::run(controller, crate::tui::theme::NORD).await
            }
            None => print_status(cx, &controller.status().await?),
        }
    }
}

fn print_shortcut_change(
    cx: &Context,
    changed: bool,
    status: shortcut::ShortcutStatus,
    message: &str,
) -> Result<()> {
    if cx.out.is_json() {
        cx.out.json(&serde_json::json!({ "changed": changed, "status": status }))
    } else {
        println!("{message}");
        Ok(())
    }
}

fn print_status(cx: &Context, status: &model::SlotStatus) -> Result<()> {
    if cx.out.is_json() {
        cx.out.json(status)
    } else if let Some(app) = &status.app_name {
        println!("Stream Slot {:?} · {} · {}", status.phase, app, status.shortcut);
        Ok(())
    } else {
        println!("Stream Slot empty · {}", status.shortcut);
        Ok(())
    }
}
