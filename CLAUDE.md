# kit — working agreement

`kit` is a single Rust binary that hosts small, sharp tools as plug-in
subcommands over a shared spine. The architecture, the build loop, and the
ubiquitous language live elsewhere — read them first, don't re-derive them:

- `docs/plan.md` — what we're building and the module DAG.
- `docs/dev-guide.md` — the build model and the edit→run loop.
- `CONTEXT.md` — the glossary (a term used loosely is a bug).
- `docs/adr/` — decisions that were hard to reverse; honor them or supersede them.

This file is the one thing those don't cover: how the code itself must read.

## The bar

Beautiful, first. Code a principal engineer reopens in six months and still
likes. "It compiles and the tests pass" is the floor, not the goal. If a shape
is ugly, the shape is wrong — fix it, don't comment around it.

## Hygiene

**The module DAG is law.** `tools/* → framework | tui | cdp`. A tool never reaches
into another tool; the spine never reaches up into `tools`. If a tool needs
another tool's capability, the capability is in the wrong place — lift it into a
peer module (that is why `cdp` is a peer, not a child of `scout`). See ADR-0001.

**Engines are pure; shells do I/O.** The probe logic takes data and returns data
— no terminal, no globals, no network baked in. That is what lets a unit test run
it with zero `/proc` and zero sockets. `run`/`tui`/the daemon are the thin I/O
shell around a pure core.

**Names are the documentation.** `fetch_targets` over `get_data`. If a function
can't be named in five words it does too much — split it. Rename freely, including
in files you weren't asked to touch, when a better name is obvious.

**Comments explain *why*, never *what*.** The code already says what it does. A
comment earns its place only for a reason the code can't carry: a protocol quirk
(`// the DevTools frontend is the debugger's own reflection — never capture it`),
a concurrency hazard (`// keeps the lock off the await`), a deliberate trade-off.
Delete any comment that restates the line below it. No commented-out code, ever.

**`///` the public surface, sparingly.** A doc-comment on each `pub` type and the
`pub` items that carry meaning — one line that says what it *is*, not how it works.
No narration inside private function bodies.

**Errors: `anyhow` in the app, `thiserror` in the engine.** The engine returns
precise typed errors; the shell flattens them into `anyhow::Result` with
`.context(...)` at the boundary, so a failure reads as a sentence.

**No untyped JSON across a boundary.** `serde_json::Value` is fine *inside* the CDP
decode layer (the protocol is dynamic there); the moment data leaves the engine it
is a named `Serialize` type. No fields named `data`, `info`, `meta`, `payload`.

**Model nullability honestly.** If a field can be absent, it's an `Option` with a
reason — not a sentinel, not a silent default that hides the question.

**Delete on sight.** Dead code is a lie about the system. Unused imports, "just in
case" helpers, legacy fallbacks, half-finished branches — gone the moment you see
them. The best diff is a negative one. Cut over completely: no shims, no
re-exports, no two implementations of one concern coexisting.

**Visual rhythm.** One blank line between unrelated blocks. Imports grouped std /
external / crate, sorted within each. No trailing whitespace. `cargo fmt` is not
optional.

## Before you call it done

Every change, not just the big ones:

```
cargo clippy --all-targets    # zero warnings
cargo test                    # green
cargo fmt                     # clean
cargo install --path .        # the binary the user runs is the code you wrote
```

A `kit cdp` change isn't done until it's been run against a live Instance — the
daemon is stateful and survives reloads; a green test suite does not prove the
warm path works. Detach the stale daemon (`kit cdp detach --all`) after a daemon
change so the next command spawns the new binary.
