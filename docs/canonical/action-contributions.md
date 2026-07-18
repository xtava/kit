# Action Contributions and Context Menus

## TLDR

Kit's action-contribution system is a compiled-in, typed organization boundary for interactive
tools. Shared TUI code validates and resolves action metadata and owns context-menu mechanics. Each
tool owns its action IDs, context, commands, predicates, presentation choices, and execution policy.

The first adopter is `kit stats`: its context-menu, inline, and keyboard affordances all project one
immutable per-run catalog and execute through one Stats command owner. This is not a dynamically
loaded plugin runtime, stable extension ABI, or application-global command registry.

## Scope

The system provides:

- qualified `ActionId` and `MenuId` identities;
- typed action metadata, menu placements, and keybinding placements;
- construction-time graph validation and deterministic menu resolution;
- caller-owned visibility and enablement predicates;
- typed invocations carrying a caller-owned frozen context;
- reusable context-menu state, layout, rendering, and input capture; and
- one contribution catalog projected into multiple interaction surfaces.

It deliberately does not provide installable plugins, dynamic libraries, WASM or subprocess
extensions, manifests, runtime registration, hot reload, persistence, a command palette,
user-configurable keybindings, submenus, icons, check/radio items, or asynchronous menu loading.

## Canonical owners

| Owner | Responsibility | Must not own |
| --- | --- | --- |
| `src/tui/actions.rs` | Domain-neutral IDs, builder validation, immutable registry, deterministic menu/key resolution, and typed invocation lookup | Tool vocabulary, handlers, effects, mutable/global registration |
| `src/tui/context_menu.rs` | Frozen popup state, Unicode-cell geometry, clamped layout, rendering, keyboard/mouse capture, and typed outcomes | Tool commands, authorization, terminal loop, Stats state |
| `src/tools/stats/contributions.rs` | Stats action/menu IDs, labels, commands, context shape, predicates, placements, and chords | Popup mechanics or OS effects |
| `src/tools/stats/app.rs` | Exact-target context construction, one command executor, overlay lifecycle, confirmation policy, and event routing | Process signalling implementation |
| `src/tools/stats/render.rs` | Registry-backed inline projection, semantic action/confirmation hit regions, and last-pass popup rendering | Action metadata copies or input policy |
| `src/tools/stats/tui.rs` | Registry validation before interactive side effects, the terminal loop, and production-path acceptance tests | A second action catalog or event loop |
| `src/tools/stats/actions.rs` | Single-flight asynchronous process-action execution | Menu visibility or selection policy |
| `src/tools/stats/host/` | Platform capabilities and generation-bound native process effects | UI enablement as authorization |

The repository dependency rule remains `tools/* -> framework | tui | cdp`. Shared TUI modules never
import Stats, and one tool never imports another tool.

## End-to-end flow

```text
tool contribution function
  -> ActionRegistryBuilder<ToolContext, ToolCommand>
  -> build and validate before interactive side effects
  -> immutable ActionRegistry<ToolContext, ToolCommand>
  -> resolve named menu or keybinding against a tool-owned context
  -> ResolvedMenu / ActionInvocation<ToolContext>
  -> shared context-menu mechanics or tool-owned inline projection
  -> one tool-owned command executor
  -> tool overlay or existing effect controller
  -> platform host boundary
```

For Stats, `tui::run` constructs and validates the catalog before creating the sampler, worker,
terminal session, or event reader. Each draw resolves current inline actions from that catalog.
Right-click resolves the process context menu once and freezes both its resolved items and exact
`ProcessIdentity`. Keys resolve through the same registry. Every resulting invocation enters
`StatsApp::invoke_action`; there is no parallel key, mouse, or visible-label dispatch path.

## Registry contract

`ActionRegistryBuilder<C, Command>` accepts three explicit contribution types:

- `ActionSpec<C, Command>` maps one stable ID to a title, typed command, and enablement function;
- `MenuPlacement<C>` places an action in a named menu with group/order and a visibility function;
- `KeybindingPlacement<C>` maps one normalized chord to an action with a visibility function.

`build()` fails before use when an ID is invalid, an action or menu placement is duplicated, a
placement references an unknown action, one group uses conflicting orders within the same menu, or
one normalized chord maps to multiple actions. IDs are ASCII qualified names of at most 96 bytes;
labels are never identities. The builder canonicalizes placements once by
`(menu_id, group_order, group, order, action_id)`; resolution only filters that immutable order for
one menu and context. Source insertion order is not behavior, and resolved items do not retain
sorting metadata that consumers could reinterpret.

