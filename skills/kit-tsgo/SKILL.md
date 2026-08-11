---
name: kit-tsgo
description: >-
  Trace and inspect semantic TypeScript call graphs with `kit tsgo`, a warm
  workspace-scoped native TypeScript 7 language-service daemon. Use when an
  agent needs to identify a function's callers or callees, trace upward toward
  entry points, understand downstream implementation calls, inspect callsites,
  assess change impact, follow converging or recursive call chains, verify that
  edits changed the semantic graph, or manage the reusable tsgo service. Accepts
  semantic symbols or exact UTF-16 source positions and supports structured JSON.
license: MIT
metadata:
  source: https://github.com/xtava/kit
---

# kit tsgo — trace a function through the semantic call graph

`kit tsgo` keeps one native `tsgo --lsp --stdio` child warm per canonical Git
worktree and exact server version. The first trace starts the service lazily;
later traces reuse the same daemon and native child. Use Kit for the entire
lifecycle—never launch, signal, or clean the language server by hand.

Requires `kit` on `PATH` and a native `tsgo` launcher. By default Kit resolves
`<worktree>/node_modules/.bin/tsgo`; use `--tsgo` only to select an intentional
exact launcher.

## Start with the function

When the semantic name is known:

```bash
kit tsgo trace --symbol 'CheckoutService.submit' --direction callers
```

When only the source location is known:

```bash
kit tsgo trace \
  --at src/checkout/service.ts \
  --line 141 \
  --character 16 \
  --direction callers
```

`--line` and `--character` are zero-based UTF-16 positions. A file alone never
identifies a function.

## Choose the direction by the question

- `--direction callers`: “What can lead to this function?” The tree starts at
  the target and traces outward toward semantic entry points.
- `--direction callees`: “What can this function call?” The tree starts at the
  target and traces outward through its implementation dependencies.

Callers are the usual choice for tracing a feature, command, request, tool, or
event back through its possible production entry paths. Callees are the usual
choice for understanding implementation and change impact below a function.

## Resolve ambiguity; never guess

Prefer a container-qualified name such as `ReadTool.readFile`. Narrow a common
name to a canonical workspace subpath with `--in`:

```bash
kit tsgo trace \
  --symbol readTextFile \
  --in packages/tvx/ai/src/tools/builtin/read/read-text.ts \
  --direction callers
```

If Kit returns `ambiguous`, inspect the candidates and retry with a qualified
name, narrower `--in`, or exact `--at` coordinates. Never select the first
candidate merely because it is first.

## Read the merged tree precisely

The selected function is `[1]`. Every acquired caller→callee edge appears once.
Kit expands each semantic node once, so large converging graphs remain readable:

- `[n]`: first occurrence; descendants expand here.
- `↩ [n]`: another edge to an already expanded node.
- `⇄ [n]`: an edge to a node active in the current recursive branch.
- `endpoint`: a genuine semantic end in the requested direction.
- `external`: a dependency outside the canonical workspace.
- `max-depth`: relations omitted by the depth guard.
- `max-nodes`: relations omitted by the node guard.

A boundary is not an endpoint. A static call graph is not a sampled runtime
stack: it describes possible TypeScript call relationships, including tests,
polymorphic dispatch, and branches that did not execute in any particular run.

## Use JSON for evidence

```bash
kit tsgo trace --symbol 'CheckoutService.submit' --direction callers --json
```

Treat `result.nodes` and `result.edges` as canonical. Use
`result.endpoints`, `result.boundaries`, and `result.cycle_components` to
distinguish completeness, cuts, and recursion. The top-level `ascii` field is
the same colorless tree shown in text mode.

Every trace also returns daemon-owned reuse evidence under `service`:

```text
protocol_version · instance_id · started_at_ms · request_count
child.run_id · child.generation · child.started_at_ms · child.server_version
```

Reuse means the instance/start and child identity remain unchanged while
`request_count` increases. PID equality alone is not proof.

## Keep the service warm

There is no warm-up command. Trace directly. Inspecting never starts a service:

```bash
kit tsgo inspect
kit tsgo stop
kit tsgo prune
```

- `inspect`: query live services and report stale owned state.
- `stop`: request graceful shutdown and reap the owned process.
- `prune`: remove stale registry/socket state while retaining live services.

Use `--workspace <path>` when operating outside the intended worktree. Use
`--all` only for deliberate fleet-wide management. Never kill a discovered PID,
delete a socket manually, or run `tsgo --lsp --stdio` beside Kit's owner.

## Agent operating rules

1. Trace the requested function directly; do not perform a separate warm-up.
2. Prefer semantic names, then exact positions when names remain ambiguous.
3. Begin with default limits; raise only the guard shown in structured cuts.
4. Use `--json` when comparing runs or claiming graph/reuse evidence.
5. Do not modify the target repository merely to make a query easier.
6. Do not claim dynamic runtime execution from static call-hierarchy evidence.
7. Stop only when teardown or a clean replacement is part of the task.

## Going deeper

- [references/commands.md](references/commands.md) — complete public command,
  selector, limit, output, and lifecycle reference.
- [references/recipes.md](references/recipes.md) — focused workflows for feature
  tracing, implementation exploration, ambiguity, reuse, live edits, large
  graphs, recovery, and Modular's Read tool.
