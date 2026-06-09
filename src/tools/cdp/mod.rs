//! `cdp` — a warm, attach-based Chrome DevTools Protocol debugger for Electron fleets.
//!
//! Every command talks to a warm Attachment daemon (lazily spawned, kept alive across reloads and
//! restarts) over a unix socket; the daemon holds the live CDP connection and the Timeline. The
//! engine itself is the shared `crate::cdp` module — this tool is the daemon, the thin client, and
//! the command surface on top of it. See `CONTEXT.md` and `docs/adr/000{1,2,3}`.

mod client;
mod daemon;
mod format;
mod interactive;
mod protocol;
mod readiness;
mod registry;
mod snapshot;

use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use clap::{
    ArgMatches, Args, Command as ClapCommand, CommandFactory, FromArgMatches, Parser, Subcommand,
};

use crate::cdp::{Source, TrackKind};
use crate::framework::{Context, Tool, ToolMeta};

use protocol::{Command, IgnoreOp, TimelineQuery};

pub fn tool() -> CdpTool {
    CdpTool
}

const HELP_RECIPES: &str = "\
RECIPES
  Orient, then probe — everything lazy-attaches and stays warm:
    kit cdp                                 instances + live attachments
    kit cdp ready --app dev                  is the workbench up? which target won, and why
    kit cdp eval 'location.href' --app dev
    kit cdp tail --since 3s --app dev        all tracks on one clock

  Capture errors that fire on load / compile / reload:
    The Timeline records from attach onward (CDP never replays the past), so
    PRE-WARM first, then reproduce:
      kit cdp attach --app dev               warm BEFORE the error fires
      # …save the file / reload the window…
      kit cdp errors --since 30s --app dev   deduped: what broke, and how often
      kit cdp console --since 30s --app dev
      kit cdp tail --track exception --since 30s --app dev

  Split the Electron main process from the web renderer:
    kit cdp tail --source main --app dev      Node main only (needs --inspect)
    kit cdp console --source renderer --app dev

  Inspect & drive a target (refs come from snap):
    kit cdp snap -i --app dev
    kit cdp click @e5 --app dev

  Extension runtime diagnosis:
    kit cdp lens extensions --app dev -- modular.local-sdk-view-showcase
    kit cdp ext doctor modular.local-sdk-view-showcase --app dev
    kit cdp ext bundle modular.local-sdk-view-showcase --since 60s --app dev

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

    /// Enter the live interactive debugger — a streaming Timeline you drive with commands.
    #[arg(short, long)]
    interactive: bool,

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
        /// Restrict to tracks, comma-separated.
        #[arg(long)]
        track: Option<String>,
        #[command(flatten)]
        filter: TimelineFilterArgs,
    },
    /// Timeline, console tracks only (console · exceptions · log).
    Console {
        #[command(flatten)]
        filter: TimelineFilterArgs,
    },
    /// What's broken — error-shaped events (exceptions · console.error · log/error · failed
    /// requests), with duplicates collapsed to `error (N×)` so it stays cheap to read. Never hides
    /// silently: a `⚠` banner fires if the view is lossy (merged differing errors, ring eviction,
    /// undecoded events). Use `--explain` to expand exactly what each group absorbed.
    Errors {
        /// Expand each collapsed group into the distinct lines it absorbed — the audit view.
        #[arg(long)]
        explain: bool,
        #[command(flatten)]
        filter: TimelineFilterArgs,
    },
    /// Timeline, network requests only.
    Net {
        #[command(flatten)]
        filter: TimelineFilterArgs,
    },
    /// Timeline, websocket frames only (your realtime/RPC wire).
    Ws {
        #[command(flatten)]
        filter: TimelineFilterArgs,
    },
    /// Evaluate JS in a Target and return its value.
    Eval {
        expr: Vec<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        target: Option<String>,
    },
    /// Is the workbench up? Reports the selected Target, its document state, recent errors, and the
    /// ranked candidate field with why each won or lost.
    Ready {
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
    /// Suppress noise from the Timeline — add a substring, `--list`, or `--clear` (per attachment).
    Ignore {
        pattern: Vec<String>,
        #[arg(long)]
        list: bool,
        #[arg(long)]
        clear: bool,
    },
    /// Run a saved lens script inside a Target.
    Lens {
        name: Option<String>,
        #[arg(long)]
        list: bool,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
        #[arg(long)]
        target: Option<String>,
    },
    /// Extension-runtime shortcuts built on the Modular extension lens.
    Ext {
        #[command(subcommand)]
        command: ExtensionCommand,
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

#[derive(Args, Clone)]
struct TimelineFilterArgs {
    #[arg(long)]
    since: Option<String>,
    /// Restrict to one process side: `main` (Electron main) or `renderer` (web).
    #[arg(long)]
    source: Option<String>,
    /// Restrict to Timeline events whose Target label matches this selector.
    #[arg(long)]
    target: Option<String>,
    /// Restrict to rendered Timeline rows containing this text.
    #[arg(long)]
    grep: Option<String>,
    /// Restrict to rows associated with an extension id.
    #[arg(long)]
    extension: Option<String>,
    /// Return only the most recent N matching rows.
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Subcommand)]
enum ExtensionCommand {
    /// Diagnose one extension from the live runtime graph.
    Doctor {
        extension_id: String,
        #[arg(long)]
        target: Option<String>,
    },
    /// Capture extension diagnosis plus a bounded Timeline slice.
    Bundle {
        extension_id: String,
        #[command(flatten)]
        filter: TimelineFilterArgs,
        #[arg(long)]
        target: Option<String>,
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
            None if args.interactive => interactive::run(app).await,
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
            Some(CdpCommand::Lens { list: true, .. }) => {
                println!("{}", render_lens_list(json));
                Ok(())
            }

            Some(session) => finish(client::query(app, json, session_command(session)?).await?),
        }
    }
}

