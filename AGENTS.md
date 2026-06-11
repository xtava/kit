# Repository Guidelines

## Project Structure & Module Organization

`kit` is one Rust crate and one binary. `src/main.rs` only wires the registry; reusable logic lives in `src/lib.rs`.

- `src/framework/`: shared tool spine (`Tool`, `Context`, output, config, registry).
- `src/tui/`: terminal/session helpers shared by interactive tools.
- `src/cdp/`: Chrome DevTools Protocol engine used by multiple tools.
- `src/tools/`: subcommands (`cdp`, `scout`, `domain`). Keep tools independent of each other.
- `tests/`: integration tests; `tests/cdp_errors.rs` exercises live CDP behavior when Chrome is available.
- `docs/` and `examples/`: contributor notes, CDP docs, and runnable examples.

The dependency direction is architectural: `tools/* -> framework | tui | cdp`; never `tools/a -> tools/b`.

## Build, Test, and Development Commands

- `cargo watch -x check`: fast edit loop for type and borrow-checking.
- `cargo run -- scout` or `cargo run -- domain example.io`: run a tool locally.
- `cargo test`: run unit and integration tests.
- `cargo clippy --all-targets`: lint all targets; keep it warning-free.
- `cargo fmt`: format using the repository `rustfmt.toml`.
- `cargo install --path .`: install the current binary as `kit`.

## Coding Style & Naming Conventions

Use Rust 2021 and the house rustfmt style: 100 columns with `use_small_heuristics = "Max"`. Group imports as standard library, external crates, then crate modules, sorted within each group. Prefer descriptive names such as `fetch_targets` over vague names like `get_data`.

Keep engines pure and testable: parsing, probing, grouping, and protocol logic should take data and return data. Terminal, process, network, and daemon concerns belong in thin shell layers such as `run`, `tui`, or daemon modules. Use `thiserror` for engine errors and `anyhow::Result` with context at application boundaries.

## Testing Guidelines

Put focused unit tests near pure engine code and broader behavior tests in `tests/`. Name tests by behavior, for example `errors_view_collapses_real_duplicate_errors`. CDP integration tests may skip when no Chrome binary is present; still run `cargo test` before handing off changes. For `kit cdp` daemon changes, also exercise a live command and detach stale daemons with `kit cdp detach --all`.

## Commit & Pull Request Guidelines

History uses short scoped subjects such as `cdp: fix dead timeline scroll` and `chore: codify the house rustfmt style`. Follow that pattern: `<scope>: <imperative summary>`.

Pull requests should describe the behavior change, list verification commands, and include terminal output or screenshots for TUI/CDP user-facing changes. Link related issues when available and call out any skipped live checks.
