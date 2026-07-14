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

## Controls

| Action | Keyboard | Mouse |
| --- | --- | --- |
| Select process | `↑`/`↓`, `j`/`k`, `PageUp`/`PageDown` | click row or scroll |
| Expand/collapse a process family | `Space`, `←`/`→` | click the disclosure marker |
| Move between processes and inspector | `Tab`/`Shift-Tab`, `←`/`→` at a tree boundary | click either region |
| Open the inspector | `Enter` | click an inspector tab |
| Change inspector tab | `←`/`→` while the inspector is active | click Overview, Family, Threads, Resources, or Profile |
| Focus the selected family | `f` | — |
| Focus a logical CPU | `[`/`]` cycle cores | click a CPU tile; click again to clear |
| Return from CPU focus | `Esc` or `c` | click the active CPU tile |
| Search name, command, PID, or user | `/`, type, `Enter` | — |
| Sort CPU, RAM, PID, name | `1`, `2`, `3`, `4` | click a column header |
| Open Profile | `p` | click **Profile** |
| Gracefully end a process | `x` or `Delete`, then confirm | click **End process**, then confirm |
| Force kill | `X`, or `f` in confirmation | choose force in confirmation |
| Quit | `q` or `Ctrl-C` | — |

The default process CPU value is aggregate CPU: a process using 250% is consuming about 2.5
logical CPUs. A focused core view is explicitly approximate. Linux exposes the CPU where each
thread was last observed, not exact historical scheduler attribution, so the table is labeled
“threads last seen on Core N · approx CPU.”

The process viewport always begins at the hottest sorted row. Explicit navigation scrolls the table,
while the next sample returns the viewport to the top without transferring the selected stable
process identity. Filtering may hide a selection without replacing it; PID reuse never transfers a
selection to the replacement generation.

At 120 columns or wider, the process tree and inspector render side by side. Compact terminals show
one active region at a time, with the same Tab and arrow navigation. The cyan border identifies the
active region. The inspector exposes Overview, Family, Threads, Resources, and Profile as explicit
capability-aware surfaces rather than expanding expensive detail inline in the process table.

## Safety and performance

The overview keeps one persistent `sysinfo::System`, publishes only the latest snapshot, and does
not scan every thread or collect unused per-process resources. Threads, Resources, and focused-core
attribution are separate typed requests with bounded cadences and explicit warming, unavailable, and
error states. Late detail responses cannot overwrite a newer request. The default overview cadence
is two seconds; `--interval` remains available for an explicit faster cadence.

Process control has no PID-only fallback. Host adapters expose their capabilities explicitly and
revalidate stable process identity before acting. On Linux, Kit opens a pidfd, rechecks the process
start token, and sends through that descriptor. It refuses PID 1, itself, an exited or replaced
process, and any target that cannot be addressed safely. Graceful termination is the default and
force termination is a separate confirmation path.
