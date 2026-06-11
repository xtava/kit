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

use protocol::{Command, IgnoreOp, NetCommand, TimelineQuery};

pub fn tool() -> CdpTool {
    CdpTool
}

const HELP_RECIPES: &str = "\
RECIPES
  Launch a browser the agent can debug from the first app script:
    kit cdp launch http://localhost:3000 --name checkout --headless
    kit cdp launch-log --app checkout        startup Timeline from before navigation
    kit cdp state --visual --app checkout    readiness, focus, failures, screenshot path

  Bound an action so logs stay useful:
    kit cdp mark before-save --app checkout
    kit cdp click @e5 --app checkout
    kit cdp after before-save --app checkout
    kit cdp bundle checkout --since before-save

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
      kit cdp brief --since 30s --app dev    agent-safe compact Timeline
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

  Launched browser sessions:
    kit cdp launch http://localhost:3000 --name app
    kit cdp launched
    kit cdp state --app app
    kit cdp after before-click --app app
    kit cdp close app
";

const LAUNCH_AFTER_HELP: &str = "\
SAFETY
  Startup capture is on by default: the browser opens on about:blank, CDP capture starts,
  then the requested URL is loaded. Names may contain only ASCII letters, digits, '-' and '_'.
  The normal Chrome profile is never used; pass --profile to reuse an explicit kit profile.

EXAMPLES
  kit cdp launch http://localhost:3000 --name checkout --headless
  kit cdp launch http://localhost:3000 --name checkout --viewport 1440x1000 --timezone America/New_York
  kit cdp launch http://localhost:3000 --name checkout --profile authed-user --reuse
  kit cdp launch http://localhost:3000 --name checkout --replace
";

const LAUNCH_ELECTRON_AFTER_HELP: &str = "\
SAFETY
  The renderer CDP port is bound to localhost. With --cdp-port auto a free port is allocated and
  exported into the app's environment under --cdp-env; the app must read it (Electron calls
  app.commandLine.appendSwitch('remote-debugging-port', …) itself). Names may contain only ASCII
  letters, digits, '-' and '_'.

EXAMPLES
  kit cdp launch-electron --name studio --cwd . --cdp-env STUDIO_CDP_PORT \\
    --env STUDIO_DOC=documents/demo.html --renderer-target studio \\
    -- pnpm --filter studio start:prebuilt
  kit cdp launch-electron --name app --electron-arg '--remote-debugging-port={cdp_port}' -- ./my-app
  kit cdp launch-electron --name studio --cdp-port 9223 --cdp-env STUDIO_CDP_PORT -- pnpm start
";

const STATE_AFTER_HELP: &str = "\
EXAMPLES
  kit cdp state --app checkout
  kit cdp state --visual --app checkout
  kit cdp state --json --app checkout
";

const LAUNCH_LOG_AFTER_HELP: &str = "\
EXAMPLES
  kit cdp launch-log --app checkout
  kit cdp tail --since-mark launch --app checkout
";

const MARK_AFTER_HELP: &str = "\
EXAMPLES
  kit cdp mark before-save --app checkout
  kit cdp tail --since-mark before-save --app checkout
";

const AFTER_AFTER_HELP: &str = "\
EXAMPLES
  kit cdp after before-save --app checkout
  kit cdp after before-save --idle 500ms --timeout 5s --app checkout
";

const BUNDLE_AFTER_HELP: &str = "\
CONTENTS
  Bundles include summary.md, timeline.json, errors.txt, network.har, environment.json,
  redactions.json, and placeholder directories for screenshots/snapshots. Secrets are redacted
  unless --include-secrets is passed.

EXAMPLES
  kit cdp bundle checkout --since before-save
  kit cdp bundle checkout --include har,screenshots --json
";

const NET_AFTER_HELP: &str = "\
EXAMPLES
  kit cdp net failed --app checkout
  kit cdp net --since-mark before-save slow --app checkout
  kit cdp net show <request-id> --app checkout
  kit cdp net block analytics --app checkout
  kit cdp net mock GET /api/me fixtures/me.json --status 200 --app checkout
  kit cdp net rules clear --app checkout
";

const PROFILE_AFTER_HELP: &str = "\
EXAMPLES
  kit cdp profile ls
  kit cdp profile new clean
  kit cdp profile clone authed-user --from checkout
  kit cdp launch http://localhost:3000 --name checkout --profile authed-user
";

const CLOSE_AFTER_HELP: &str = "\
EXAMPLES
  kit cdp close checkout
  kit cdp close --all

