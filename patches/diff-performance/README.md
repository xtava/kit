# Diff performance evidence

This directory records reproducible `kit diff` lifecycle measurements. The TUI harness is the
ignored `tools::diff::tui::perf_tests::benchmark_diff_lifecycle` test; repository loading has a
separate ignored `tools::diff::git::tests::benchmark_repository_load` test.

The control and every experiment use the same three pre-built fixtures:

| Case | Source lines | Changed files | Terminal |
| --- | ---: | ---: | --- |
| ordinary | 250 | 25 | 100x32 inline |
| realistic | 5,000 | 250 | 160x50 split |
| extreme | 25,000 | 2,000 | 160x50 split |

The lifecycle harness isolates model construction, document projection, tree construction, warm
frames, vertical scrolling, and tree selection. Fixture construction happens outside measurement.

The repository benchmark measures the complete pre-TUI load: repository discovery, status parsing,
source retrieval, and diff-model construction. Its retained control p95 is 39.694 ms for 25 files,
387.217 ms for 250 files, and 1.530 s for 1,000 files. This is now the dominant time-to-visible path;
the optimized projection and warm-frame paths are sub-millisecond on the realistic fixture.

Run the harness with:

```bash
cargo test --release -j 2 tools::diff::tui::perf_tests::benchmark_diff_lifecycle -- --ignored --nocapture
```

Raw JSON lines and experiment verdicts live in timestamped subdirectories. Product experiments must
preserve review semantics and improve realistic or extreme p95 latency by at least 30% without
regressing the ordinary p95 by more than 10%.
