# Tsgo Language Service Performance Baseline

Date: 2026-08-10
Workspace: `/home/tvx/Desktop/projects/modular`
Kit: installed `/home/tvx/.local/bin/kit`, version `0.1.0`
Server: `@typescript/native-preview` `7.0.0-dev.20260418.1`

## Measurement boundary

- CPU: `perf stat` for the foreground Kit command and descendants, plus cgroup-v2
  `cpu.stat` / systemd `CPUUsageNSec` for the detached Kit daemon and native `tsgo` child.
- Memory: service cgroup `memory.current` and `memory.peak`.
- Latency: `perf` `duration_time`, measured around the installed public command.
- Cold: five fully stopped first-query cycles. Warm/hot: one retained service.
- Temporary untracked Modular fixtures: 4 functions, 50-callee fan-out, and an intentional
  500-callee upper bound. All are removed after measurement.
- Host: AMD Ryzen 9 9950X3D, 16 cores / 32 threads, 91 GiB RAM, Linux 7.0.9,
  `amd-pstate-epp` with `powersave`; starting load average 2.19 / 2.44 / 2.93.

## Baseline summary

| Operation | Samples | Wall median | Wall p95 | Combined CPU | Memory |
| --- | ---: | ---: | ---: | ---: | ---: |
| Cold first `prepare` | 5 | 390.0 ms | 411.1 ms | 3,793.7 ms median | 949.9 MiB peak median |
| Hot `prepare` | 30 | 20.3 ms | 35.9 ms | 23.0 ms/query mean | 1,149 MiB current |
| Hot ordinary `outgoing` (2 callees) | 20 | 21.4 ms | 37.5 ms | 23.6 ms/query mean | 1,156 MiB current |
| First realistic `outgoing` (50 callees) | 1 | 91.8 ms | — | 357.1 ms | 1,163 MiB peak |
| Hot realistic `outgoing` (50 callees) | 10 | 22.6 ms | 26.3 ms | 26.0 ms/query mean | 1,163 MiB current |
| Concurrent pair of hot prepares | 10 pairs | 22.5 ms/pair | 40.2 ms/pair | 47.3 ms/pair mean | 1,255 MiB current after upper-bound open |
| Warm `inspect` | 20 | 3.1 ms | 4.3 ms | 5.9 ms/query mean | no material change |
| Warm `stop` | 1 | 41.7 ms | — | 11.6 ms foreground only | service reaped |
| Idle service | 10 s | — | — | **0 µs cgroup CPU delta** | unchanged |

Cold versus hot `prepare`:

- 19.2× lower median wall latency (94.8% saved).
- 164.7× lower combined CPU per query (99.39% saved).
- Cold startup used 3.33–4.16 CPU-seconds in the service during a 350–411 ms response,
  averaging roughly 9.6 fully utilized cores at the median.

## Raw cold samples

| # | Wall ms | Foreground CPU ms | Service CPU ms | Combined CPU ms | Peak MiB | Tasks |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 377.46 | 28.06 | 3,327.62 | 3,355.68 | 957.36 | 139 |
| 2 | 389.99 | 28.81 | 3,799.51 | 3,828.32 | 916.07 | 140 |
| 3 | 411.13 | 30.58 | 4,160.57 | 4,191.15 | 949.91 | 140 |
| 4 | 349.77 | 27.58 | 3,620.60 | 3,648.18 | 940.30 | 138 |
| 5 | 401.22 | 31.56 | 3,762.14 | 3,793.70 | 950.63 | 141 |

The one-second settled reading changed service CPU by at most 0.18 ms, so the measured cold
spike completed before the public result returned.

## Steady-state ownership

- Hot `prepare` service CPU: 0.54 ms/query; foreground Kit CPU: 22.49 ms/query.
- Hot ordinary `outgoing` service CPU: 0.74 ms/query; foreground Kit CPU: 22.83 ms/query.
- Direct `tsgo --version`: 15.38 ms median wall, 15.06 CPU-ms, about 123.0 million
  instructions. It dominates the steady foreground query cost because exact version resolution
  currently runs on every query.
- Bare Kit process floor: 2.52 ms median wall and 4.20 CPU-ms.
- Warm process shape after loading Modular: native `tsgo` about 1.15 GiB RSS with 46 threads;
  total service cgroup about 146 tasks including Kit/process supervision.
