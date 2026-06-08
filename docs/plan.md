# kit — plan

One binary, many tools. `kit` is a personal toolbelt: a single Rust binary that
hosts small, sharp utilities as plug-in subcommands over a shared framework.

```
kit scout                 # live Electron memory recon  (TUI when interactive)
kit scout --json          # …headless, machine-readable
kit domain ari.com io     # authoritative domain checker
kit domain                # …its TUI
kit                       # list tools
```

The seed tools — `scout` (memory recon) and `domain` (domain checker) — are **the
same program written twice**: clap + `--json` headless, a TUI when the terminal is
interactive, a tokio "engine" of async probes, a `Serialize` result model, XDG
config, a slash-command line. `kit` extracts that shared spine once, so the next
tool is *only* its own logic.

For working in the repo — efficient builds, the edit→run loop, code practices —
see [dev-guide.md](./dev-guide.md).

---

## 1. The framework — `framework` and `tui`

The shared spine is two modules. `framework` is logic with no UI dependency; `tui`
is the interactive layer on top of it.

### 1.1 The `Tool` plug-in contract (`framework`)

A tool is one value implementing `Tool` — three methods, no defaults. Registering
it is one line; the binary never learns a tool's flags.

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn meta(&self) -> ToolMeta;          // name, about, version → dispatch + help
    fn command(&self) -> clap::Command;   // its args, mounted under `kit <name>`
    async fn run(&self, cx: &Context, m: &ArgMatches) -> Result<()>;
}