The registry contains metadata and function-pointer predicates, not execution handlers. It is
immutable after construction and belongs to one adopting tool run. `command_for()` re-evaluates
enablement against the invocation's captured context and returns either the typed command or a typed
unavailable error. Predicate results are presentation policy, not destructive authorization.

## Context-menu contract

`ContextMenu<C>` opens only for a non-empty `ResolvedMenu` and stores the resolved items, selected
index, anchor, and caller-owned context. It returns typed `Captured`, `Dismissed`, `Unavailable`, or
`Invoke` outcomes; the caller remains the command owner.

One `ContextMenuLayout` is the canonical handoff between rendering and subsequent mouse input. It
contains the popup rectangle, action-ID-bearing item hit rows, and separators. Rendering and input
resolve those IDs against the menu's frozen items instead of trusting positional indices, so a
stale or foreign layout cannot activate an unrelated item or panic. Layout uses terminal-cell width,
saturating/clamped geometry, and the current viewport, so resizing recomputes geometry without
changing the selected action or frozen context. Production input never fabricates a fallback layout.

The menu supports Up/Down, `j`/`k`, Home/End, Enter, Esc, and `q`. Registered shortcuts shown on its
items remain active while it is open; exact unmodified navigation, activation, and dismissal chords
retain modal precedence and are omitted as unusable item hints. Modified variants remain eligible
as contributed shortcuts. Pointer movement selects a published item row; left-click invokes an
enabled row, reports an unavailable disabled row, captures clicks inside non-item menu space, and dismisses
clicks outside. Dismissal is modal and cannot fall through to an underlying row, tab, split divider,
or inline action. Stats handles `Ctrl-C` before one overlay gateway, which routes every other modal
event before base input. The menu renders last.

With `--no-mouse`, Stats still renders inline labels and shortcut hints and resolves registered keys,
but it publishes no pointer action regions and cannot open or invoke a pointer menu.

## Stats catalog

### Named menus

| Menu ID | Projection |
| --- | --- |
| `stats.process.context` | Deliberate right-click on a concrete process row or process inspector |
| `stats.process.commandInline` | Overview's explicit command control |
| `stats.process.inspectorInline` | Inspector Profile and graceful-termination controls |

### Actions

| Action ID | Title | Typed command | Surfaces | Chord | Availability |
| --- | --- | --- | --- | --- | --- |
| `stats.process.viewCommand` | View full command | `StatsCommand::ViewCommand` | Process context; command-inline on Overview | `v` on Overview | Process identity is still live |
| `stats.process.openProfile` | Profile | `StatsCommand::OpenProfile` | Process context; inspector-inline | `p` | Always enabled navigation; the Profile tab explains unavailable collection |
| `stats.process.terminate` | End process… | `RequestTerminate(GracefulTerminate)` | Process context; inspector-inline | `x`, Delete | Live stable generation, host capability, and no action already running |
| `stats.process.forceTerminate` | Force end process… | `RequestTerminate(ForceTerminate)` | Process context only | `X` | Live stable generation, host capability, and no action already running |

The context menu groups View/Profile under `navigation` and both termination actions under
`destructive`, with one separator between the groups. Inspector-inline resolution also runs for an
exited selected snapshot: Profile remains available while termination stays visible and disabled by
the contribution predicate.

## Exact-target and effect safety

An invocation carries the exact `ProcessIdentity` captured by the surface that created it. A row
right-click selects and captures that row; an inspector right-click captures the process currently
shown. Later selection changes do not retarget an open menu. No contributed action resolves by PID,
row index, or current selection after capture.

Stats performs destructive checks again when opening confirmation: it rejects an active action,
unsupported capability, missing/replaced process, or snapshot-only identity, then stores the exact
`ProcessKey { pid, start_token }`. Confirmation choices name concrete effects: a graceful request may
offer graceful, capability-available force, and Cancel; a force request offers only force and
Cancel. Cancel is the default. Activation revalidates the selected effect before emitting it.
`ActionController` admits one process action at a time and delegates to the platform host. On Linux,
the host opens a pidfd, re-reads the start token, rejects replacement, and signals through the
descriptor. PID-only fallback is forbidden.

`StatsOverlay` is the sole modal owner with mutually exclusive `ContextMenu`, `Confirmation`, and
`CommandViewer` variants. Target loss or PID reuse closes an open menu or confirmation with an
unavailable status. A command viewer keeps the command text it already copied and remains frozen.

