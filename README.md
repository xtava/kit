# kit

One binary, many sharp tools. `kit` is a personal toolbelt — a single Rust binary
that hosts small utilities as plug-in subcommands over a shared framework, so each
new tool is *only* its own logic.

```
kit                        # list tools
kit cdp ready              # warm CDP debugger — is the workbench up? selected target + why
kit cdp tail --since 3s    # …correlated timeline of a running fleet
kit cdp eval 'location'    # …probe a live target (lazy-attaches, stays warm)
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
