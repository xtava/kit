# Kit system map

Use the repository's live `AGENTS.md`, `CONTRIBUTING.md`, `docs/dev-guide.md`, canonical docs, and
source as the final authority. This map identifies where to look and who should own a change.

## Architecture

Kit is one Rust 2021 crate and one binary. The module graph is the architecture:

```text
src/main.rs -> Registry composition
src/tools/* -> framework | tui | cdp
```

The spine and peer modules never reach into `tools`; one tool never imports another tool.

| Surface | Canonical responsibility |
| --- | --- |
| `src/main.rs` | Construct and register tools; keep behavior out |
| `src/lib.rs` | Library surface and top-level module graph |
| `src/framework/` | Tool contract, registry, context, output, config, settings, repositories, terminal, atomic files, processes |
| `src/tui/` | Reusable terminal interaction and presentation mechanics |
| `src/cdp/` | Shared Chrome DevTools Protocol engine |
| `src/tools/<tool>/` | One command's discovery, policy, orchestration, and presentation |
| `tests/` | Cross-boundary behavior and integration tests |

A new peer module is justified only by a named domain capability, more than one real consumer, and
an API with no tool vocabulary.

## Framework contract

- Implement `framework::Tool` for command identity, clap command construction, optional settings,
  and asynchronous execution.
- Accept shared services through borrowed `framework::Context`: configuration, output, terminal,
  repository lookup, and process supervision. Kit intentionally has no DI container.
- Register the tool in `src/main.rs`; keep parsing, policy, and behavior in library modules.
- Respect global output behavior. Machine-readable paths use `Output` and remain deterministic;
  interactive behavior is selected using `Context::term` inside the tool.
- Contribute editable preferences through `SettingsSection` and `ConfigStore` rather than creating
  a tool-specific configuration lane.
- Use `AtomicFileWriter` for durable state that must survive interruption without partial writes.
- Use `RepositoryLocator` and `WorktreeRoot` for repository discovery rather than inventing path
  heuristics.

## Process boundary

All external commands cross `framework::process::ProcessSupervisor`. Use the process types for
attached commands, detached services, receipts, sessions, output, and reports. Tests use
`framework::process::test_support::CommandFixture`, including ordered output, delays, stdin, exit
status, hangs, and invocation assertions.

Keep process invocation in a thin shell. Feed its typed result into pure engine code. Model
cancellation and child cleanup explicitly; a dropped future must not silently orphan work.

## TUI contract

Tools own their `tokio::select!` event loops because their async sources differ. Shared TUI modules
own reusable mechanics:

| Need | Existing owner |
| --- | --- |
| terminal lifecycle | `Session`, `SessionOptions` |
| async terminal input | `EventReader` |
| text entry | `LineEditor` |
| actions, keybindings, menus | `ActionRegistry`, `ActionSpec`, `KeybindingPlacement`, `MenuPlacement` |
| context menus | `ContextMenu` |
| arrow, tab, and mouse geometry | `NavigationMap`, `NavigationRegion`, `Direction` |
| back/forward traversal | `NavigationHistory` |
| resizeable panels | `SplitFrame`, `SplitRatio`, `SplitDrag` |
| search and ranking | `FuzzyIndex`, `FrecencyStore`, suggestions |
| clipboard | `tui::clipboard` |
| editable preferences | `SettingsEditor`, `SettingsFlow` |
| shared appearance | `tui::theme`, syntax, and Markdown renderers |

Build one typed action vocabulary and project it into every interaction surface. Every visible
control receives a mouse hit target; every core operation receives a keyboard path; arrows follow
rendered geometry; left/right navigation and explicit back/forward use history where stateful
navigation is involved. Resize behavior is constrained by shared minimums and ratios.

Keep frame-local geometry derived from the current render. Normalize focus when panels disappear or
resize. Context-menu enablement and keybinding enablement derive from the same action state so the UI
cannot advertise an unavailable command.

## Platform boundary

Kit supports Linux and macOS. Put platform differences behind narrow `cfg` modules with the same
typed contract. Keep the engine platform-neutral and test platform command construction where the
host cannot execute the other platform. Release assets and supported-target logic contain no
Windows lane.

## Secrets and trust boundaries

Use the shared `onepassword` capability for `op://` references and secret-bearing process launches.
Keep secret values out of arguments, logs, debug output, persisted configuration, fixtures, and
error text. Preserve redacted `Debug` implementations, bounded secret buffers, zeroization, strict
file permissions, and child cleanup. Validate paths, URLs, repository roots, and external command
output at the boundary before they become trusted domain values.

## Documentation ownership

- `CONTRIBUTING.md` owns contribution architecture and reuse rules.
- `docs/dev-guide.md` owns setup and the development loop.
- `docs/release.md` owns release and updater behavior.
- `docs/canonical/` owns subsystem-specific verification or architecture contracts.
- Tool docs own user-facing command behavior.

Update a canonical document when its behavior contract changes. Keep research and temporary plans
out of durable user documentation unless explicitly requested.
