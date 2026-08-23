# Kit Rust practices

These rules specialize standard Rust practice for Kit. Match the repository's Rust 2021 style and
`rustfmt.toml`: 100 columns with `use_small_heuristics = "Max"`; group imports as standard library,
external crates, then crate modules, sorted within each group.

## Types and APIs

- Model state and commands with structs and enums. Make invalid states unrepresentable instead of
  coordinating booleans, strings, and optional fields.
- Give each capability one owner and a narrow typed API. Accept caller-owned data; return values or
  explicit transitions rather than reaching into another tool's state.
- Borrow by default. Clone only at an ownership boundary where retaining an independent value is
  intentional and cheap enough.
- Prefer exhaustive `match` for domain state. Use `let ... else` for local early exits and `?` for
  error propagation. Use combinators when they clarify a single transformation, not to compress a
  multi-step state machine.
- Repeated `if let Some(action)` dispatch is an ownership smell when it appears across input paths.
  Resolve keyboard, mouse, and menu input into one typed action invocation, then execute it once.
  Avoid macros that merely hide duplicated control flow.
- Implement `From`, `TryFrom`, `Display`, `Default`, and iterator traits only when their semantics are
  unsurprising and total for the advertised contract.
- Keep generics at reusable mechanics. Tool policy can use concrete types when abstraction would
  only rename one caller.

## Engines and effects

- Pure engines accept data and return data. Filesystem, terminal, process, network, daemon, and
  clock effects remain in thin `run`, TUI, or daemon shells.
- Use `thiserror` for precise engine errors and `anyhow::Result` with `.context(...)` at application
  boundaries. Preserve actionable source errors; avoid string-only error plumbing.
- Represent durable transitions explicitly. Use atomic replacement for state files and avoid two
  writable caches for one fact.
- Treat external commands as protocols: typed arguments, stdin, ordered output, exit status,
  cancellation, and lifetime all belong to the canonical process boundary.

## Async and concurrency

- Keep blocking filesystem or process work off Tokio workers when it can block materially; use the
  existing async/process boundary or `spawn_blocking` where appropriate.
- Avoid holding mutex or read/write guards across `.await`. Copy or move the required state out,
  release the guard, then await.
- Give spawned tasks an owner, cancellation path, and join/error policy. A dropped screen or command
  should not leave silent background work.
- In `tokio::select!`, reason about cancellation safety and terminal/process cleanup for every
  branch. Centralize periodic refresh rather than starting one polling loop per view.
- Use channels for ownership transfer and event streams; use shared locks for genuinely shared
  mutable state, not as a default communication mechanism.

## Platform and unsafe code

- Isolate Linux/macOS differences behind `cfg` modules implementing the same typed contract. Keep
  platform commands and path rules out of the engine.
- Prefer Rust and maintained crates to shell parsing. Shell out when the external program is the
  source of truth, such as Git or Tailscale.
- Keep unsafe code exceptional. Document the invariant immediately at the unsafe boundary and add a
  test or proof that would expose its violation.
- Keep lint allowances narrow, local, and reasoned. Warning-free Clippy is the normal state.

## Security and resource bounds

- Wrap sensitive bytes in Kit's secret types and zeroize them. Redact `Debug`, errors, process
  reports, recordings, and fixtures by construction rather than at the final print call.
- Prefer secret references over secret values in configuration. Keep credentials out of command
  arguments and environment snapshots; use the established 1Password process path.
- Bound reads, queues, caches, histories, retries, and refresh intervals. Define eviction and
  shutdown behavior for long-lived state.
- Validate external paths and archive contents before writes. Use atomic replacement for updates and
  durable state; preserve executable permissions deliberately.
- Give network operations explicit timeouts and contextual errors. Treat remote schemas and CLI
  output as fallible inputs even when the remote program is trusted.

## Tests

- Name tests by behavior, such as `stale_sessions_merge_pending_visits_instead_of_overwriting`.
- Test pure state transitions, parse failures, empty input, boundary values, cancellation, and
  concurrency behavior at their owner.
- Use `CommandFixture` for external commands instead of ad hoc shell scripts.
- Test shared abstractions with more than one domain shape when genericity is part of the claim.
- Test each tool's adapter separately from the shared invariant.
- Keep platform construction tests host-independent; reserve native runtime claims for the native
  CI runner.

## Review pass

Before handoff, search for unnecessary clones, allocations in hot render/event loops, locks held
across awaits, detached tasks, stringly typed states, duplicated optional dispatch, broad lint
allows, stale compatibility code, and tool vocabulary leaked into shared modules. Fix findings at
the canonical owner rather than layering guards around symptoms.
