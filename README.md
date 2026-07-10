# kit

One binary, many sharp tools. `kit` is a personal toolbelt — a single Rust binary
that hosts small utilities as plug-in subcommands over a shared framework, so each
new tool is *only* its own logic.

```
kit                        # list tools
kit cdp launch http://localhost:3000 --name app
                           # controlled Chrome launch with startup capture
kit cdp ready              # warm CDP debugger — is the workbench up? selected target + why
kit cdp tail --since 3s    # …correlated timeline of a running fleet
kit cdp brief --since 30s  # …agent-safe compact timeline, with omission counts
kit cdp eval 'location'    # …probe a live target (lazy-attaches, stays warm)
kit cdp click 'button:Save'  # …drive it by accessible name; waits for the dust to settle
kit cdp verify             # …PASS/FAIL since that click: ready · errors · failed net
kit cdp snap --diff        # …what changed on screen since the last snap
kit cdp do "click 'button:Save'; expect text 'Saved'; verify"
                           # …a whole verification sequence in one round trip
kit cdp flow run smoke --record
                           # …the same sequence, saved in .kit/cdp/flows/ — recorded to an mp4
kit cdp shot               # …timestamped screenshot, never overwrites
kit cdp watch add cart 'cart.items.length'
                           # …subscribe to a value; changes land on the timeline
kit cdp ext doctor <id>    # …diagnose a Modular extension view/webview runtime
kit scout                  # live Electron memory recon   (TUI when interactive)
kit scout --once           # …one survey, headless table
kit scout dive             # capture a window's heap snapshot → memlab
kit domain example.com io  # authoritative domain checker
kit domain                 # …its TUI
```

Every tool inherits the same spine for free: a global `--json`, headless-vs-TUI
dispatch from a single `Context`, XDG config, and a panic-safe TUI harness. Adding
a tool is a module under `src/tools/` plus one `register()` line.

For the CDP debugger, see **[docs/cdp.md](./docs/cdp.md)**. It covers the warm
attach flow, the controlled launcher flow (startup capture, state/marks, network
rules, redacted bundles, profiles, cleanup), and the verification loop: locators,
settle, `wait`/`expect`/`verify`, `do` batches, flows, watches, and `snap --diff`.
The live playground every feature is verified against lives in
[`testbed/`](./testbed/README.md).

Coding agents get all of this as a canonical [Agent Skill](https://agentskills.io)
in [`skills/kit-cdp/`](./skills/kit-cdp/SKILL.md) — auto-discovered by Claude Code
and Codex inside this repo, installable anywhere else (the repo doubles as a
Claude Code plugin). See [`skills/README.md`](./skills/README.md).

## Layout

One crate; the layers are modules, and the module dependency direction *is* the
architecture (`tools → framework | tui | cdp`, never tool↔tool, never `spine → tools`):

```
src/
├─ main.rs            Registry::new().register(cdp::tool()).register(scout::tool())….dispatch()
├─ framework/         Tool · Context · Output · ConfigStore · Registry   — the spine (no UI deps)
├─ tui/               Session (panic-safe terminal) · EventReader · LineEditor · CommandSet
├─ cdp/               Chrome DevTools Protocol engine — client · discovery · target · timeline
└─ tools/
   ├─ cdp/            warm CDP debugger — daemon · client · snapshot · readiness · lenses
   ├─ scout/          proc · cdp (via kit::cdp) · survey · correlate · report · tui · dive
   └─ domain/         engine{dns,rdap,whois} · config · report · tui
```

A tool implements one trait — `Tool { meta, command, run }` — and decides
headless-vs-interactive inside its own `run` from `cx.term`. The binary never
learns a tool's flags.

## Develop

```bash
cargo watch -x check     # tight feedback loop
cargo run -- scout       # run a tool
cargo install --path .   # → ~/.cargo/bin/kit
```

Efficient builds (fast linker, the edit→run loop, when to split crates) and code
practices are in **[docs/dev-guide.md](./docs/dev-guide.md)**. The architecture and
roadmap are in **[docs/plan.md](./docs/plan.md)**.
