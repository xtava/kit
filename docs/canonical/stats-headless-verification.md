# Stats headless verification

## TLDR

Agents verify `kit stats` without opening a terminal window or starting an interactive terminal
session. The canonical acceptance path is:

1. Ratatui `TestBackend` tests for layout, rendered text, hit regions, navigation, overlays, and
   event routing.
2. Pure projection and history tests for ranking, pressure persistence, and viewport invariants.
3. `kit --json stats --once` for an installed, warmed sampler snapshot when live host data matters.

Do not launch WezTerm, another terminal emulator, a PTY verifier, or an interactive `kit stats`
session during agent verification. Interactive verification is allowed only when the user explicitly
requests it.

## Ownership

| Concern | Canonical owner |
| --- | --- |
| Raw system, process, and file-watcher observations | `src/tools/stats/sampler.rs`, `src/tools/stats/host/` |
| History admission and recent-pressure derivation | `src/tools/stats/history.rs`, `src/tools/stats/app.rs` |
| Terminal projection and semantic hit regions | `src/tools/stats/render.rs` |
| Headless frame, input, resize, and overlay acceptance | `src/tools/stats/tui.rs` tests using Ratatui `TestBackend` |
| Installed live-data proof | `kit --json stats --once` |

The renderer must return semantic `UiRegions`; tests interact with those regions instead of relying
on guessed screen coordinates. This exercises the same render and application event paths as the
interactive loop without mutating a real terminal session.

## Required proof by change type

### Projection, layout, or interaction changes

Use deterministic `TestBackend` fixtures. Assert the rendered labels or values and the semantic
regions/events that establish the behavior. Cover compact and wide terminal sizes when layout can
change across the 120-column boundary.

```bash
cargo test -j 2 tools::stats::tui::tests
```

During development, a narrower named Stats test is acceptable. Before handoff, run the complete
Stats test namespace once when the change crosses projection, history, interaction, or host layers:

```bash
cargo test -j 2 tools::stats
```

### Sampler or installed-binary changes

After the relevant tests and a single locked install, capture one warmed snapshot from the installed
binary:

```bash
./install.sh
"$HOME/.local/bin/kit" --json stats --once --interval 1000
```

The JSON snapshot proves live collection, named logical-core values, global CPU, and global process
ranking data. It does not prove interactive colors or terminal-emulator behavior.

### Core-pressure regressions

Use deterministic snapshots with one pegged core, more logical cores than screen cells, and a hot
process below a quiet parent. The acceptance assertions must prove:

- `CPU AVG` and `PEAK CORE` are distinct signals;
- the hot core remains numerically identifiable as `Cxx` after map compression;
- `TOP CORES NOW/RECENT` retains a recently hot core across scheduler migration;
- `PRESSURE SOURCES NOW/RECENT` ranks processes globally rather than by tree position;
- displayed `NOW` CPU remains the raw current sample while recent history controls only ranking;
- refresh preserves a manually scrolled viewport anchor.

### File-watcher regressions

Use deterministic watcher detail with multiple owners, a partial observation count, and configured
limits. The acceptance assertions must prove:

- WATCHERS is a top-level view rather than a process-inspector tab;
- collection is requested only while WATCHERS is active;
- rows rank kernel-observed watch records without process-name classification;
- a watcher row retains the exact stable process identity and can open its existing inspector;
- wide and compact projections expose semantic view and row regions;
- unsupported and partial observations remain explicit instead of becoming zero.

## Prohibited default verification

Agents must not perform any of the following unless the user explicitly requests interactive or
terminal-emulator validation:

- open WezTerm or another terminal window;
- run `kit stats` attached to a real TTY or synthetic PTY;
- leave a background Stats session running for visual inspection;
- use repeated polling, screenshots, or manual clicks as acceptance proof;
- create an uncontrolled CPU load or terminate existing workloads to manufacture a demonstration.

If a behavior cannot be proven through the headless paths above, report the missing proof boundary.
Do not silently escalate to an interactive session.
