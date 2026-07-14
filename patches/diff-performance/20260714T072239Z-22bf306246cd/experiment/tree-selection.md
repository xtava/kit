# Tree-selection input batching

## Problem

Every queued arrow event selected a document and forced a complete projection before the next event
could be handled. Holding an arrow therefore accumulated obsolete projection work for files the user
had already moved past.

## Hypothesis

Apply terminal events already waiting in the input queue before drawing. Selection, scrolling, and
command semantics remain sequential, but only the final state is projected. This introduces no
product cache or duplicated state.

## Control

| Case | One selection p95 | Ten obsolete projections |
| --- | ---: | ---: |
| ordinary | 7.178 ms | 71.78 ms |
| realistic | 147.622 ms | 1,476.22 ms |
| extreme | 815.320 ms | 8,153.20 ms |

The ten-projection column is the control cost implied by the former one-event/one-frame runtime
loop. Promotion uses the measured ten-event burst-to-final-frame result from the experiment run.

## Result

| Case | Former ten-event cost | Batched burst p95 | Reduction | Speedup |
| --- | ---: | ---: | ---: | ---: |
| ordinary | 71.780 ms | 7.459 ms | 89.61% | 9.62x |
| realistic | 1,476.220 ms | 163.810 ms | 88.90% | 9.01x |
| extreme | 8,153.200 ms | 831.479 ms | 89.80% | 9.81x |

Promote. The runtime applies all queued events in order, stops draining on commands such as quit,
refresh, or stage, and renders only the final accumulated state. Focused verification passes with
46 active Diff tests and two ignored performance harnesses.