## Adding contributions to another tool

1. Define private qualified action/menu IDs plus tool-owned `Context` and `Command` types under
   `src/tools/<tool>/`.
2. Add an explicit contribution function that registers action specs, named menu placements, and
   chords in `ActionRegistryBuilder<Context, Command>`.
3. Build and validate one registry at the interactive entry boundary before terminal, worker,
   process, network, or daemon side effects.
4. Resolve inline named menus during rendering and emit semantic hit regions carrying the rendered
   `Rect`, `ActionId`, and exact domain identity. Resolve a context menu when the deliberate open
   event occurs so its items and context freeze together; render it last and publish its exact
   `ContextMenuLayout` for input.
5. Capture the exact domain identity in every invocation. Route inline, context-menu, and keyboard
   projections into one tool-owned command executor.
6. Re-check liveness, capabilities, identity, and authorization at the command/effect owner; never
   treat `ActionState::Enabled` as authority.
7. Test shared mechanics at `tui`, contribution policy under the tool, and the full production
   render/input/effect boundary at the adopting tool.

Do not make shared TUI code import the adopter, add callbacks to the registry, or import another
tool to reuse its catalog. A second adopter may extend shared APIs only for a demonstrated common
mechanic, not to move tool policy into `tui`.

## Canonical invariants

- One `ActionId` maps to one typed command; visible text never selects behavior.
- One validated per-tool registry is the source for menu, inline, and keyboard projections.
- Registries are callback-free for execution, immutable, non-global, and non-persistent.
- Visibility and enablement are separate; enablement is checked again at invocation.
- Every invocation carries caller-owned context, including exact domain identity.
- Destructive execution revalidates at the tool and host boundaries.
- One published action-ID-bearing layout owns both popup rendering and hit-testing.
- Overlay input is modal, outside dismissal is consumed, and global `Ctrl-C` retains priority.
- One overlay enum makes simultaneous menu, confirmation, and viewer states unrepresentable.
- Headless/JSON behavior does not construct the interactive registry or popup path.
- Current consumers use the canonical path directly; no compatibility lane is permitted.

## Paths that must not return

- direct `p`, `v`, `x`, `X`, or Delete command branches beside registry resolution;
- bespoke `command_open`, `profile`, or `end_process` hit-region fields;
- independent confirmation and command-viewer state with duplicated priority rules;
- selected-target-only helpers for contributed commands;
- hard-coded action labels or shortcut hints in the renderer;
- dispatch by visible label, current selection, PID alone, or row index;
- fabricated/default popup geometry when no rendered layout exists;
- application-global or mutable registries, handler callbacks, linker inventory, or dual catalogs;
- aliases, adapters, fallback branches, or old/new coexistence around this ownership path.

## Verification entry points

- `cargo test -j 2 tui::actions::tests` — registry graph, ordering, chord normalization, and genericity.
- `cargo test -j 2 tui::context_menu::tests` — frozen context, layout/render hit-map identity,
  Unicode geometry, resize, disabled state, and modal input.
- `cargo test -j 2 tools::stats::contributions::tests` — exact catalog, projections, chords, and
  enablement matrix.
- `cargo test -j 2 tools::stats::tui::tests::headless_process_menu_acceptance_covers_render_input_and_resize`
  — production registry/app/renderer/`UiRegions` integration at wide and compact sizes.
- `cargo test -j 2 tools::stats::tui::tests` — projection parity, exact target, overlay lifecycle,
  no-mouse, Ctrl-C, target loss/reuse, and command-hitbox regressions.
- `cargo test -j 2 tools::stats::host::linux::tests::pidfd_terminates_the_exact_disposable_child`
  — real Linux generation-bound effect against only the test-owned child.

## Boundaries and stop gates

Keep new action surfaces compiled-in and tool-owned. Re-enter architecture design before adding
runtime plugin discovery, a stable third-party ABI, manifests, unloading/hot reload, global
registration, persistence, configurable keybindings, a command palette, submenus, asynchronous
resolution, or cross-process execution. These concerns add lifecycle, trust, ownership, and failure
models that the current registry intentionally does not have.

Likewise, do not expand the shared API speculatively for a future adopter. Add shared capability
only when a concrete tool exposes a domain-neutral mechanic that cannot be expressed by the current
typed context, command, placement, and outcome contracts.
