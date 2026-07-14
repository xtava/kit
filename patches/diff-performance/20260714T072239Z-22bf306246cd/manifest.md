# Diff performance control

- Commit: `22bf306246cd3e065ea6c802dbd3dad23e42001d`
- Diff TUI SHA-256: `92e223bbfac22813cbfe4e08780fdb5b16fee9c584f93b0e41c1041c04796648`
- Diff docs SHA-256: `797787119d98e5291185f6e2bea9a0844724fc0d653b20c0c10e244d0c62ff48`
- Toolchain: `rustc 1.92.0 (ded5c06cf 2025-12-08)`
- Host: Linux 7.0.9-arch1-1 x86_64, 32 logical CPUs
- Command: `cargo test --release -j 2 tools::diff::tui::tests::benchmark_scroll_event_to_frame -- --ignored --nocapture`
- Repetitions: 2
- Competing Cargo/rustc: none
- External load: Chrome remained near 60% CPU; one-minute load was 0.55 before the build and 2.40 immediately after the second run.
- Result: valid deterministic control. The two runs were within 8.5% at realistic p95 and 1.0% at extreme p95. Ordinary p95 had one 10.1 ms warm-build outlier run, so promotion comparisons use the worse control value.
