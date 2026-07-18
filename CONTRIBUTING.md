# Contributing to Kit

Kit is distributed as one binary, but it should not become a collection of isolated commands.
Every contribution should leave behind capabilities that other tools can reuse when the underlying
behavior is genuinely shared.

The governing rule is:

> Tools own domain policy and presentation. Shared modules own reusable mechanics.

Reuse is not achieved by making a helper `pub`, adding callbacks to tool-specific code, or copying
an implementation into a second tool. Reusable code has one canonical owner, depends on no tool,
uses domain-neutral inputs and outputs, and is exercised through the modules that need it.

## Architecture and ownership

The dependency direction is a hard boundary:

```text
tools/* -> framework | tui | cdp
```

`tools/a` must never depend on `tools/b`. If two tools need the same behavior, move that behavior to
the appropriate shared owner and cut both tools over to it.

| Concern | Canonical owner | Examples |
| --- | --- | --- |
| Tool registration, configuration, output, and durable state mechanics | `src/framework/` | `Tool`, `ConfigStore`, `AtomicFileWriter` |
| Terminal interaction and reusable UI behavior | `src/tui/` | sessions, input, navigation, suggestions, fuzzy indexing, themes |
| Chrome DevTools Protocol behavior | `src/cdp/` | discovery, targets, timelines, source maps |
| Command-specific discovery, policy, orchestration, and presentation | `src/tools/<tool>/` | Markdown discovery in Render, deployment policy in Deploy |

A substantial capability that is neither generic framework code nor terminal behavior may deserve a
new peer module beside `framework`, `tui`, and `cdp`. Create one only when the capability has a clear
domain name, more than one real consumer, and an API that does not depend on a tool.

Shared modules must not import from `src/tools/`. A tool may adapt a shared result into its own
labels, commands, configuration, and workflow; the shared owner must not learn those policies.

## Reuse-first workflow

Before implementing a non-trivial feature:

1. Search `src/framework/`, `src/tui/`, `src/cdp/`, and sibling tools for an existing owner or a
   repeated implementation.
2. Separate the request into domain policy and reusable mechanics. Write down which side owns each
   piece before editing.
3. Reuse or extend the canonical shared owner. Do not add a second implementation because its API is
   slightly inconvenient.
4. Design shared APIs around typed data and explicit capabilities, not the vocabulary or state of
   the first caller.
5. Keep filesystem, terminal, process, network, and daemon effects at thin boundaries. Keep parsing,
   ranking, grouping, and state transitions pure and testable where possible.
6. Integrate the shared capability into every current caller that needs it and delete the displaced
   local implementations in the same change.
7. Test the shared invariant at its owner, then test each tool's policy adapter separately.

When only one caller exists, do not manufacture a speculative framework. A shared-first design is
still justified when the boundary is an inherent Kit capability and can be proven independently of
the first tool with domain-neutral types and tests. Otherwise, keep the code local until a second
real use reveals the correct abstraction.

## What reusable code looks like

Good shared code:

- accepts caller-owned typed values instead of reaching into a tool's state;
- returns data or explicit state transitions instead of rendering tool-specific text;
- exposes one canonical lifecycle and source of truth;
- supports composition without boolean flags for every caller;
- has precise errors (`thiserror` in engines, contextual `anyhow` at application boundaries);
- documents meaningful public types and invariants;
- has tests that use more than one domain shape when genericity is part of the contract.

Bad shared code:

- imports a tool module or mentions one tool throughout a supposedly generic API;
- hides copied logic behind a differently named wrapper;
- mirrors state in a second cache, registry, or persistence path;
- accumulates optional callbacks and mode flags that reconstruct tool-specific behavior;
- is added "for later" without a current invariant or consumer;
- leaves old and new implementations active at the same time.

For example, `src/tui/search.rs` owns typed fuzzy indexing and generic-key frecency, while
`src/tools/render/search.rs` owns Markdown discovery, Git-ignore policy, and Render suggestion
labels. `src/cdp/` similarly owns the protocol engine used by multiple tools rather than living
under either `scout` or the `cdp` command.

## Adding or changing a tool

Keep `src/main.rs` limited to registry wiring and put command code under `src/tools/<tool>/`.
Organize substantial tools into:

- a pure engine for parsing, probing, ranking, or state transitions;
- a thin `run` boundary for I/O and framework integration;
- a `tui` or daemon boundary when interactive or long-lived behavior is required.

Before adding a helper under a tool, ask whether another module already performs the same mechanism.
Before moving a helper into shared code, remove its tool vocabulary and verify that the shared owner
can describe its invariant without mentioning the original caller.

When replacing an implementation, make a clean cutover: migrate every caller, delete the superseded
path, remove unused dependencies and tests, and leave one source of truth.

## Dependencies

Prefer the standard library and existing Kit capabilities. When a maintained Rust crate provides a
complex, well-understood primitive, use it at the shared owner instead of recreating it independently
inside tools. Evaluate maintenance, API stability, transitive cost, and platform behavior before
adding it. Do not introduce multiple crates for the same responsibility.

External commands are appropriate when the command itself is the source of truth, such as Git for
repository state. Do not shell out merely to avoid integrating a suitable Rust library.

## Tests and verification

Tests should prove ownership boundaries, not only happy paths:

- shared modules test domain-neutral mechanics, lifecycle, errors, and concurrency;
- tool tests cover discovery, configuration, labels, and orchestration;
- integration tests cover behavior that crosses process, terminal, browser, or filesystem boundaries;
- regression tests are named by behavior, such as
  `stale_sessions_merge_pending_visits_instead_of_overwriting`.

Use the smallest relevant verification first and keep builds resource-conscious:

```bash
cargo check -j 2
cargo test -j 2 <test-or-module>
cargo clippy -j 2 --all-targets
cargo fmt --check
```

Run only one heavy Cargo operation at a time. Use the full `cargo test -j 2` suite once near handoff
when the change surface warrants it. See [the development guide](./docs/dev-guide.md) for setup and
the edit-run loop.

Stats TUI acceptance is intentionally headless. Use Ratatui `TestBackend` projection/input tests and
an installed `kit --json stats --once` snapshot rather than launching a terminal emulator or PTY.
The canonical proof matrix and escalation boundary live in
[Stats headless verification](./docs/canonical/stats-headless-verification.md).

## Contribution checklist

Before handing off a change, confirm:

- [ ] I searched for the canonical owner before adding code.
- [ ] Tool-specific policy remains under its tool.
- [ ] Reusable mechanics live in `framework`, `tui`, `cdp`, or a justified peer module.
- [ ] No tool imports another tool.
- [ ] Shared code contains no caller-specific vocabulary or presentation policy.
- [ ] Current consumers use the shared path; duplicate implementations were deleted.
- [ ] Shared invariants and tool adapters have focused tests.
- [ ] Public APIs and changed architecture are documented.
- [ ] Verification was proportional, sequential, and warning-free.
- [ ] The commit subject follows `<scope>: <imperative summary>`.

Pull requests should explain the owner boundary, list reused or newly shared capabilities, identify
deleted duplicate paths, report verification commands, and include terminal output or screenshots
for user-facing TUI/CDP changes.
