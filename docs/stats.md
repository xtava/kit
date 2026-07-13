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
| Expand process threads | `Enter`, `Space`, or `→` | click an already selected row |
| Focus a logical CPU | `[`/`]` cycle cores | click a CPU tile; click again to clear |
| Return from CPU focus | `Esc` or `c` | click the active CPU tile |
| Search name, command, PID, or user | `/`, type, `Enter` | — |
| Toggle process hierarchy | `t` | — |
| Sort CPU, RAM, PID, name | `1`, `2`, `3`, `4` | click a column header |
| Gracefully end a process | `x` or `Delete`, then confirm | click **End process**, then confirm |
| Force kill | `X`, or `f` in confirmation | choose force in confirmation |
| Quit | `q` or `Ctrl-C` | — |

The default process CPU value is aggregate CPU: a process using 250% is consuming about 2.5
logical CPUs. A focused core view is explicitly approximate. Linux exposes the CPU where each
thread was last observed, not exact historical scheduler attribution, so the table is labeled
“threads last seen on Core N · approx CPU.”

The process viewport always begins at the hottest sorted row. Selecting a process updates the
inspector without letting later CPU re-sorts drag the table down to that process's new position.
The dashboard uses the exact Nord roles from the active btop theme on this machine, including its
transparent terminal background, quiet gray borders, blue CPU gradient, and selected-row colors.
The CPU surface follows btop's dense composition: one global history followed by compact logical
core readings, while the process table keeps the full terminal width and the inspector stays in a
fixed strip below it.

## Safety and performance

The overview keeps one persistent `sysinfo::System`, publishes only the latest snapshot, and does
not scan every thread or collect unused per-process disk I/O. Expanding a process samples only its
tasks. Selecting a core warms its task deltas after two seconds, then refreshes the expensive
all-task attribution pass every four seconds. The default dashboard cadence is two seconds, matching
the active btop configuration; `--interval` remains available for an explicit faster cadence.

Process control has no PID-only fallback. Kit opens a Linux pidfd, rechecks the process start token,
and sends through that descriptor. It refuses PID 1, itself, an exited or replaced process, and any
target that cannot be addressed safely. `SIGTERM` is the default and `SIGKILL` is a separate force
action.
