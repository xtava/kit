# Retained document projection

## Hypothesis

Vertical scrolling should move a viewport over an already rendered document projection. Rebuilding
and syntax-highlighting the entire selected diff is only necessary when the selected document,
effective layout mode, width, horizontal offsets, divider, or repository contents change.

The experiment replaces the separately retained anchor list with one keyed document projection.
The viewport then paints only its visible rows. This keeps one owner for rendered lines and anchors
instead of adding a parallel scroll cache.

## Conservative result

The comparison uses the worse p95 from two control runs and the worse p95 from two experiment runs.

| Case | Control p95 | Experiment p95 | Reduction | Speedup |
| --- | ---: | ---: | ---: | ---: |
| ordinary | 10,145 us | 233 us | 97.703% | 43.5x |
| realistic | 159,818 us | 356 us | 99.777% | 448.9x |
| extreme | 847,654 us | 617 us | 99.927% | 1,373.8x |

## Verdict

Promote. All three cases exceed the 30% promotion threshold, the ordinary case does not regress,
and the focused Diff suite passes with 45 active tests and one intentionally ignored benchmark.

The harness measures warm vertical scrolling after the initial projection exists. Selection,
refresh, layout-mode, width, horizontal-pan, and divider changes intentionally rebuild the
projection and may still pay whole-document syntax-highlighting cost. Ratatui frame production is
included; terminal-emulator flush and display latency are not.

An automated live launch against this repository could not complete its cursor-position handshake
inside the command runner's pseudo-terminal. The deterministic backend and focused correctness
suite passed; subjective terminal-emulator smoothness remains a manual sanity gate.
