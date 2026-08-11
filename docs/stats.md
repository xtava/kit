# kit stats

`kit stats` is Kit's interactive CPU and process monitor. It opens a Ratatui dashboard when both
stdin and stdout are terminals, and falls back to a warmed-up text snapshot when piped. Global
`--json` emits the same named snapshot model used by the dashboard.

```bash
kit stats
kit stats --no-mouse
kit stats --once
kit --json stats --once
kit stats --interval 500
```

Agent acceptance for this TUI is headless: deterministic Ratatui frame/input tests prove the
interactive projection, and `kit --json stats --once` proves installed live sampling. Agents do not
open terminal windows or PTY sessions unless interactive validation is explicitly requested. See
[Stats headless verification](./canonical/stats-headless-verification.md).

## Controls

| Action | Keyboard | Mouse |
| --- | --- | --- |
| Open File Watchers | `w`; `w` or `Esc` returns to Processes | click **WATCHERS** or **PROCESSES** |
| Select process | `↑`/`↓`, `j`/`k`, `PageUp`/`PageDown` | click row or scroll |
| Jump to the hottest/top row | `Home` | click a sort header to reorder from the top |
| Expand/collapse a process family | `Space`, `←`/`→` | click the disclosure marker |
| Move between processes and inspector | `Tab`/`Shift-Tab`, `←`/`→` at a tree boundary | click either region |
| Resize process and inspector panels | `<`/`>`; `=` resets | drag the divider |
| Open the inspector | `Enter` | click an inspector tab |
| Change inspector tab | `←`/`→` while the inspector is active | click Overview, Family, Threads, Resources, or Profile |
| Focus the selected family | `f` | — |
| Focus a logical CPU | `[`/`]` cycle cores | click a CPU tile; click again to clear |
| Return from CPU focus | `Esc` or `c` | click the active CPU tile |
| Search name, command, PID, or user | `/`, type, `Enter` | — |
| Sort CPU, RAM, PID, name | `1`, `2`, `3`, `4` | click CPU, MEM, PID, or NAME; click again to reverse |
| Open the full observed command | `v` from Overview | click **View full command** |
| Open Profile | `p` | click **Profile** |
| Gracefully end a process | `x` or `Delete`, then confirm | click **End process**, then confirm |
| Force kill | `X`, or `f` in confirmation | choose force in confirmation |
| Open process actions | use the action shortcuts above | right-click a process row or the process inspector |
| Quit | `q` or `Ctrl-C` | — |

## File watchers

The top-level **WATCHERS** view attributes Linux inotify resources to their generation-verified
owning processes. It finds Parcel, rspack, Electron, and other watcher implementations from kernel
state rather than process names or command-line guesses. Rows are ranked by watch count and show
the current process CPU and memory alongside:

- **DESCRIPTORS** — open file descriptors that refer to inotify instances;
- **WATCHES** — watch records reported by those descriptors.

The summary shows the observed totals and the host's configured per-user limits for watches,
instances, and queued events. A duplicated descriptor may refer to the same underlying inotify
instance, so Kit labels the observed descriptor count precisely instead of claiming it is an exact
instance count.

Watcher collection is lazy and cooperative. Stats scans only while the WATCHERS view is active and
yields to the normal overview refresh between processes. Processes that exit, reuse a PID, or
cannot be inspected are excluded from attribution and reported in the partial-sample count.
Individual watched paths are not exposed by Linux fdinfo and are not inferred from inode numbers.
macOS and Windows display the watcher capability as unsupported rather than reporting zero.

## Process actions

Right-clicking a concrete process row selects that exact row and opens its action menu at the
pointer. Right-clicking the process inspector opens the same menu for the process currently shown
there. Ordinary left-clicks in inspector content never open a menu; they activate only explicit
tabs and visible inline actions.

The menu order is:

1. **View full command** — the same action as `v` from Overview and the Overview inline action.
2. **Profile** — the same navigation action as `p` and the inspector inline action.
3. **End process…** — the same graceful request as `x`/`Delete` and the inspector inline action.
4. **Force end process…** — the same force request as `X`; it is intentionally menu- and
   keyboard-only.

Navigation and destructive actions are separated visually. Actions that require a live process,
stable process generation, available host capability, or an idle process-action controller remain
visible but disabled with the reason shown. A disabled action cannot invoke. Profile remains a
navigation surface and explains unavailable profiling inside the tab rather than starting an
unsupported collector.

Use `↑`/`↓`, `j`/`k`, `Home`/`End`, and `Enter` inside the menu. The registered action shortcuts
remain active while it is open. `Esc`, `q`, or a click outside closes it, and that dismissal click is
consumed instead of activating the surface underneath. `Ctrl-C` always quits Stats, including while
a menu, confirmation, or full-command viewer is open.

With `--no-mouse`, Stats does not create pointer action regions and mouse input cannot open or invoke
the menu. Inline labels and shortcut hints remain visible, and `v`, `p`, `x`/`Delete`, and `X`
continue to use the same registered actions from the keyboard.

