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
| `kit diff` | Review staged, unstaged, and untracked Git changes inline or side by side; stage or unstage the selected file. |
| `kit domain` | Check domain registration through DNS, RDAP, and WHOIS, including aftermarket listings. |
| `kit deploy` | Run typed deployment plans, inspect version history, and execute configured rollbacks. |
| `kit record` | Start, stop, inspect, save, and replay Modular Playwright recorder runs. |
| `kit render` | Read Markdown in the terminal and fuzzy-search Markdown files in the current workspace. |
| `kit secrets` | Browse, search, and manage 1Password through a local TUI backed by the official `op` CLI. |
| `kit swarm` | Run deterministic multi-thread Codex councils and independently inspect them in a tree/detail TUI. |
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
```

## Documentation

- [Contribution guide](./CONTRIBUTING.md)
- [Build provider client](./docs/build.md)
- [CDP debugger](./docs/cdp.md)
- [CPU and process monitor](./docs/stats.md)
- [Git diff viewer](./docs/diff.md)
- [Deployment plans and rollback](./docs/deploy.md)
- [Modular recorder integration](./docs/record.md)
- [Markdown viewer](./docs/render.md)
- [1Password secrets client](./docs/secrets.md)
- [Codex swarm orchestrator](./docs/swarm.md)
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
