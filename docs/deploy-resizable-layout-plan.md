# Deploy Resizable Panels Architecture Plan

## TLDR

- **Outcome:** Operators can drag the divider in Deploy's Browse, Versions, and Running views;
  each split survives exit and restart and remains usable after terminal resizes.
- **Canonical owner:** `deploy::state::App` owns the active layout preferences, current drag, and
  last rendered divider geometry. A small deploy-specific store only loads and atomically saves a
  typed snapshot.
- **Reference finding:** Bottom does not contain a draggable layout engine. At pinned revision
  `9c7962242277b436bba6e53ff048af2eeeeb040f`, it models rows and columns with typed ratios,
  resolves them recursively through Ratatui `Layout`, and records rendered bounds for mouse hit
  testing, while its event reader explicitly discards mouse move and drag events.
- **Target shape:** Keep Ratatui as the layout engine. Replace Deploy's three fixed percentage
  splits with normalized `SplitRatio` values, derive separator hit regions during each render, and
  route Crossterm down/drag/up events through one deploy-owned drag state machine.
- **Persistence:** Store one versioned, profile-scoped document at Kit's XDG state boundary as
  `deploy-layout.json`. Persist only the three normalized ratios; never terminal dimensions,
  rectangles, target data, config paths, or drag state.
- **Cutover:** Remove the fixed 42/58, 60/40, 58/42, and 43/57 split literals as layout owners.
  There is no compatibility lane or second runtime layout state.
- **Primary verifier:** A real terminal test must resize all three phases, restart `kit deploy`,
  observe restoration, shrink and re-expand the terminal, and confirm both panels stay above their
  minima without losing the saved preference.
- **Constraints:** This is a plan only. The current dirty tree and uncommitted Deploy implementation
  remain untouched until this revision is approved.
- **Approval state:** Approved for implementation by the user on 2026-07-13.

## Decision

Adopt a deploy-specific split model backed by Ratatui constraints. `App` is the sole runtime writer;
rendering derives geometry from it, input mutates it, and `LayoutStore` serializes snapshots at the
XDG state boundary.

This follows the useful part of Bottom's design without claiming a capability Bottom does not have:
typed relative ratios and geometry discovered from the actual rendered layout. Kit adds the missing
drag lifecycle because Bottom deliberately ignores drag events.

Reject:

- A generic shared-TUI docking or layout framework. Deploy has three one-axis splits, and no current
  second consumer justifies a tree editor, registry, pane IDs, or cross-tool persistence API.
- Copying Bottom's complete layout graph. Its arbitrary monitor-widget topology and keyboard
  neighbor mapping solve a different product problem.
- Persisting pixel/cell rectangles. They become invalid whenever terminal size, footer height, or
  phase content changes.
- Keeping local ratios in render functions and layering saved overrides on top. That creates two
  selectable layout owners and ambiguous reset behavior.
- Saving on every drag event. It couples rendering cadence to filesystem writes and can expose
  partially written state without adding user value.

Approval requested: approve the deploy-owned ratio, drag, and persistence contracts in this plan,
then authorize the implementation sequence as one clean cutover.

## Requirements and Non-Goals

### Requirements

- A left-button drag on the visible divider resizes the active two-panel phase immediately.
- Browse, Versions, and Running retain independent normalized preferences across launches.
- Journal-backed and Cloudflare-backed Versions use the same Versions preference and interaction.
- Panel minima are enforced in terminal cells; a small terminal degrades deterministically without
  corrupting the saved preference.
- Terminal resize, phase change, mouse release outside the divider, cancellation, and quit have
  explicit behavior.
- Keyboard behavior remains unchanged when no drag is active. `Ctrl-C` keeps its existing run-cancel
  priority.
- Missing state uses typed defaults. Invalid, unreadable, or newer-schema state produces a visible,
  actionable notice and typed defaults, never a panic.
- Persistence is versioned, atomic, profile-scoped, and contains no deploy target or secret data.
- Tests cover ratio math, hit testing, drag selection/cancellation/commit, restoration, corruption,
  terminal resize, and phase-specific rendering.

