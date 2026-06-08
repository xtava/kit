# kit

One binary, many sharp tools. `kit` is a personal toolbelt — a single Rust binary
that hosts small utilities as plug-in subcommands over a shared framework, so each
new tool is *only* its own logic.

```
kit                       # list tools
kit scout                 # live Electron memory recon   (TUI when interactive)
kit scout --once          # …one survey, headless table
kit scout --json          # …headless, machine-readable
kit scout dive            # capture a window's heap snapshot → memlab
kit domain ari.io studio  # authoritative domain checker
kit domain                # …its TUI
```

Every tool inherits the same spine for free: a global `--json`, headless-vs-TUI
dispatch from a single `Context`, XDG config, and a panic-safe TUI harness. Adding
a tool is a module under `src/tools/` plus one `register()` line.

## Layout

One crate; the layers are modules, and the module dependency direction *is* the
architecture (`tools → framework`/`tui`, never tool↔tool, never `framework → tui`):

```
src/
├─ main.rs            Registry::new().register(scout::tool()).register(domain::tool()).dispatch()
├─ framework/         Tool · Context · Output · ConfigStore · Registry   — the spine (no UI deps)
├─ tui/               Session (panic-safe terminal) · EventReader · LineEditor · CommandSet
└─ tools/
   ├─ scout/          proc · cdp · survey · correlate · report · tui · dive
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
