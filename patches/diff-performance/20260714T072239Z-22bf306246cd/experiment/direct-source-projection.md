# Direct source projection

## Hypothesis

Diff should style only canonical rows that appear in the comparison instead of syntax-highlighting
both complete file sides before the first frame. Character-level change emphasis and row backgrounds
remain intact; whole-file syntax coloring is removed from Diff.

## Control

| Case | Syntax projection p95 | Plain projection p95 | Syntax share |
| --- | ---: | ---: | ---: |
| ordinary | 7.105 ms | 0.053 ms | 99.25% |
| realistic | 144.938 ms | 1.220 ms | 99.16% |
| extreme | 896.083 ms | 40.267 ms | 95.51% |

The plain control still allocated fallback spans for every source line. The experiment removes those
whole-file fallback allocations as well and creates styled spans only for rows present in hunks.

## Result

| Case | Control p95 | Experiment p95 | Reduction | Speedup |
| --- | ---: | ---: | ---: | ---: |
| ordinary | 7.105 ms | 0.027 ms | 99.62% | 263.1x |
| realistic | 144.938 ms | 0.716 ms | 99.51% | 202.4x |
| extreme | 896.083 ms | 35.674 ms | 96.02% | 25.1x |

Promote. Realistic tree selection fell from 144.357 ms to 0.933 ms; extreme selection fell from
857.533 ms to 36.077 ms. Diff keeps row backgrounds and character-level emphasis but no longer
performs whole-file syntax coloring. The shared Markdown renderer remains unchanged.