/// Map a parsed session subcommand to its wire [`Command`]. Shared by the one-shot CLI and the
/// interactive session, so a typed `eval` in the REPL and `kit cdp eval` are the same grammar.
/// Lifecycle subcommands (attach/detach/ls/gc/serve) are routed before this and never reach it.
fn session_command(command: CdpCommand) -> Result<Command> {
    Ok(match command {
        CdpCommand::Targets => Command::Targets,
        CdpCommand::Tail { track, filter } => {
            Command::Tail(timeline_query(filter, track.map(parse_tracks))?)
        }
        CdpCommand::Console { filter } => Command::Tail(timeline_query(
            filter,
            Some(vec![TrackKind::Console, TrackKind::Exception, TrackKind::Log]),
        )?),
        CdpCommand::Errors { explain, filter } => {
            Command::Errors { query: timeline_query(filter, None)?, explain }
        }
        CdpCommand::Net { filter } => {
            Command::Tail(timeline_query(filter, Some(vec![TrackKind::Network]))?)
        }
        CdpCommand::Ws { filter } => {
            Command::Tail(timeline_query(filter, Some(vec![TrackKind::Ws]))?)
        }
        CdpCommand::Eval { expr, file, target } => {
            Command::Eval { target, expr: read_expr(expr, file)? }
        }
        CdpCommand::Ready { target } => Command::Ready { target },
        CdpCommand::Heap { target } => Command::Heap { target },
        CdpCommand::Snap { interactive, target } => Command::Snap { target, interactive },
        CdpCommand::Click { reference, target } => Command::Click { target, reference },
        CdpCommand::Fill { reference, text, target } => {
            Command::Fill { target, reference, text: text.join(" ") }
        }
        CdpCommand::Ignore { pattern, list, clear } => {
            Command::Ignore(ignore_op(pattern, list, clear))
        }
        CdpCommand::Lens { name, list: false, args, target } => {
            let Some(name) = name else {
                bail!("no lens name — pass a name or use `kit cdp lens --list`");
            };
            Command::Lens { target, source: load_lens(&name)?, args }
        }
        CdpCommand::Lens { list: true, .. } => bail!("lens --list is not a session command"),
        CdpCommand::Ext { command } => match command {
            ExtensionCommand::Doctor { extension_id, target } => {
                Command::Lens { target, source: load_lens("extensions")?, args: vec![extension_id] }
            }
            ExtensionCommand::Bundle { extension_id, filter, target } => {
                let query = timeline_query(filter, None)?.with_extension(extension_id.clone());
                Command::ExtensionBundle {
                    target,
                    source: load_lens("extensions")?,
                    extension_id,
                    query,
                }
            }
        },
        CdpCommand::Attach { .. }
        | CdpCommand::Detach { .. }
        | CdpCommand::Ls
        | CdpCommand::Gc
        | CdpCommand::Serve { .. } => {
            bail!("not a session command — manage attachments from the shell, not in interactive mode")
        }
    })
}