- Ten one-second `pidstat` intervals reported 0.00% CPU for the Kit owners, and the recursive
  service cgroup `usage_usec` did not change at all while idle.

## Concurrency

Two simultaneous hot prepares completed in 22.52 ms median per pair, close to the 20.28 ms
single-query median. Both commands exited zero. This is about 88.8 queries/second at median pair
latency versus 49.3 queries/second serially, with essentially linear CPU cost per query.

## Upper-bound failure

The intentional 500-callee outgoing hierarchy did not pass the public surface:

```text
Error: query replacement tsgo service
Caused by: tsgo service reply exceeded 64 KiB
```

It failed after 100.84 ms wall and 372.77 combined CPU-ms, and raised the service peak to
1,263.39 MiB. The native result was computed, but the Kit client rejected the serialized reply.
The ceiling is owned by `REGISTRY_FILE_LIMIT` being reused for socket replies in
`src/tools/tsgo/mod.rs`. Therefore the feature is verified for ordinary and 50-callee realistic
results, but not for large hierarchies above the 64 KiB response limit.

## Full call-chain traversal

A controlled Modular fixture defined a linear static hierarchy from `chain00` through `chain11`.
The verifier followed all 11 outgoing edges through the installed public Kit command, and every
response named the exact next function. Each accepted mode was measured three times:

| Service mode | Median wall | Median combined CPU | Identity proof |
| --- | ---: | ---: | --- |
| Restart before every hop | 3,981.3 ms | 41,672.9 ms | 11 daemon instances and 11 native children per trace |
| Lazy start, then reuse | 667.1 ms | 5,677.9 ms | one instance/child; request count reached 11 |
| Already-hot service | 326.9 ms | 1,925.5 ms | one instance/child; request count advanced from 1 to 12 |

The already-hot trace was not a result-cache benchmark: a single `prepare` warmed the process, then
the verifier queried 11 distinct function positions for the first time. The lazy trace included
native startup in its first hop. The restart control stopped through Kit outside each measured query,
so its totals count repeated query startup but exclude stop latency.

Raw accepted samples (wall / combined CPU, milliseconds):

| Sample | Restart each hop | Lazy start and reuse | Already hot |
| ---: | ---: | ---: | ---: |
| 1 | 4,003.8 / 42,414.8 | 609.4 / 5,530.1 | 327.6 / 1,925.5 |
| 2 | 3,981.3 / 41,672.9 | 667.1 / 5,746.2 | 326.9 / 1,962.8 |
| 3 | 3,902.3 / 41,373.7 | 676.6 / 5,677.9 | 296.2 / 1,103.6 |

Against restart-per-hop, an already-hot service made the complete trace **12.2× faster in wall
time** (91.8% saved) and used **21.6× less combined CPU** (95.4% saved). Even including the first
lazy startup, one reusable process was 6.0× faster and used 7.3× less CPU. The already-hot median
averaged 29.7 ms wall per resolved edge, including a fresh Kit client and exact-version probe for
every hop.

A read-only check on existing Modular source also followed
`parseServerMode → findArgument → Array.indexOf` in
`packages/extensions/vscode.typescript-language-features/web/src/util/args.ts`. Both Kit queries
returned the expected next item while retaining instance
`e91b41ba-a4a6-4b18-93d7-a4b25b896285` and native child
`0ea789a4-5c02-4ab6-a996-1c29ea64c8d6`; request count advanced from 1 to 2. Public stop then
reported `graceful: true`, `reaped: true`, and natural child exit.

## Baseline verdict

- The warm-process objective is quantitatively successful: it removes the large native startup
  spike, makes ordinary queries roughly 20–23 ms at median, and makes an 11-edge call-chain trace
  12.2× faster than restarting the server at every edge.
- Idle CPU is zero in the measured window, but retained memory is large: roughly 1.15 GiB for the
  Modular workspace, growing to 1.26 GiB after the extreme file.
- The next highest-leverage performance hypothesis is avoiding a native `tsgo --version` launch on
  every already-identified warm query while preserving exact executable/version identity.
- The 64 KiB reply ceiling is a correctness/scalability defect, not a performance optimization.
  Fix it before claiming unbounded call-hierarchy support.
