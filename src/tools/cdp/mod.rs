//! `cdp` — a warm, attach-based Chrome DevTools Protocol debugger for Electron fleets.
//!
//! Every command talks to a warm Attachment daemon (lazily spawned, kept alive across reloads and
//! restarts) over a unix socket; the daemon holds the live CDP connection and the Timeline. The
//! engine itself is the shared `crate::cdp` module — this tool is the daemon, the thin client, and
//! the command surface on top of it. See `CONTEXT.md` and `docs/adr/000{1,2,3}`.

mod client;
mod daemon;
mod format;
mod protocol;
mod registry;
mod snapshot;

use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command as ClapCommand, CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::cdp::TrackKind;
use crate::framework::{Context, Tool, ToolMeta};

use protocol::Command;

pub fn tool() -> CdpTool {
    CdpTool
}

const HELP_RECIPES: &str = "\
RECIPES
  Orient, then probe — everything lazy-attaches and stays warm:
    kit cdp                                 instances + live attachments
    kit cdp eval 'location.href' --app dev
    kit cdp tail --since 3s --app dev        all tracks on one clock

  Capture errors that fire on load / compile / reload:
    The Timeline records from attach onward (CDP never replays the past), so
    PRE-WARM first, then reproduce:
      kit cdp attach --app dev               warm BEFORE the error fires
      # …save the file / reload the window…
      kit cdp console --since 30s --app dev
      kit cdp tail --track exception --since 30s --app dev

  Inspect & drive a target (refs come from snap):
    kit cdp snap -i --app dev
    kit cdp click @e5 --app dev

  Health & cleanup:
    kit cdp ls                               kit cdp detach --all
";

pub struct CdpTool;

#[derive(Parser)]
#[command(
    name = "cdp",
    about = "Warm Chrome DevTools Protocol debugger for Electron fleets",
    long_about = "Attaches to a running Electron instance and keeps the connection warm in a background daemon: \
                  one correlated Timeline (console · network · websocket), live probes (eval · heap · targets), \
                  and scriptable lenses. Survives HMR reloads and app restarts; addresses targets by selector. \
                  The first command lazily attaches — there is no setup step.",
    after_help = HELP_RECIPES
)]
struct CdpArgs {
    #[command(subcommand)]
    command: Option<CdpCommand>,

    /// Instance selector — app name, worktree, instance id, or port. Picks which Attachment to use.
    #[arg(long, global = true)]
    app: Option<String>,
}

#[derive(Subcommand)]
enum CdpCommand {
    /// Pre-warm an Attachment (lazy auto-attach makes this optional).
    Attach {
        /// Tracks to capture, comma-separated (default: all).
        #[arg(long)]
        track: Option<String>,
    },
    /// Dispose an Attachment.
    Detach {
        #[arg(long)]
        all: bool,
    },
    /// List live Attachments and their health.
    Ls,
    /// Sweep dead Attachments.
    Gc,
    /// List the Targets in the Instance, with the selector that addresses each.
    Targets,
    /// Slice the Timeline — all tracks on one clock (the "what just happened" view).
    Tail {
        #[arg(long)]
        since: Option<String>,
        /// Restrict to tracks, comma-separated.
        #[arg(long)]
        track: Option<String>,
    },
    /// Timeline, console tracks only (console · exceptions · log).
    Console {
        #[arg(long)]
        since: Option<String>,
    },
    /// Timeline, network requests only.
    Net {
        #[arg(long)]
        since: Option<String>,
    },
    /// Timeline, websocket frames only (your realtime/RPC wire).
    Ws {
        #[arg(long)]
        since: Option<String>,
    },
    /// Evaluate JS in a Target and return its value.
    Eval {
        expr: Vec<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        target: Option<String>,
    },
    /// JS heap + DOM counters for a Target, on demand.
    Heap {
        #[arg(long)]
        target: Option<String>,
    },
    /// Accessibility-tree snapshot with `@eN` refs for click/fill.
    Snap {
        /// Compact view — only ref-bearing nodes and their structure.
        #[arg(short, long)]
        interactive: bool,
        #[arg(long)]
        target: Option<String>,
    },
    /// Click an element by its `@ref` from the last snap.
    Click {
        reference: String,
        #[arg(long)]
        target: Option<String>,
    },
    /// Fill an input by its `@ref` with text.
    Fill {
        reference: String,
        text: Vec<String>,
        #[arg(long)]
        target: Option<String>,
    },
    /// Run a saved lens script inside a Target.
    Lens {
        name: String,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
        #[arg(long)]
        target: Option<String>,
    },
    /// Internal: the Attachment daemon. Not for direct use.
    #[command(name = "__serve", hide = true)]
    Serve {
        #[arg(long)]
        name: String,
        #[arg(long)]
        selector: String,
        #[arg(long)]
        port: u16,
        #[arg(long, default_value_t = 0)]
        root_pid: u32,
        #[arg(long)]
        track: Option<String>,
    },
}