NOTES
  close stops launched browsers and removes temporary profiles. detach only stops capture.
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
    /// Launch an isolated browser session, attach before navigation, and start capture.
    #[command(after_help = LAUNCH_AFTER_HELP)]
    Launch {
        /// URL to load after CDP capture is enabled.
        url: String,
        /// Stable launched-session name; only ASCII letters, digits, '-' and '_' are allowed.
        #[arg(long)]
        name: Option<String>,
        /// Browser executable to launch. Defaults to KIT_CDP_BROWSER or Chrome/Chromium discovery.
        #[arg(long)]
        browser: Option<PathBuf>,
        /// Launch Chrome with `--headless=new`.
        #[arg(long)]
        headless: bool,
        /// Require an unnamed temporary profile. Cannot be combined with --profile.
        #[arg(long)]
        fresh: bool,
        /// Use a named kit CDP profile from `kit cdp profile ls`.
        #[arg(long)]
        profile: Option<String>,
        /// Keep a temporary launch profile after close for later inspection.
        #[arg(long)]
        keep_profile: bool,
        /// Set viewport/window size, for example `1440x1000`.
        #[arg(long)]
        viewport: Option<String>,
        /// Load the URL directly and accept that early startup events may be missed.
        #[arg(long)]
        no_startup_capture: bool,
        /// Reuse an existing launched session with this name and navigate it.
        #[arg(long)]
        reuse: bool,
        /// Close any existing launched session with this name before launching a new one.
        #[arg(long)]
        replace: bool,
        /// Apply a session-scoped timezone override, for example `America/New_York`.
        #[arg(long)]
        timezone: Option<String>,
        /// Apply a session-scoped locale override, for example `fr-FR`.
        #[arg(long)]
        locale: Option<String>,
        /// Emulate `prefers-color-scheme: dark` for the session.
        #[arg(long)]
        dark: bool,
        /// Emulate offline network conditions for the session.
        #[arg(long)]
        offline: bool,
        /// Emulate network throttling. Supported presets: `slow-3g`, `fast-3g`.
        #[arg(long)]
        throttle: Option<String>,
    },
    /// Launch an Electron app, wait for the renderer CDP endpoint it exposes, and attach to it.
    #[command(after_help = LAUNCH_ELECTRON_AFTER_HELP)]
    LaunchElectron {
        /// The app command and its arguments, after `--`, e.g. `-- pnpm --filter studio start`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
        /// Stable launched-session name; only ASCII letters, digits, '-' and '_' are allowed.
        #[arg(long)]
        name: Option<String>,
        /// Working directory to spawn the command in. Defaults to the current directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Renderer CDP port. `auto` (default) allocates a free one; or pass a fixed port.
        #[arg(long, default_value = "auto")]
        cdp_port: String,
        /// Env var the app reads the renderer CDP port from, e.g. `STUDIO_CDP_PORT`.
        #[arg(long)]
        cdp_env: Option<String>,
        /// Extra `KEY=VALUE` environment entry for the app. Repeatable.
        #[arg(long)]
        env: Vec<String>,
        /// Extra process arg with `{cdp_port}` expanded, e.g. `--remote-debugging-port={cdp_port}`.
        #[arg(long)]
        electron_arg: Vec<String>,
        /// Renderer target to select by title/url substring once the endpoint is up.
        #[arg(long)]
        renderer_target: Option<String>,
        /// Skip the initial renderer-target probe and accept that early events may be missed.
        #[arg(long)]
        no_startup_capture: bool,
        /// Reuse an existing launched session with this name.
        #[arg(long)]
        reuse: bool,
        /// Close any existing launched session with this name before launching a new one.
        #[arg(long)]
        replace: bool,
    },
    /// List launched browser sessions.
    Launched,
    /// Close launched browser sessions. Unlike detach, this stops the browser process too.
    #[command(after_help = CLOSE_AFTER_HELP)]
    Close {
        /// Launched session name to close. Omit only when there is exactly one session.
        name: Option<String>,
        /// Close every launched browser session.
        #[arg(long)]
        all: bool,
    },
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
    /// Agent-safe Timeline brief: errors, repeated noise groups, recent raw tail, and omission
    /// counts. It compresses presentation only; use `tail` with the same filters for raw rows.
    Brief {
        /// Restrict to tracks, comma-separated.
        #[arg(long)]
        track: Option<String>,
        /// Raw recent rows to include after the summaries.
        #[arg(long, default_value_t = 12)]
        tail: usize,
        /// Repeated non-error groups to include.
        #[arg(long, default_value_t = 8)]
        groups: usize,
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
    /// Timeline/network tools. Bare `net` returns network Timeline rows.
    #[command(after_help = NET_AFTER_HELP)]
    Net {
        #[command(subcommand)]
        command: Option<CdpNetCommand>,
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
    /// Current target state: readiness, recent failures, focus, rules, and optional screenshot.
    #[command(after_help = STATE_AFTER_HELP)]
    State {
        /// Capture a screenshot and include its artifact path.
        #[arg(long)]
        visual: bool,
    },
    /// Launch startup log captured before navigation.
    #[command(after_help = LAUNCH_LOG_AFTER_HELP)]
    LaunchLog,
    /// Add a named mark to the Timeline.
    #[command(after_help = MARK_AFTER_HELP)]
    Mark {
        /// Mark name used later by --since-mark, after, and bundle --since.
        name: String,
    },
    /// Summarize what changed after a mark until idle or timeout.
    #[command(after_help = AFTER_AFTER_HELP)]
    After {
        /// Existing mark name to start from.
        mark: String,
        /// Stop after this much Timeline quiet. Accepts ms/s/m suffixes.
        #[arg(long, default_value = "500ms")]
        idle: String,
        /// Maximum time to wait before returning the summary.
        #[arg(long, default_value = "5s")]
        timeout: String,
    },
    /// Export a redacted evidence bundle for a launched session.
    #[command(after_help = BUNDLE_AFTER_HELP)]
    Bundle {
        /// Launched session name. Falls back to --app or the sole attachment when omitted.
        name: Option<String>,
        /// Named Timeline mark to start the bundle from.
        #[arg(long)]
        since: Option<String>,
        /// Comma-separated requested evidence families, for example `har,screenshots`.
        #[arg(long, value_delimiter = ',')]
        include: Vec<String>,
        /// Include sensitive cookies, auth-like fields, and request/response bodies.
        #[arg(long)]
        include_secrets: bool,
    },
    /// Manage named browser profiles used by launcher sessions.
    #[command(after_help = PROFILE_AFTER_HELP)]
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
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

#[derive(Subcommand)]
enum CdpNetCommand {
    /// Show failed network requests in the selected Timeline window.
    Failed,
    /// Rank recent network requests by observed request lifetime.
    Slow,
    /// Show all captured events for one CDP request id.
    Show {
        /// CDP request id from `net slow`, `net failed`, `state`, or raw network rows.
        request_id: String,
    },
    /// Block requests whose URL contains this substring.
    Block {
        /// Case-insensitive URL substring to block.
        pattern: String,
    },
    /// Fulfill matching requests from a fixture file.
    Mock {
        /// HTTP method to match.
        method: String,
        /// Case-insensitive URL substring to match.
        pattern: String,
        /// File whose contents become the mocked response body.
        fixture: PathBuf,
        /// HTTP status code to return.
        #[arg(long, default_value_t = 200)]
        status: u16,
        /// Response content-type. Defaults to application/json.
        #[arg(long)]
        mime: Option<String>,
    },
    /// List or clear active network block/mock rules.
    Rules {
        #[command(subcommand)]
        command: Option<NetRulesCommand>,
    },
}

#[derive(Subcommand)]
enum NetRulesCommand {
    /// Remove all active network block/mock rules.
    Clear,
}

#[derive(Subcommand)]
enum ProfileCommand {
    /// List named kit CDP profiles.
    Ls,
    /// Create an empty named kit CDP profile.
    New {
        /// Profile name. Stored under kit's CDP profile directory.
        name: String,
    },
    /// Clone a launched session's current profile into a named profile.
    Clone {
        /// New profile name.
        name: String,
        /// Launched session name to clone from.
        #[arg(long)]
        from: String,
    },
}

#[derive(Args, Clone)]
struct TimelineFilterArgs {
    /// Timeline window, for example `500ms`, `10s`, or `5m`.
    #[arg(long)]
    since: Option<String>,
    /// Start at a named Timeline mark.
    #[arg(long)]
    since_mark: Option<String>,
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

            Some(CdpCommand::Launch {
                url,
                name,
                browser,
                headless,
                fresh,
                profile,
                keep_profile,
                viewport,
                no_startup_capture,
                reuse,
                replace,
                timezone,
                locale,
                dark,
                offline,
                throttle,
            }) => {
                client::launch(
                    client::LaunchOptions {
                        url,
                        name,
                        browser,
                        headless,
                        fresh,
                        profile,
                        keep_profile,
                        viewport,
                        startup_capture: !no_startup_capture,
                        reuse,
                        replace,
                        timezone,
                        locale,
                        dark,
                        offline,
                        throttle,
                    },
                    json,
                )
                .await
            }
            Some(CdpCommand::LaunchElectron {
                command,
                name,
                cwd,
                cdp_port,
                cdp_env,
                env,
                electron_arg,
                renderer_target,
                no_startup_capture,
                reuse,
                replace,
            }) => {
                client::launch_electron(
                    client::ElectronLaunchOptions {
                        command,
                        name,
                        cwd,
                        cdp_port: parse_cdp_port(&cdp_port)?,
                        cdp_env,
                        env,
                        electron_args: electron_arg,
                        renderer_target,
                        startup_capture: !no_startup_capture,
                        reuse,
                        replace,
                    },
                    json,
                )
                .await
            }
            Some(CdpCommand::Launched) => client::launched(json).await,
            Some(CdpCommand::Close { name, all }) => {
                client::close_launched(name.as_deref(), all).await
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
            Some(CdpCommand::Profile { command }) => client::profile(profile_op(command), json),
            Some(CdpCommand::Bundle { name, since, include, include_secrets }) => finish(
                client::query(
                    name.as_deref().or(app),
                    json,
                    Command::Bundle { since, include, include_secrets },
                )
                .await?,
            ),

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
        CdpCommand::Brief { track, tail, groups, filter } => {
            Command::Brief { query: timeline_query(filter, track.map(parse_tracks))?, tail, groups }
        }
        CdpCommand::Console { filter } => Command::Tail(timeline_query(
            filter,
            Some(vec![TrackKind::Console, TrackKind::Exception, TrackKind::Log]),
        )?),
        CdpCommand::Errors { explain, filter } => {
            Command::Errors { query: timeline_query(filter, None)?, explain }
        }
        CdpCommand::Net { command: None, filter } => {
            Command::Tail(timeline_query(filter, Some(vec![TrackKind::Network]))?)
        }
        CdpCommand::Net { command: Some(command), filter } => Command::Net(net_command(
            command,
            timeline_query(filter, Some(vec![TrackKind::Network]))?,
        )?),
        CdpCommand::Ws { filter } => {
            Command::Tail(timeline_query(filter, Some(vec![TrackKind::Ws]))?)
        }
        CdpCommand::Eval { expr, file, target } => {
            Command::Eval { target, expr: read_expr(expr, file)? }
        }
        CdpCommand::Ready { target } => Command::Ready { target },
        CdpCommand::State { visual } => Command::State { visual },
        CdpCommand::LaunchLog => Command::LaunchLog,
        CdpCommand::Mark { name } => Command::Mark { name },
        CdpCommand::After { mark, idle, timeout } => Command::After {
            mark,
            idle_ms: parse_duration(&idle)?,
            timeout_ms: parse_duration(&timeout)?,
        },
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
        | CdpCommand::Launch { .. }
        | CdpCommand::LaunchElectron { .. }
        | CdpCommand::Launched
        | CdpCommand::Close { .. }
        | CdpCommand::Bundle { .. }
        | CdpCommand::Profile { .. }
        | CdpCommand::Detach { .. }
        | CdpCommand::Ls
        | CdpCommand::Gc
        | CdpCommand::Serve { .. } => {
            bail!("not a session command — manage attachments from the shell, not in interactive mode")
        }
    })
}

fn net_command(command: CdpNetCommand, query: TimelineQuery) -> Result<NetCommand> {
    Ok(match command {
        CdpNetCommand::Failed => NetCommand::Failed { query },
        CdpNetCommand::Slow => NetCommand::Slow { query },
        CdpNetCommand::Show { request_id } => NetCommand::Show { request_id },
        CdpNetCommand::Block { pattern } => NetCommand::Block { pattern },
        CdpNetCommand::Mock { method, pattern, fixture, status, mime } => NetCommand::Mock {
            method,
            pattern,
            body: std::fs::read_to_string(&fixture)
                .with_context(|| format!("read {}", fixture.display()))?,
            status,
            mime,
        },
        CdpNetCommand::Rules { command: Some(NetRulesCommand::Clear) } => NetCommand::RulesClear,
        CdpNetCommand::Rules { command: None } => NetCommand::Rules,
    })
}

fn profile_op(command: ProfileCommand) -> client::ProfileOp {
    match command {
        ProfileCommand::Ls => client::ProfileOp::Ls,
        ProfileCommand::New { name } => client::ProfileOp::New { name },
        ProfileCommand::Clone { name, from } => client::ProfileOp::Clone { name, from },
    }
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
        since_mark: non_empty(filter.since_mark),
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

/// `auto` → let the launcher allocate a free port; otherwise a fixed port number.
fn parse_cdp_port(value: &str) -> Result<Option<u16>> {
    if value.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    value
        .parse::<u16>()
        .map(Some)
        .with_context(|| format!("invalid --cdp-port '{value}' — use 'auto' or a port number"))
}

fn parse_duration(value: &str) -> Result<u64> {
    let value = value.trim();
    let parse = |suffix: &str, scale: u64| {
        value.strip_suffix(suffix).and_then(|n| n.trim().parse::<u64>().ok()).map(|n| n * scale)
    };
    parse("ms", 1)
        .or_else(|| parse("s", 1_000))
        .or_else(|| parse("m", 60_000))
        .or_else(|| value.parse::<u64>().ok().map(|n| n * 1_000))
        .with_context(|| format!("invalid duration '{value}' — use 500ms, 5s, or 1m"))
}
