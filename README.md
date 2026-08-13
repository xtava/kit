# kit

`kit` is a CLI toolkit distributed as a single Rust binary.

## Install

Download the Linux or macOS archive for your platform from
[GitHub Releases](https://github.com/xtava/kit/releases), put `kit` on your `PATH`, then keep it
current with:

```bash
kit update
```

`kit update` downloads the compatible release, verifies GitHub's SHA-256 asset digest, and replaces
the executable. Interactive Kit sessions notify you when a cached newer version is available.

Contributors can still build and install the current checkout with `./install.sh`.

## Commands

| Command | Purpose |
| --- | --- |
| `kit build` | Run repository-owned build workflows with supervised transcripts and a validated provider protocol. |
| `kit cdp` | Attach to running Chrome or Electron instances, inspect CDP timelines, interact with targets, assert behavior, trace code, and capture diagnostic bundles. |
| `kit scout` | Inspect Electron memory by instance and process role, with CDP target attribution and optional heap capture. |
| `kit stats` | Monitor CPU, memory, process trees, threads, and resources; safely terminate a selected process. |
| `kit monitor` | Inspect production health, metrics, logs, deployments, recurring costs, and source readiness through a read-only TUI or bounded JSON. |
| `kit diff` | Review staged, unstaged, and untracked Git changes inline or side by side; stage or unstage the selected file. |
| `kit domain` | Check domain registration through DNS, RDAP, and WHOIS, including aftermarket listings. |
| `kit deploy` | Run typed deployment plans, inspect version history, and execute configured rollbacks. |
| `kit ops` | Run named refs-only operations with validated public JSON input and 1Password-masked secrets. |
| `kit record` | Start, stop, inspect, save, and replay Modular Playwright recorder runs. |
| `kit render` | Read syntax-highlighted source or rich Markdown in the terminal and fuzzy-search supported workspace files. |
| `kit secrets` | Browse, search, and manage 1Password through a local TUI backed by the official `op` CLI. |
| `kit swarm` | Run deterministic multi-thread Codex councils and independently inspect them in a tree/detail TUI. |
| `kit tail` | Share pasted text and dragged files across Tailscale devices; receive, copy, save, open, or expire incoming items. |
| `kit console` | Connect to persistent terminal sessions and diagnose, install, or restart Console agents across Tailscale. |
| `kit sync` | Keep source projects aligned across Tailscale machines without sharing Git metadata, dependencies, or credentials. |
| `kit settings` | Edit tool-owned operator preferences in a shared TUI. |
| `kit update` | Verify and install the newest compatible GitHub release. |

Use built-in help for the current command surface:

```bash
kit --help
kit cdp --help
kit stats --help
```

Commands with headless output support `--json`. Interactive-only commands reject it.

## Examples

```bash
# Attach to a running Electron or Chrome instance
kit cdp ready
kit cdp snap
kit cdp errors --since 30s

# Launch an isolated browser and capture startup activity
kit cdp launch http://localhost:3000 --name app

# Act and verify in one daemon round trip
kit cdp do "click 'button:Save'; expect text 'Saved'; verify"

# Process and memory inspection
kit stats
kit stats --once
kit scout --once

# Production operations and agent-readable monitoring
kit monitor --config examples/monitor.toml
kit monitor --config examples/monitor.toml logs --level error --since 30m --limit 100
kit --json monitor --config examples/monitor.toml sources

# Git review
kit diff
kit diff --mode split

# Domain checks
kit domain example.com
kit domain --for-sale ink

# Deployment, recording, and Markdown
kit deploy --config examples/deploy.toml
kit record -i
kit render README.md

# Run the nearest repository build provider
kit build run check

# Start a Codex swarm, then inspect all runs independently
kit swarm run --detach "Review this architecture"
kit swarm

# Share text or dragged files with another Tailscale device
kit tail

# Diagnose or force-restart a remote Console agent
kit --json console status remote-mac
kit --json console restart remote-mac --force
```

### Scout live controls

Press `Ctrl-P` in `kit scout` to open its searchable command palette. Type to filter Scout's
actions, use the arrow keys to select, and press `Enter` to invoke. `Esc` or a click outside closes
the palette; it reflows automatically when the terminal is resized. Direct controls remain
available: `Up` / `Down` or `j` / `k` navigate, `Enter` / `Space` expands or collapses, `r`
refreshes, and `q` quits. Both routes invoke the same typed Scout action registry.

### Monitor searchable commands

Press `Ctrl-P` anywhere in `kit monitor`—including while editing a `/` filter—to search its full
contextual action catalog. Type part of a title, group, or action ID; use `Up` / `Down`,
`Ctrl-P` / `Ctrl-N`, or the mouse wheel to select; then press `Enter` or click a row to invoke it.
`Esc` and clicks outside the overlay close it.

The palette includes **Inspect**, **Refresh**, **Open provider**, **Open in kit deploy**, **Toggle log
follow**, and trace, metrics, and deployment correlation commands. It projects Monitor's existing
typed registry, so the same enablement and exact captured target apply to palette, keyboard, inline,
and context-menu actions. Commands unavailable for the current selection remain discoverable and
explain the missing capability instead of silently retargeting another item.

Examples:

- From Overview, search `refresh` to update the active scope.
- Select a service and search `inspect` to focus its inspector.
- In Logs with a ready Loki source, search `follow` to start or pause three-second refreshes.
- Search `provider`, `deploy`, or `trace` to discover whether the selected source publishes that
  handoff.

## Documentation

- [Contribution guide](./CONTRIBUTING.md)
- [Build provider client](./docs/build.md)
- [CDP debugger](./docs/cdp.md)
- [CPU and process monitor](./docs/stats.md)
- Production operations monitor controls are documented above and in the built-in `?` help.
- [Git diff viewer](./docs/diff.md)
- [Deployment plans and rollback](./docs/deploy.md)
- [Modular recorder integration](./docs/record.md)
- [Source and Markdown viewer](./docs/render.md)
- [1Password secrets client](./docs/secrets.md)
- [Codex swarm orchestrator](./docs/swarm.md)
- [Tailscale sharing](./docs/tail.md)
- [Synced Projects](./docs/canonical/sync.md)
- [Console lifecycle and recovery](./docs/canonical/console.md)
- [Canonical engineering documentation](./docs/canonical/README.md)
- [Development guide](./docs/dev-guide.md)
- [CDP testbed](./testbed/README.md)
- [Agent skill installation](./skills/README.md)

## Development

Read the [contribution guide](./CONTRIBUTING.md) before adding a tool or shared capability. Reuse is
an architectural requirement: tools own domain policy, while reusable mechanics belong in the
shared `framework`, `tui`, or `cdp` owners.

```bash
cargo check -j 2
cargo run -- scout --once
cargo test -j 2
cargo clippy -j 2 --all-targets
cargo fmt
```

Reusable code lives in `src/lib.rs`; `src/main.rs` only registers commands. The dependency direction
is:

```text
tools/* -> framework | tui | cdp
```

Tools do not depend on other tools. Shared terminal behavior belongs in `src/tui/`; shared CDP
protocol behavior belongs in `src/cdp/`.