### Non-goals

- Arbitrary pane creation, reordering, vertical splits, tabs, docking, or nested user-authored layouts.
- Making Review or Summary resizable; neither currently contains a two-panel split.
- Moving deployment config, journal, Cloudflare history, run progress, or output ownership.
- Adding resize settings to `deploy.toml`; panel sizing is operator UI state, not deployment policy.
- Generalizing the feature for Scout, Stats, Domain, Record, or Render before a real second consumer
  proves a shared abstraction.

### Hard constraints

- Preserve `tools/* -> framework | tui | cdp`; Deploy must not depend on another tool.
- Use the existing `Session` and `EventReader`; do not create a parallel terminal stack.
- Use typed Serde data and `thiserror` at the persistence boundary; use `anyhow` context only at the
  tool/application boundary.
- Do not serialize absolute config paths, target names, account identifiers, URLs, tokens, command
  output, terminal dimensions, or rendered rectangles.
- Preserve unrelated staged, unstaged, and untracked work exactly.

## Evidence and Source Truth

### Canonical sources

| Source | Baseline | Authority | Relevant ownership |
| --- | --- | --- | --- |
| `src/tools/deploy/state.rs:17-24,81-119` | current dirty tree | local | Deploy phases and the current `App` interaction state |
| `src/tools/deploy/tui.rs:42-203` | current dirty tree | local | Deploy event loop, session construction, rendering cadence, and async run/backend events |
| `src/tools/deploy/tui.rs:454-469` | current dirty tree | local | Header/content/footer topology and phase renderer dispatch |
| `src/tools/deploy/tui.rs:492-495,651-652,696-697,897-899` | current dirty tree | local | The fixed Browse, journal Versions, Cloudflare Versions, and Running percentages being replaced |
| `src/tui/session.rs:26-42,79-90` | current dirty tree | local | Existing optional mouse capture and RAII restoration |
| `src/tui/events.rs:15-38` | current dirty tree | local | Shared event transport already carries full Crossterm `Event` values |
| [Bottom layout model](https://github.com/ClementTsang/bottom/blob/9c7962242277b436bba6e53ff048af2eeeeb040f/src/app/layout_manager.rs#L14-L36) | `9c796224` | upstream | Typed row/column/widget layout and ratio-bearing constraints |
| [Bottom recursive Ratatui layout](https://github.com/ClementTsang/bottom/blob/9c7962242277b436bba6e53ff048af2eeeeb040f/src/canvas.rs#L423-L456) | `9c796224` | upstream | Two-pass recursive constraint resolution and widget rendering |
| [Bottom rendered click bounds](https://github.com/ClementTsang/bottom/blob/9c7962242277b436bba6e53ff048af2eeeeb040f/src/app/layout_manager.rs#L835-L857) | `9c796224` | upstream | Mouse regions are derived from rendered geometry |
| [Bottom mouse filtering](https://github.com/ClementTsang/bottom/blob/9c7962242277b436bba6e53ff048af2eeeeb040f/src/lib.rs#L184-L204) | `9c796224` | upstream | Move and drag events are discarded rather than used for resizing |
| `src/tools/deploy/journal.rs:134-170,187-213,273-319` | current dirty tree | local precedent | Typed XDG state errors, schema validation, locking, and atomic-write shape |

### Verified facts

| Claim | Evidence | Confidence | Implication |
| --- | --- | --- | --- |
| Deploy currently ignores every non-key terminal event. | `src/tools/deploy/tui.rs:101-116` | high | Mouse and resize routing belongs in Deploy's existing select loop. |
| Mouse capture is already an option on the shared RAII session and is disabled on restoration. | `src/tui/session.rs:26-42,79-90` | high | Deploy only needs to request it; no shared terminal rewrite is needed. |
| Browse, both Versions variants, and Running use independent hard-coded horizontal percentages. | `src/tools/deploy/tui.rs:492-495,651-652,696-697,897-899` | high | These literals are the complete current split-owner cutover surface. |
| Bottom uses Ratatui `Layout` recursively over typed constraints. | pinned `canvas.rs:423-456` above | high | Ratatui remains Kit's layout engine; saved state should be ratios, not rectangles. |
| Bottom does not implement drag resize. | pinned `lib.rs:184-204` above | high | Drag behavior must be Kit-owned and tested as an intentional adaptation. |
| Kit already has a production-shaped XDG state store in Deploy's journal. | `src/tools/deploy/journal.rs:187-319` | high | Layout persistence should mirror its directory/error/atomicity conventions without joining the journal schema. |

### Inferences and open questions

| Statement | Why not yet verified | Decision impact | Resolution |
| --- | --- | --- | --- |
| A profile-scoped layout is preferable to config- or target-scoped layout. | Product preference is not encoded in source. | Changes persistence keys and whether layouts vary by repo. | Proposed as the smallest privacy-safe model; approval of this plan locks it. |
| A one-cell visible divider with a three-cell hit region will feel sufficiently easy to grab. | Requires live terminal use. | May alter hit-test expansion, not ownership or persistence. | Treat as a visual tuning constant and verify in WezTerm and tmux. |
| Panel minima should be content-specific rather than one universal number. | Current renderers have different useful content widths. | Determines clamping constants. | Establish named minima from render content in implementation tests; changing them does not change the schema. |

## Current and Target Ownership

| Responsibility | Current owner | Problem | Target owner | Cutover action |
| --- | --- | --- | --- | --- |
| Split preference | Percentage literals in four render paths | Not mutable or persistent; two Versions defaults diverge | `DeployLayout` inside `App` | Replace all four literals with one phase lookup. |
| Drag lifecycle | None | Mouse events are discarded | `Option<SplitDrag>` inside `App` | Route mouse events to typed transitions. |
| Divider geometry | Implicit one-cell `Layout::spacing(1)` gaps | Input cannot identify a divider | Derived `LayoutFrame` from each render | Record only current-frame separator rectangles. |
| Layout persistence | None | Preferences disappear on exit | `LayoutStore` in Deploy's shell boundary | Load before `App::new`; save committed snapshots atomically. |
| Terminal capture and events | `crate::tui::{Session, EventReader}` | Already sufficient but Deploy does not opt into mouse capture | unchanged shared owners | Open with `mouse_capture: true`; consume existing events. |

### Ownership invariants

- `App.layout` is the only mutable runtime source of split preferences.
- `App.drag` is the only drag state. It is transient and never serialized.
- `LayoutFrame` is derived geometry from the latest terminal area and preference; it cannot write a
  second preference or survive a new frame.
- `LayoutStore` knows paths, locking, schemas, and bytes, but never decides ratios or drag behavior.
- Renderers receive rectangles from the layout resolver; individual panels do not recompute ratios.

## Target Architecture

### Topology

```text
XDG state/deploy-layout.json
  -> LayoutStore::load_or_default
  -> App.layout (canonical runtime preferences)
  -> resolve_layout(phase, content_rect, preference, named minima)
  -> LayoutFrame { panels, separator, hit_region }
  -> render panels and route mouse events
  -> App.layout mutation during drag
  -> LayoutStore::save(snapshot) on committed release/reset and clean-exit retry
```

### API and data-model preview

The names are contractual; exact visibility may stay module-private.

```rust
const LAYOUT_SCHEMA_VERSION: u32 = 1;
const RATIO_SCALE: u16 = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
struct SplitRatio(u16); // validated to 1..RATIO_SCALE - 1

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeployLayoutDocument {
    schema_version: u32,
    splits: DeployLayout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeployLayout {
    browse: SplitRatio,   // default 420
    versions: SplitRatio, // default 600; shared by journal and Cloudflare
    running: SplitRatio,  // default 430
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SplitSurface { Browse, Versions, Running }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SplitDrag {
    surface: SplitSurface,
    start_ratio: SplitRatio,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LayoutFrame {
    surface: Option<SplitSurface>,
    first: Rect,
    second: Option<Rect>,
    separator: Option<Rect>,
    separator_hit_region: Option<Rect>,
}

// New fields on the existing deploy App; no second runtime owner.
struct AppLayoutFields {
    layout: DeployLayout,
    drag: Option<SplitDrag>,
    layout_frame: LayoutFrame,
    layout_dirty: bool,
}

impl App {
    fn handle_mouse(&mut self, event: MouseEvent) -> LayoutEffect;
    fn cancel_drag(&mut self);
    fn reset_active_split(&mut self) -> LayoutEffect;
}

enum LayoutEffect { None, Redraw, Persist }
```

| Field or event | Type | Owner | Scope | Default | Persistence | Mutation source |
| --- | --- | --- | --- | --- | --- | --- |
| `browse` | `SplitRatio` | `App.layout` | user profile | 420/1000 | XDG state | Browse drag or reset |
| `versions` | `SplitRatio` | `App.layout` | user profile | 600/1000 | XDG state | Either Versions variant drag or reset |
| `running` | `SplitRatio` | `App.layout` | user profile | 430/1000 | XDG state | Running drag or reset |
| `drag` | `Option<SplitDrag>` | `App` | current interaction | `None` | never | left down, drag, release, escape, phase/terminal change |
| `frame` | `LayoutFrame` | render resolver | one rendered frame | derived | never | phase, area, ratio, minima |

### Geometry and constraint contract

1. The stored ratio expresses the preferred first-panel share on a 1,000-unit scale.
2. The resolver subtracts the one-cell separator, computes named first/second minima, and clamps the
   effective divider cell to the feasible interval.
3. If both minima cannot fit, the resolver allocates cells deterministically, never underflows, and
   may collapse decorative spacing before content. The stored preference is not mutated.
4. Ratatui receives cell-exact `Constraint::Length` values for the resolved frame. This prevents
   percentage rounding from disagreeing with hit testing.
5. The visible separator is one cell. Its hit region expands by one cell on either side, clipped to
   the content rectangle. Panel rectangles remain the rendering bounds.
6. Re-expanding the terminal reapplies the unchanged saved preference, so a temporary small terminal
   cannot permanently flatten a layout.

### Persistence contract

- Location: `ProjectDirs::from("", "", "kit").state_dir()` with the journal's documented
  `data_local_dir()` fallback, file name `deploy-layout.json`.
- The file contains only `schema_version` and three ratios. It is independent of the Deploy Journal
  because journal entries are operational history while layout is profile UI preference.
- Load before `App::new`. Missing file yields `DeployLayout::default()` without a notice.
- Invalid JSON, invalid ratio, unreadable file, or unsupported schema yields defaults plus an
  actionable non-fatal notice naming the state file and the `=` reset action. The invalid file is
  not overwritten merely by opening the TUI.
- Left-button release after an actual ratio change and explicit `=` reset mark the snapshot dirty
  and request one atomic save. Drag events redraw but do not write.
- Save uses a sibling temporary file, flush/sync, atomic rename, and the same advisory-lock pattern
  as the journal. A failed save leaves runtime state intact, keeps the snapshot dirty, and shows a
  notice; clean exit retries once.
- A newer schema is never rewritten automatically. The notice explains that the installed Kit is
  older than the layout state.

### Bottom adaptation map

| Surface | Classification | Source or owner | Intentional adaptation |
| --- | --- | --- | --- |
| Relative typed ratios | adapt | pinned `layout_manager.rs:14-36` | Use one `SplitRatio` rather than Bottom's arbitrary widget tree. |
| Recursive Ratatui constraints | adapt | pinned `canvas.rs:423-456` | Deploy needs one horizontal split per applicable phase, not nested rows/columns. |
| Bounds derived while drawing | adapt | pinned `layout_manager.rs:835-857` and widget renderers | Store one current divider `Rect`, not bounds on every widget. |
| Mouse move/drag filtering | deliberate deviation | pinned `lib.rs:184-204` | Kit must forward down/drag/up to implement the requested behavior. |
| Keyboard neighbor graph | reject | pinned `layout_manager.rs` | Deploy's existing phase/list focus model remains canonical. |
| User-configured monitor layout | reject | Bottom config layout | Deploy layout is UI preference state, not deployment config. |

### Expected file surface

| Action | File or responsibility | Reason |
| --- | --- | --- |
| add | `src/tools/deploy/layout.rs` | Pure ratio validation, geometry resolution, typed document, and store boundary |
| change | `src/tools/deploy/state.rs` | Canonical layout and drag interaction state |
| change | `src/tools/deploy/tui.rs` | Mouse/resize routing, render-frame use, divider presentation, persistence effects |
| change | `src/tools/deploy/mod.rs` | Register the private layout module only |
| change | `docs/deploy.md` | Document controls, state path, reset, and failure behavior |

No change is planned for `src/tui/` because its session and event primitives already expose the
needed capabilities.

## Lifecycle and Failure Model

| Transition or state | Trigger | Canonical owner | Required behavior | Failure or absence behavior | Proof |
| --- | --- | --- | --- | --- | --- |
| construct | `kit deploy` starts | `LayoutStore`, then `App` | Load and validate one document before first frame | Missing uses defaults; invalid uses defaults plus notice | store and App tests |
| render | every draw/tick/event | layout resolver | Derive panels and hit region from current area and ratio | Tiny areas stay bounded and panic-free | geometry property/table tests |
| begin drag | left down in active hit region | `App` | Capture surface and starting ratio | Outside hit region is ignored | hit-test state test |
| update drag | left drag | `App` | Convert pointer column to clamped preferred ratio and redraw | Pointer outside area clamps safely | drag table tests |
| commit | left up while dragging | `App`, then `LayoutStore` | End drag and persist only if changed | Save error notices and remains dirty | state/store tests |
| cancel | Escape while dragging | `App` | Restore starting ratio without saving | No active drag leaves existing Escape behavior | cancellation test |
| phase switch | normal navigation | `App` | Clear any drag before changing phase | No stale divider can mutate another phase | phase test |
| terminal resize | Crossterm resize event | render resolver | Clear drag and derive a new frame; preserve preference | Too small degrades deterministically | resize TUI test |
| active run cancel | `Ctrl-C` in Running | existing run owner | Cancel run first and clear drag without committing | Existing cancellation behavior is unchanged | focused run-input test |
| reset | `=` in a split phase | `App`, then store | Restore that surface's typed default and persist | Non-split phase shows no-op help/notice | reset test |
| shutdown | quit or event stream closes | TUI shell | Cancel drag; retry one dirty committed snapshot | Report save failure after restoring terminal | boundary test |

### Keyboard and mouse coexistence

- When no drag is active, the existing key map is unchanged except for documented `=` reset.
- While dragging, Escape cancels the drag; `Ctrl-C` retains its existing quit/run-cancel semantics;
  other navigation keys are ignored until release or cancellation.
- Scroll and clicks outside the divider remain unclaimed in this change. This prevents accidental
  expansion into list mouse-selection behavior.
- The footer advertises `drag divider` and `= reset` only on split phases.

## Cutover and Deletion Ledger

| Old owner, API, state, or registration | Producers and consumers | Target path | Deletion action | Negative proof |
| --- | --- | --- | --- | --- |
| Browse `42/58` percentages | `render_browse` | `DeployLayout::browse` | delete literals and resolve through `LayoutFrame` | grep finds no percentage split in renderer |
| Journal Versions `60/40` | `render_versions` | `DeployLayout::versions` | delete local literal | both version sources assert same preference |
| Cloudflare Versions `58/42` | `render_cloudflare_versions` | `DeployLayout::versions` | delete backend-specific literal | grep plus backend branch test |
| Running `43/57` | `render_running` | `DeployLayout::running` | delete literals | rendering test uses saved ratio |
| Non-key event discard | Deploy event loop wildcard | typed mouse/resize routing | replace wildcard with explicit event handling | event tests exercise drag and resize |

### Nuclear deletion list

- [ ] All four fixed split declarations cease to own panel widths.
- [ ] No backend-specific Versions ratio remains.
- [ ] No serialized rectangle, terminal size, drag flag, or duplicate runtime ratio exists.
- [ ] No unconditional non-key event discard remains in Deploy.
- [ ] No generic shared layout registry, pane tree, or compatibility override is introduced.

## Implementation Sequence

1. Add pure `SplitRatio`, `DeployLayout`, `LayoutFrame`, and geometry tests. Exit when all supported
   areas produce bounded, non-overlapping panels and deterministic separators.
2. Add the typed `LayoutStore` by adapting journal directory, lock, schema, and atomic-write
   conventions. Exit when round-trip, missing, corrupt, invalid-ratio, newer-schema, and failed-write
   tests prove the contract.
3. Move canonical runtime preference and drag state into `App`; add transition-table tests before UI
   wiring. Exit when begin/update/cancel/commit/reset/phase-change behavior is pure and deterministic.
4. Enable mouse capture and route explicit key, mouse, and resize events in the existing select loop.
   Exit when current keyboard and run-cancellation tests remain unchanged and drag effects request
   redraw/persist correctly.
5. Cut all split renderers over to `LayoutFrame`, draw the divider, and delete fixed percentages in
   the same change. Exit when Browse, both Versions sources, and Running render from saved ratios.
6. Document controls/state/failures, run the full quality gate, then perform the live restart and
   terminal-resize proof in both WezTerm and tmux. Exit only when the persisted result is visibly
   restored and the old literals are absent.

Each step leaves one canonical path. No phase may add a saved override alongside fixed renderer
percentages.

## Risks, Decisions, and Stop Gates

| ID | Risk or unresolved decision | Likelihood | Impact | Evidence needed | Stop gate or mitigation | Owner |
| --- | --- | --- | --- | --- | --- | --- |
| R1 | Terminal drag reporting differs under WezTerm and tmux | medium | high | live event proof in both | Do not hand off without both runtime checks | implementer |
| R2 | Panel minima make a saved ratio appear ignored in narrow terminals | high | medium | shrink/re-expand test | Preserve preferred ratio separately from derived effective cells | layout resolver |
| R3 | Mouse capture changes selection behavior in the host terminal | medium | medium | enter/exit and copy/select smoke | RAII restore plus documented capture behavior | TUI shell |
| R4 | Save failures silently lose user preference | low | high | injected filesystem failure | visible notice, dirty retry, actionable path | `LayoutStore` |
| R5 | Profile-scoped rather than config-scoped layout is not desired | low | medium | user review | Approval of this revision is the stop gate | user |
| R6 | A generic abstraction is introduced before a second consumer exists | medium | medium | file/dependency review | Keep module private to Deploy | reviewer |

## Verification Matrix

| Requirement, invariant, or failure mode | Verification | Expected evidence | False-pass defense | Authorization |
| --- | --- | --- | --- | --- |
| Ratio validation and clamping | focused unit tests over boundary table | no zero/full ratios or arithmetic underflow | include widths below combined minima | allowed |
| Geometry tracks terminal size | pure resolver tests | bounded panels, separator, and hit region | shrink then re-expand with same stored ratio | allowed |
| Drag lifecycle | `App` state tests | down/drag/up commits; Escape restores start | include outside hit region and phase change | allowed |
| Versions source parity | TUI state/render tests for journal and Cloudflare | both consume `versions` | use a non-default ratio so fixed literals cannot pass | allowed |
| Persistence round trip | temp-dir store test | exact typed ratios restored | restart a fresh `App`, not the existing instance | allowed |
| Invalid/newer state | parse/load tests | defaults plus actionable warning, no overwrite | compare file bytes before and after open | allowed |
| Atomic failure behavior | injected rename/write failure | prior document remains valid | reopen from disk after failure | allowed |
| Existing interaction preserved | current Deploy unit/TUI tests plus focused key tests | navigation, deploy, versions, rollback, cancel pass | test `Ctrl-C` during a Running drag | allowed |
| Build quality | `cargo fmt -- --check`; `cargo test -j 2`; `cargo clippy -j 2 --all-targets -- -D warnings`; `cargo build -j 2` | all exit zero with no warnings | run one heavy Cargo command at a time | allowed after implementation |
| Real persistence and resize | launch installed `kit deploy` in WezTerm and tmux, resize each split, quit, relaunch, shrink/re-expand | all three preferences restore and remain usable | inspect file and fresh process; do not reuse same `App` | requires implementation approval |
| Clean cutover | `rg` for the four old percentage pairs and duplicate layout owners | no selectable old path | inspect both Cloudflare and journal render branches | allowed |

### Completion proof

- [ ] Every requirement has passing evidence.
- [ ] The old fixed-layout owners and non-key discard path are absent.
- [ ] One runtime preference owner and one transient drag owner exist.
- [ ] The saved document contains only schema version and three ratios.
- [ ] Corrupt/newer state and failed writes are visible and non-destructive.
- [ ] Full build, test, clippy, and formatting gates pass sequentially with two jobs where applicable.
- [ ] Fresh-process restoration is demonstrated in WezTerm and tmux.

## Adversarial Review Ledger

| ID | Round and lens | Severity | Confidence | Finding and failure mode | Evidence | Required revision | Disposition |
| --- | --- | --- | --- | --- | --- | --- | --- |
| A1 | premise/upstream | high | high | Calling Bottom a draggable layout engine would produce a false copy plan. | pinned `lib.rs:184-204` | Separate ratio precedent from Kit drag design. | accepted |
| A2 | ownership | high | high | Persisted overrides plus fixed renderer percentages would create two owners. | local renderer citations | Delete literals in the same cutover. | accepted |
| A3 | lifecycle | high | high | Clamping by mutating the saved ratio would destroy preference on terminal shrink. | geometry contract | Keep saved preference; derive effective cells. | accepted |
| A4 | persistence | high | medium | Writing every drag event could churn disk and expose partial state. | interaction lifecycle | Persist on release/reset with atomic write. | accepted |
| A5 | simplicity | medium | high | A shared arbitrary pane framework has no second consumer. | local surface inventory | Keep `layout.rs` private and three-surface-specific. | accepted |
| A6 | verification | high | high | In-process tests can falsely claim persistence without a reload. | verification matrix | Require fresh `App` and live process restart. | accepted |

### Coverage audit

| Set | Source count or inventory | Plan coverage | Gap |
| --- | --- | --- | --- |
| resizable surfaces | Browse, Versions, Running | data model, cutover ledger, render/runtime tests | none |
| versions variants | journal and Cloudflare | one `versions` ratio plus branch-specific tests | none |
| interaction transitions | down, drag, up, cancel, phase change, resize, reset, quit | lifecycle table and state tests | none |
| persistence states | missing, valid, corrupt, invalid ratio, unreadable, newer schema, failed save | persistence contract and store tests | none |
| legacy owners | four percentage pairs and non-key wildcard | deletion ledger and negative grep | none |
| environment proof | direct terminal, WezTerm, tmux | runtime matrix | direct terminal is covered by either host; no separate emulator required |

## Approval

- **Revision:** 1 — 2026-07-13.
- **Architecture decision requested:** Approve profile-scoped, deploy-owned normalized ratios; one
  transient drag state; render-derived separator geometry; and a versioned XDG snapshot containing
  only Browse, Versions, and Running preferences.
- **Implementation authorization requested:** Granted on 2026-07-13 for phases 1-6 as one clean
  cutover, including docs, tests, sequential Cargo verification, one install, and non-mutating live
  TUI verification against the user's existing deploy configuration.
- **External or destructive action authorization:** None. No deploy or rollback operation is part of
  verification.
- **Goal activation:** Not requested.

## Revision History

| Revision | Evidence or review that triggered it | Material changes | Approval state |
| --- | --- | --- | --- |
| 1 | Local Deploy/TUI owner map plus Bottom `9c796224` source audit | Selected a narrow deploy-owned split model; explicitly identified dragging as a Kit adaptation; specified persistence, lifecycle, deletion, and proof | approved 2026-07-13 |
