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

Read [CONTRIBUTING.md](./CONTRIBUTING.md) before non-trivial changes. It defines the reuse-first
workflow: tools own domain policy and presentation, shared modules own reusable mechanics, and
cross-tool behavior must have one canonical owner rather than copied per-tool implementations.

## Durable Development Logs

Development ledgers under `sessions/` are opt-in. Load and use the project-local `kit-dev-log`
skill only when the user explicitly asks for a durable development log, handoff, resume record, or
another persistent task ledger. Do not create or update a session ledger merely because work spans
multiple turns. Never record secrets, authentication URLs, private terminal contents, or chat
transcripts. See [Development logs](./docs/canonical/dev-logs.md) for the ledger contract when it is
explicitly requested.

Task lists, TODO inventories, and execution plans are likewise opt-in. Create or maintain them only
when the user explicitly asks for planning or a task-list artifact; otherwise make the requested
change directly and report the relevant verification.

## Build, Test, and Development Commands

- `cargo watch -x check`: fast edit loop for type and borrow-checking.
- `cargo run -- scout` or `cargo run -- domain example.io`: run a tool locally.
- `cargo test`: run unit and integration tests.
- `cargo clippy --all-targets`: lint all targets; keep it warning-free.
- `cargo fmt`: format using the repository `rustfmt.toml`.
- `./install.sh`: install the current binary at `~/.local/bin/kit`.

## Resource-Conservative Cargo Workflow

Treat CPU, memory, disk I/O, and build concurrency as shared machine resources. Verification should
be proportional to the change and should never make Kit's own development work the hottest workload
on the machine.

- Run only one heavy Cargo operation at a time. Never overlap `cargo test`, `cargo clippy`, release
  builds, benchmarks, or `cargo install`, and never launch them in parallel workers.
- Do not chain heavy Cargo operations with `&&` in one shell command. Run one command, inspect and
  report its result, then decide whether the next command is still necessary.
- Before starting a heavy build, check for an existing Cargo or rustc build. Do not compete with a
  build already running for the user or another agent.
- Default to at most two build jobs for non-trivial work: use `cargo check -j 2`, targeted
  `cargo test -j 2 <test-or-module>`, `cargo clippy -j 2`, and `./install.sh`.
- Start with the cheapest relevant proof: `cargo check -j 2` or a targeted test. Run the full
  `cargo test -j 2` suite only once near handoff when the change surface warrants it.
- Run clippy separately and only after targeted checks pass. Do not run full tests, clippy, and an
  install back-to-back unless the user explicitly asks for exhaustive verification.
- Install only once, after the implementation is settled and the necessary checks have passed.
  Do not repeatedly run `./install.sh` after each visual or code iteration.
- Release builds, ignored performance benchmarks, and 30-second sampling gates are opt-in heavy
  checks. Run them only when performance or release behavior is actually in scope, and announce
  them before starting.
- If a heavy command is interrupted or aborted, verify that its Cargo/rustc process tree has exited
  before starting another build.
- For a user-requested build/install, prefer one targeted check followed by one bounded-job install;
  do not silently expand the request into the full test, clippy, benchmark, and release matrix.

## Coding Style & Naming Conventions

Use Rust 2021 and the house rustfmt style: 100 columns with `use_small_heuristics = "Max"`. Group imports as standard library, external crates, then crate modules, sorted within each group. Prefer descriptive names such as `fetch_targets` over vague names like `get_data`.

Keep engines pure and testable: parsing, probing, grouping, and protocol logic should take data and return data. Terminal, process, network, and daemon concerns belong in thin shell layers such as `run`, `tui`, or daemon modules. Use `thiserror` for engine errors and `anyhow::Result` with context at application boundaries.

## Testing Guidelines

Put focused unit tests near pure engine code and broader behavior tests in `tests/`. Name tests by behavior, for example `errors_view_collapses_real_duplicate_errors`. CDP integration tests may skip when no Chrome binary is present; still run `cargo test` before handing off changes. For `kit cdp` daemon changes, also exercise a live command and detach stale daemons with `kit cdp detach --all`.

For `kit stats`, agents must use the canonical headless verification path in
[`docs/canonical/stats-headless-verification.md`](./docs/canonical/stats-headless-verification.md).
Do not launch a terminal window, PTY verifier, or interactive Stats session unless the user
explicitly requests interactive validation.

## Commit & Pull Request Guidelines

History uses short scoped subjects such as `cdp: fix dead timeline scroll` and `chore: codify the house rustfmt style`. Follow that pattern: `<scope>: <imperative summary>`.

Pull requests should describe the behavior change, list verification commands, and include terminal output or screenshots for TUI/CDP user-facing changes. Link related issues when available and call out any skipped live checks.