Stats builds one typed action registry for each interactive run. Context-menu, inline, and keyboard
projections resolve from that catalog, while one mutually exclusive overlay owner holds the menu,
confirmation, or command viewer. This is an internal organization boundary for Rust contributions,
not an installable or dynamically loaded plugin runtime.

See [Action contributions and context menus](./canonical/action-contributions.md) for the canonical
architecture and extension contract.

The default process CPU value is aggregate CPU: a process using 250% is consuming about 2.5
logical CPUs. A focused core view is explicitly approximate. Linux exposes the CPU where each
thread was last observed, not exact historical scheduler attribution, so the table is labeled
“threads last seen on Core N · approx CPU.”

The system band separates whole-machine `CPU AVG` history from `PEAK CORE` history. `TOP CORES
NOW/RECENT` names the busiest logical CPUs and retains their recent peak for three admitted samples,
so scheduler migration does not immediately erase the previous hot-core identity. The compact Core
Map remains clickable and preserves the hottest member when several cores share a cell. Selecting a
core focuses the process table on threads last observed there.

`PRESSURE SOURCES NOW/RECENT` is a global process ranking independent of the process hierarchy. It
shows exact current CPU beside a three-sample recent average, allowing a hot descendant to remain
visible even when its parent branch is below the process viewport. The CPU column also displays exact
current CPU; only CPU ordering uses the recent average to prevent rows from swapping on every raw
sample.

The viewport follows the current top row while it is already at the top. After explicit navigation
scrolls the table, refreshes preserve the first visible stable process as the viewport anchor instead
of returning to row zero. `Home` and explicit sort changes still return to the first sorted row.
Filtering may hide a selection without replacing it; PID reuse never transfers a selection to the
replacement generation.

At 120 columns or wider, the process tree and inspector render side by side. Compact terminals show
one active region at a time, with the same Tab and arrow navigation. The cyan border identifies the
active region. The inspector exposes Overview, Family, Threads, Resources, and Profile as explicit
capability-aware surfaces rather than expanding expensive detail inline in the process table.

Overview distinguishes the selected process's own CPU and memory from the complete repaired family
aggregate. Family ranks direct children, CPU-heavy descendants, and memory-heavy descendants across
the full subtree. Threads can be ordered by live CPU, accumulated CPU, TID, or name. Resources is
deliberately aggregate-only: executable, working directory where available, virtual address space,
platform-labelled process I/O, and an aggregate file-descriptor or handle count. Kit does not read
process environments, memory contents, socket payloads, or individual descriptor targets.

## Platform capabilities

| Capability | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Stable process generation | `/proc` start ticks | `proc_pidinfo` start time | creation `FILETIME` |
| Threads | native task records, including last-observed core | libproc thread records; core unavailable | Toolhelp thread records; name/state/core unavailable |
| Resources | executable, cwd, virtual bytes, process I/O, file descriptors | executable, virtual bytes, file descriptors | executable, process I/O, handles |
| File watchers | per-process inotify descriptors and watch records; configured user limits | unsupported | unsupported |
| Process action | pidfd graceful and force termination | read-only | verified-handle force termination |
| Code profile | unavailable on this host: local perf emitted no actionable DWARF stacks | unavailable: no atomic generation-bound attach | unavailable: no bounded non-elevated generation-bound collector |

Unsupported and permission-denied fields remain visible as typed states; Kit does not replace them
with zeros or inferred values. Opening an unavailable Profile surface never starts an external
profiler. Kit does not change perf policy, request elevation, download symbols, or silently fall
back to a PID-only or system-wide capture.

## Safety and performance

The overview keeps one persistent `sysinfo::System`, publishes only the latest snapshot, and does
not scan every thread, file descriptor, or unused per-process resource. Threads, Resources,
File Watchers, and focused-core attribution are separate typed requests with bounded cadences and
explicit warming, partial, unavailable, and error states. Late detail responses cannot overwrite a
newer request. The default overview cadence is two seconds; `--interval` remains available for an
explicit faster cadence.

Process control has no PID-only fallback. Host adapters expose their capabilities explicitly and
revalidate stable process identity before acting. On Linux, Kit opens a pidfd, rechecks the process
start token, and sends through that descriptor. It refuses PID 1, itself, an exited or replaced
process, and any target that cannot be addressed safely. Graceful termination is the default and
force termination is a separate confirmation path.

Each menu invocation carries the exact process generation captured when the menu opened. Opening a
termination confirmation retains that generation, and confirmation revalidates it before emitting
the process action. If the target exits or its PID is reused, an open menu or confirmation closes
with an unavailable status; it never follows the current selection or the replacement process. A
full-command viewer that already copied observed text remains open and frozen.

History is admitted every two seconds and bounded to eight minutes (240 points) for at most 1,024
stable process generations. Exited history expires; a replacement that reuses the same PID receives
a distinct generation. Focused-core collection scans native task records cooperatively and yields
to an overdue overview refresh between processes.
