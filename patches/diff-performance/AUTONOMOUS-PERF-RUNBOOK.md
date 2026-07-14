# `kit diff` performance runbook

## Preflight

Run these separately before a release benchmark:

```text
pgrep -a -x cargo
pgrep -a -x rustc
ps -eo pid,pcpu,pmem,comm,args --sort=-pcpu | head -n 16
```

Do not overlap Cargo, rustc, another benchmark, packaging, or installation. External machine load is
recorded with the run, but deterministic control and experiment results are comparable only when no
competing build is active.

## Control and experiments

1. Record the commit, dirty Diff path hashes, toolchain, command, and host load.
2. Run the ignored release lifecycle harness twice without changing product code.
3. Apply exactly one product hypothesis.
4. Run focused Diff correctness tests, then the identical release harness twice.
5. Record p50, p95, maximum, absolute delta, and percentage delta for every fixture.
6. Reject the experiment if correctness fails, realistic/extreme p95 does not improve by at least
   30%, or ordinary p95 regresses by more than 10%.
7. Preserve a reversible product patch and confirm `git apply -R --check` succeeds.

## Verification

The deterministic harness covers CPU-side input, projection, Ratatui diffing, and frame production.
It does not measure terminal-emulator flush or display latency. A promoted change therefore also
requires a live `kit diff` scroll sanity check in a large real repository.