fn ignore_op(pattern: Vec<String>, list: bool, clear: bool) -> IgnoreOp {
    if clear {
        IgnoreOp::Clear
    } else if list || pattern.is_empty() {
        IgnoreOp::List
    } else {
        IgnoreOp::Add(pattern.join(" "))
    }
}

fn timeline_query(
    filter: TimelineFilterArgs,
    tracks: Option<Vec<TrackKind>>,
) -> Result<TimelineQuery> {
    Ok(TimelineQuery {
        since_ms: parse_since(filter.since.as_deref()),
        tracks,
        source: parse_source(filter.source.as_deref())?,
        target: non_empty(filter.target),
        grep: non_empty(filter.grep),
        extension: non_empty(filter.extension),
        limit: filter.limit,
    })
}

trait TimelineQueryExt {
    fn with_extension(self, extension_id: String) -> Self;
}

impl TimelineQueryExt for TimelineQuery {
    fn with_extension(mut self, extension_id: String) -> Self {
        self.extension = Some(extension_id);
        self
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
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

/// Lenses that ship in the binary. A user file of the same name in `lens_dir()` shadows the builtin,
/// so these are starting points, not walls — `kit cdp lens workbench` works with zero setup, and a
/// `workbench.js` dropped in the config dir overrides it.
const BUILTIN_LENSES: &[(&str, &str)] = &[
    ("extensions", include_str!("lenses/extensions.js")),
    ("workbench", include_str!("lenses/workbench.js")),
];

/// Load a lens by name: a user file first (the override), then a builtin, else an error that lists
/// what *is* available.
fn load_lens(name: &str) -> Result<String> {
    let path = lens_dir().join(format!("{name}.js"));
    if let Ok(source) = std::fs::read_to_string(&path) {
        return Ok(source);
    }
    if let Some((_, source)) = BUILTIN_LENSES.iter().find(|(lens, _)| *lens == name) {
        return Ok((*source).to_owned());
    }
    Err(anyhow::anyhow!("no lens '{name}'{}", available_lenses()))
}

/// The lens names a user can run — builtins plus every `*.js` in the config dir, deduped and sorted.
fn available_lenses() -> String {
    let names = lens_names();
    if names.is_empty() {
        String::new()
    } else {
        format!("\navailable: {}", names.join(", "))
    }
}

fn render_lens_list(json: bool) -> String {
    let names = lens_names();
    if json {
        serde_json::to_string_pretty(&names).unwrap_or_else(|_| "[]".to_owned())
    } else if names.is_empty() {
        "no lenses available".to_owned()
    } else {
        names.join("\n")
    }
}

fn lens_names() -> Vec<String> {
    let mut names: Vec<String> =
        BUILTIN_LENSES.iter().map(|(name, _)| (*name).to_owned()).collect();
    if let Ok(entries) = std::fs::read_dir(lens_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "js") {
                if let Some(stem) = path.file_stem() {
                    names.push(stem.to_string_lossy().into_owned());
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
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

/// `main` / `renderer`, or an error on a typo — silently returning everything would be a quiet lie.
fn parse_source(value: Option<&str>) -> Result<Option<Source>> {
    match value {
        None => Ok(None),
        Some(value) => Source::parse(value)
            .map(Some)
            .with_context(|| format!("unknown source '{value}' — expected 'main' or 'renderer'")),
    }
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