#[async_trait]
impl Tool for CdpTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "cdp",
            about: "Warm Chrome DevTools Protocol debugger for Electron fleets",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> ClapCommand {
        CdpArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = CdpArgs::from_arg_matches(matches)?;
        let json = cx.out.is_json();
        let app = args.app.as_deref();

        match args.command {
            None => client::overview(json).await,

            Some(CdpCommand::Serve { name, selector, port, root_pid, track }) => {
                daemon::serve(name, selector, port, root_pid, tracks_or_all(track.as_deref())).await
            }

            Some(CdpCommand::Attach { track }) => {
                client::attach(app, tracks_or_all(track.as_deref()), json).await
            }
            Some(CdpCommand::Detach { all }) => client::detach(app, all).await,
            Some(CdpCommand::Ls) => client::ls(json),
            Some(CdpCommand::Gc) => client::gc(json),

            Some(CdpCommand::Targets) => finish(client::query(app, json, Command::Targets).await?),
            Some(CdpCommand::Tail { since, track }) => {
                let command = Command::Tail { since_ms: parse_since(since.as_deref()), tracks: track.map(parse_tracks) };
                finish(client::query(app, json, command).await?)
            }
            Some(CdpCommand::Console { since }) => {
                let command = Command::Tail {
                    since_ms: parse_since(since.as_deref()),
                    tracks: Some(vec![TrackKind::Console, TrackKind::Exception, TrackKind::Log]),
                };
                finish(client::query(app, json, command).await?)
            }
            Some(CdpCommand::Net { since }) => {
                let command = Command::Tail { since_ms: parse_since(since.as_deref()), tracks: Some(vec![TrackKind::Network]) };
                finish(client::query(app, json, command).await?)
            }
            Some(CdpCommand::Ws { since }) => {
                let command = Command::Tail { since_ms: parse_since(since.as_deref()), tracks: Some(vec![TrackKind::Ws]) };
                finish(client::query(app, json, command).await?)
            }

            Some(CdpCommand::Eval { expr, file, target }) => {
                let expression = read_expr(expr, file)?;
                finish(client::query(app, json, Command::Eval { target, expr: expression }).await?)
            }
            Some(CdpCommand::Heap { target }) => finish(client::query(app, json, Command::Heap { target }).await?),
            Some(CdpCommand::Snap { interactive, target }) => {
                finish(client::query(app, json, Command::Snap { target, interactive }).await?)
            }
            Some(CdpCommand::Click { reference, target }) => {
                finish(client::query(app, json, Command::Click { target, reference }).await?)
            }
            Some(CdpCommand::Fill { reference, text, target }) => {
                let text = text.join(" ");
                finish(client::query(app, json, Command::Fill { target, reference, text }).await?)
            }
            Some(CdpCommand::Lens { name, args, target }) => {
                let source = load_lens(&name)?;
                finish(client::query(app, json, Command::Lens { target, source, args }).await?)
            }
        }
    }
}

/// Map a command's success flag to a process exit code without re-printing (output is already out).
fn finish(ok: bool) -> Result<()> {
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

fn read_expr(expr: Vec<String>, file: Option<PathBuf>) -> Result<String> {
    if let Some(path) = file {
        return std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()));
    }
    if expr.is_empty() {
        bail!("nothing to evaluate — pass an expression or --file <path>");
    }
    Ok(expr.join(" "))
}

fn load_lens(name: &str) -> Result<String> {
    let dir = lens_dir();
    let path = dir.join(format!("{name}.js"));
    std::fs::read_to_string(&path).with_context(|| {
        let available = list_lenses(&dir);
        format!("no lens '{name}' at {}{available}", path.display())
    })
}

fn list_lenses(dir: &std::path::Path) -> String {
    let names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "js") {
                path.file_stem().map(|stem| stem.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect();
    if names.is_empty() {
        String::new()
    } else {
        format!("\navailable: {}", names.join(", "))
    }
}

fn lens_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "kit")
        .map(|dirs| dirs.config_dir().join("cdp/lenses"))
        .unwrap_or_else(|| PathBuf::from("cdp/lenses"))
}

/// Parse a comma-separated track list, skipping unknown names.
fn parse_tracks(csv: String) -> Vec<TrackKind> {
    csv.split(',').filter_map(TrackKind::parse).collect()
}

/// Parsed tracks, falling back to all of them when none are given.
fn tracks_or_all(csv: Option<&str>) -> Vec<TrackKind> {
    match csv.map(|csv| csv.split(',').filter_map(TrackKind::parse).collect::<Vec<_>>()) {
        Some(tracks) if !tracks.is_empty() => tracks,
        _ => TrackKind::ALL.to_vec(),
    }
}

/// `2s` / `500ms` / `5m` → milliseconds. Default 10s.
fn parse_since(value: Option<&str>) -> u64 {
    let Some(value) = value else {
        return 10_000;
    };
    let value = value.trim();
    let parse = |suffix: &str, scale: u64| {
        value.strip_suffix(suffix).and_then(|n| n.trim().parse::<u64>().ok()).map(|n| n * scale)
    };
    parse("ms", 1)
        .or_else(|| parse("s", 1_000))
        .or_else(|| parse("m", 60_000))
        .or_else(|| value.parse::<u64>().ok().map(|n| n * 1_000))
        .unwrap_or(10_000)
}
