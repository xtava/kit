# kit

`kit` is a CLI toolkit distributed as a single Rust binary.

## Install

```bash
git clone https://github.com/xtava/kit.git
cd kit
cargo install --locked --path .
```

Run `kit update` to rebuild and reinstall from the same checkout. It uses the checkout as-is and
does not pull or modify Git state.

## Commands

| Command | Purpose |
| --- | --- |
| `kit cdp` | Attach to running Chrome or Electron instances, inspect CDP timelines, interact with targets, assert behavior, trace code, and capture diagnostic bundles. |
| `kit scout` | Inspect Electron memory by instance and process role, with CDP target attribution and optional heap capture. |
| `kit stats` | Monitor CPU, memory, process trees, threads, and resources; safely terminate a selected process. |
| `kit diff` | Review staged, unstaged, and untracked Git changes in unified or split mode; stage or unstage the selected file. |
| `kit domain` | Check domain registration through DNS, RDAP, and WHOIS, including aftermarket listings. |
| `kit deploy` | Run typed deployment plans, inspect version history, and execute configured rollbacks. |
| `kit record` | Start, stop, inspect, save, and replay Modular Playwright recorder runs. |
| `kit render` | Read Markdown in the terminal and fuzzy-search Markdown files in the current workspace. |
| `kit update` | Rebuild and reinstall Kit from its source checkout. |

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
```

## Documentation

- [CDP debugger](./docs/cdp.md)
- [CPU and process monitor](./docs/stats.md)
- [Git diff viewer](./docs/diff.md)
- [Deployment plans and rollback](./docs/deploy.md)
- [Modular recorder integration](./docs/record.md)
- [Markdown viewer](./docs/render.md)
- [Development guide](./docs/dev-guide.md)
- [CDP testbed](./testbed/README.md)
- [Agent skill installation](./skills/README.md)

## Development

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
