# kit — dev guide

How to work in this repo: the build model, the edit→run loop, and the few code
practices that keep it clean. For *what* we're building and why, see
[plan.md](./plan.md).

---

## One-time setup

Two files make builds fast. Apply them once; they're committed, so a fresh clone
inherits them.

### 1. A fast linker (`.cargo/config.toml`)

The final **link** runs on every rebuild and is *not* incremental — so the linker
is the single biggest lever on edit→run time. The default GNU `ld` is the slow
one. Use `lld` (already on this machine) or `mold` (fastest, one install away).

```toml
# .cargo/config.toml
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=lld"]
# after `sudo pacman -S mold`, swap lld → mold for the last bit of speed.
```

### 2. A lean dev profile (`Cargo.toml`)

Less debug info = less for the linker to chew on, without losing backtraces.

```toml
[profile.dev]
debug = "line-tables-only"     # keeps file:line in backtraces, drops the heavy DWARF
split-debuginfo = "unpacked"   # Linux: debug info stays out of the link step
```

Optional — optimize *dependencies* once (cached forever) while keeping your own
crate at `opt-level = 0` for fast rebuilds. Worth it only if a tool is CPU-bound:

```toml
[profile.dev.package."*"]
opt-level = 2
```

---

## The build model (why the loop is shaped the way it is)

`cargo build` is three phases that cache differently:

```
front-end   type-check · macro-expand · borrow-check   → cached per-query (incremental DB)
codegen     MIR → LLVM IR → .o files                   → cached per codegen-unit (only changed CGUs rebuild)
link        stitch .o + deps → the binary              → NOT incremental, runs whole every time
```

Consequences that shape how you work:

- **Dependencies compile once.** Editing a tool never rebuilds `tokio`/`reqwest`.
  Never run `cargo clean` to "fix" a build — you only throw away the cache.
- **The link is the floor.** That's why the linker (above) matters most.
- **`check` skips codegen *and* link.** So the tight feedback loop is a
  check-on-save watcher; you only `run` when you actually want to execute.

---

## The loop

```bash
cargo watch -x check        # tight loop: instant type errors on every save (cargo-watch is installed)
cargo run -- scout          # run a tool   (kit scout)
cargo run -- domain example.io  # run a tool with args
cargo test                  # unit tests (engine logic — no network, no /proc needed)
cargo clippy --all-targets  # lints
cargo fmt                   # format
./install.sh                # install the managed binary at ~/.local/bin/kit
```

`bacon` is a nicer TUI version of the check loop if you ever want it
(`cargo install bacon`); `cargo watch -x check` covers it today.

---

## Code practices

The handful that matter for a dev tool — no enterprise ceremony.

- **`lib.rs` + a thin `main.rs`.** All logic lives in the library; `main` only
  parses argv and dispatches. This is what makes the engine testable without a
  terminal.
- **Keep each tool's engine pure.** The probe logic (`scout`'s `proc`, the shared
  `cdp` engine, `domain`'s `dns`/`rdap`/`whois`) takes data and returns data — no
  framework, no globals. That's what lets a unit test run it with zero network and
  zero `/proc`. UI and I/O live in the thin shell around it (`run`, `tui`).
- **Errors: `anyhow` in the app, `thiserror` in the engine.** The engine returns
  precise typed errors; the app layer flattens them into `anyhow::Result` for
  reporting.
- **The module DAG is the architecture.** `tools/* → framework | tui | cdp`, and
  **never** `tools/a → tools/b`. Tools are blind to each other; the spine modules
  never reach up into `tools`. `cdp` (the Chrome DevTools Protocol engine) is a peer
  capability both `scout` and `cdp`-the-tool build on — see `docs/adr/0001`. (Full
  picture in [plan.md](./plan.md).)
- **Doc-comment the public surface.** `///` on the types and `pub` items that
  carry meaning; let good names and shapes do the rest. No narration inside
  function bodies.

### Testing external commands

Use `framework::process::test_support::CommandFixture` when a tool shells out to
another executable. The fixture is a real, portable process, so production code
still crosses the canonical `ProcessSupervisor` boundary while tests control its
arguments, stdin, output events, delays, exit status, and lifetime.

- Define exact argument matches with `respond`; inspect completed calls through
  `invocations` or await a live one with `wait_for_invocation`.
- Model streaming and cancellation explicitly with ordered `OutputEvent`s and
  `CommandResponse::hang`; do not generate ad hoc shell scripts in tool tests.
- Use `record_commands` only to bootstrap a fixture from a real executable. Raw
  captures stay untracked under `target/command-recordings`; only the sanitized
  JSON scenario is written to the requested repository path.
- Recording closes stdin and captures static stdout/stderr. Add secret- and
  machine-specific values to `RecordingPolicy` before recording, then model any
  timing, interleaving, or hanging behavior manually.

This keeps mocks at the operating-system boundary, reusable across tools, and
honest about the process behavior they exercise.

---

## When to graduate to a workspace

We start as **one crate** on purpose — least ceremony, fastest iteration, and the
module layout already *is* the architecture. Codegen and link scale fine in one
crate (per-CGU incremental + a fast linker). The one thing that doesn't parallelize
within a single crate is **whole-crate type-checking** — `cargo check` time grows
with crate size.

So the graduation signal is concrete: **when `cargo check` starts to drag**, split
a tool into its own crate (a workspace checks crates in parallel). Until then,
splitting buys nothing. The extraction is mechanical — `src/tools/scout/` →
`crates/kit-scout/`, flip `pub(crate)` → `pub`, add a manifest — because the
modules already obey the dependency DAG. You are never trapped by starting simple.