pub struct ToolMeta { pub name: &'static str, pub about: &'static str, pub version: &'static str }
```

Tools keep clap's `#[derive]` ergonomics *and* the thin-binary plug-in shape via
`CommandFactory`/`FromArgMatches`:

```rust
#[derive(Parser)] struct ScoutArgs { #[arg(long)] once: bool /* … */ }

fn command(&self) -> Command { ScoutArgs::command() }          // derive → Command
// in run(): let args = ScoutArgs::from_arg_matches(m)?;       // ArgMatches → derive
```

There is no `tui()` method. A tool decides headless-vs-interactive *inside* its own
`run`, reading `cx.term.interactive()` and reaching for the `tui` harness when it
goes interactive — `scout` opens its TUI on a bare `kit scout`, `domain` on a bare
`kit domain`; both fall back to headless when piped, given `--json`, or passed args.

### 1.2 The `Context` — shared services, passed by reference (`framework`)

There is no DI container. `main` builds one `Context` (the composition root) and
borrows `&Context` into every tool. Resolution is field access; lifetimes are the
borrow checker's job.

```rust
pub struct Context {
    pub config: ConfigStore,   // XDG dirs, per-tool namespaced load/save
    pub out:    Output,        // text-vs-json, color, width — honors tty + `--json`
    pub term:   Terminal,      // tty detection, size
}
```

Tools pull what they need: `cx.config.load::<DomainConfig>("domain")`,
`cx.out.json(&results)` / `cx.out.is_json()`. Where a tool needs to *swap* an
implementation (real network vs a test fake), the pattern is a `trait` at that one
seam — not a container, and not everywhere.

### 1.3 The TUI harness (`tui`)

Both TUIs need the same scaffolding, and writing it twice is how you ship the
"a panic left the terminal in raw mode" bug. `tui` provides the components — not a
forced event loop, since tools own their `tokio::select!` loop (their async sources
differ: domain awaits query results, scout awaits resurveys):

```rust
pub struct Session     { /* RAII terminal: raw mode + alt-screen, restored on Drop — panic-safe */ }
pub struct EventReader { /* crossterm events bridged onto an async channel */ }
pub struct LineEditor  { /* a UTF-8-aware single-line input */ }
pub struct CommandSet  { /* slash-command parse · fuzzy-suggest · tab-complete */ }
```

The `Session` guard enters raw mode + alt-screen and **restores on `Drop`** (and via
a panic hook) — so a panic still leaves a clean terminal. `CommandSet` is generalized
from `domain/command.rs` (`CommandSpec { name, aliases, usage, description }`): each
tool supplies its commands as a `const`, the widget parses/suggests/completes them.

---

## 2. Layout — one crate, modules as the DAG

One crate, one `Cargo.toml`, every tool always built. The module dependency
direction *is* the architecture.

```
kit/
├─ Cargo.toml                # one manifest
├─ .cargo/config.toml        # fast linker (see dev-guide)
├─ rust-toolchain.toml       # pin stable
├─ README.md · docs/{plan.md, dev-guide.md}
└─ src/
   ├─ main.rs                Registry::new().register(scout::tool()).register(domain::tool()).dispatch()
   ├─ framework/             Tool · Context · Output · ConfigStore · Registry   (no UI deps)
   ├─ tui/                   Session · EventReader · LineEditor · CommandSet     (ratatui)
   └─ tools/
      ├─ scout/              ScoutTool + proc/ · cdp/ · survey · correlate · report · tui · dive
      └─ domain/             DomainTool + engine/{dns,rdap,whois} · config · report · tui

INVARIANT (the architecture):
  tools      use  framework / tui      ✔
  tools      use  tools::<other>       ✗ never   (tools are blind to each other)
  framework  use  tui                  ✗ never   (the spine stays UI-free)
```

Dispatch rule (generalized from `domain/src/main.rs:46`): a subcommand invoked
with **no args while stdin+stdout are ttys → `tool.tui(cx)`**; otherwise
`tool.run(cx, matches)`. `kit` alone → list tools.

We start single-crate deliberately; the workspace is a documented graduation
target, taken only when `cargo check` time demands it. See the dev-guide.

---

## 3. Efficient builds

Covered in full in [dev-guide.md](./dev-guide.md). The levers, by impact: a fast
**linker** (lld now, mold next — the link is the non-incremental floor), a
**`cargo check` watch loop** for feedback, and a lean **dev profile**. No feature
flags — this is a dev tool; every tool always builds, and dependencies cache.

---

## 4. Migration — cutover, no duplicates

Both seed tools move in as the single source of truth; the standalones retire.

- **domain** → `src/tools/domain/`. `engine/{dns,rdap,whois}`, `config.rs`,
  `tui.rs`, `command.rs` move nearly verbatim. Its `main.rs` plumbing
  (tty-dispatch, `--json`, stdin tokens, text formatting) is **deleted** — that
  logic now lives once in `framework`/`tui`. What remains is `DomainTool` (its args +
  `run`/`tui`). `kit domain` reaches parity, then the `domain` repo is retired.
- **scout** → `src/tools/scout/`. `model.rs` + the `proc/` plane (already built)
  move in; the remaining phases (cdp, correlate, report, tui, dive) continue there.
- **install**: `cargo install --path .` → `~/.cargo/bin/kit`. The old
  `~/.cargo/bin/domain` is removed (cutover). Muscle memory, if wanted, is a shell
  `alias domain='kit domain'` — not a second binary.

---

## 5. Build order

- **P0 · scaffold.** One crate, `.cargo/config.toml` (linker), `rust-toolchain`,
  dev profile, empty `framework`/`tui`/`tools` modules — `kit` prints an (empty) tool list.
- **P1 · framework core.** `Tool`/`ToolMeta`/`Registry`, `Context`
  (config·output·term), dispatch (headless-vs-tui), `kit` / `kit help` /
  `kit <tool> --help`.
- **P2 · tui layer.** `Session` (RAII + panic-safe restore), `EventReader`,
  `LineEditor`, and the `CommandSet` widget generalized from `domain`.
- **P3 · absorb domain.** Port engine + tui into `DomainTool`; `kit domain`
  reaches parity with today's binary; delete the standalone.
- **P4 · finish scout.** Move model+proc; build cdp → correlate → report → tui →
  dive as `ScoutTool`; `kit scout` end-to-end, validated against live instances.
- **P5 · polish.** README, per-tool config namespaces, `kit completions zsh`.

---

## 6. Adding the next tool (the payoff)

```
src/tools/clip/
  mod.rs            pub fn tool() -> impl Tool { ClipTool }
  {run,tui}.rs      the only tool-specific code
```
…then one line in `main.rs`: `.register(clip::tool())`. Headless/JSON, tty
dispatch, the TUI harness, config — all inherited. Every existing tool: untouched.
