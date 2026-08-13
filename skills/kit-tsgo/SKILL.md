---
name: kit-tsgo
description: >-
  Diagnose explicit TypeScript documents, run authoritative project type
  checks, trace semantic call graphs, and capture replayable placement evidence
  with `kit tsgo`. Use for fast post-edit feedback, project handoff gates,
  callers, callees, definitions, references, implementations, converging call
  witnesses, explicit evidence gaps, change-impact evidence, or bounded source
  anchors before choosing where to edit. Supports structured JSON.
license: MIT
metadata:
  source: https://github.com/xtava/kit
---

# kit tsgo — diagnostics, project checks, and semantic evidence

`kit tsgo` keeps one native `tsgo --lsp --stdio` child warm per canonical Git
worktree and exact server version. The first trace, locus, or diagnose request
starts the service lazily; later queries reuse the same daemon and native child.
Authoritative `check` is deliberately separate: it runs one supervised compiler
process and never pretends that document diagnostics prove project correctness.
Use Kit for the entire lifecycle—never launch, signal, or clean the language
server by hand.

Requires `kit` on `PATH` and a native `tsgo` launcher. By default Kit resolves
`<worktree>/node_modules/.bin/tsgo`; use `--tsgo` only to select an intentional
exact launcher.

## Use the two-stage edit gate

During an edit loop, request warm diagnostics for exactly the files changed:

```bash
kit tsgo diagnose src/cart.ts src/checkout.ts --json
```

This returns `no-local-diagnostics`, `local-diagnostics`, or `incomplete`. Even
zero diagnostics means only the explicit documents were checked; the result
always declares that workspace diagnostics are unavailable.

Before handoff, run the authoritative project compiler:

```bash
kit tsgo check --project tsconfig.json --json
```

Omit `--project` to use the nearest `tsconfig.json`. `check` first reads the
effective config, then runs the exact native launcher with checking forced on,
emission disabled, and incremental state redirected into Kit's private process
workspace. Exit 0 means only `compiler-reported-no-diagnostics` for that root
project at invocation time. Input freshness is explicitly unchecked, and any
project references make the result incomplete because solution closure is not
part of v1.

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

## Compare inspect anchors without ranking them

Use `locus` when a feature request spans several plausible seams and the next
question is “what should I inspect before deciding where to edit?”:

```bash
kit tsgo locus --case placement.case.json --workspace . --json
```

`locus` is an experimental evidence surface. Do not present it as a placement
recommender or treat `evidence-ready` as a promotion decision.

A locus case declares:

- semantic seeds;
- required obligations;
- bounded native acquisitions;
- allowed candidate-discovery rules;
- explicit non-TypeScript evidence gaps.

The result may be `blocked`, `investigation-required`, `no-candidate`, or
`evidence-ready`. `evidence-ready` means only that the declared capture is
complete enough to inspect its returned anchors. It is not a recommendation,
score, or proof of runtime behavior. A cut, unsupported operation, ambiguous
seed/call item, changed input, or declared gap stays explicit rather than being
converted into negative evidence.

Use [references/locus.md](references/locus.md) for the case contract and a
minimal replayable example.

## Keep the service warm

There is no warm-up command. Query directly. Inspecting never starts a service:

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

1. Use `diagnose` for fast explicit-file feedback and `check` for the native root-project gate.
2. Trace the requested function directly; do not perform a separate warm-up.
3. Prefer semantic names, then exact positions when names remain ambiguous.
4. Begin with default limits; raise only the guard shown in structured cuts.
5. Use `--json` when comparing runs or claiming diagnostic, graph, or reuse evidence.
6. Do not modify the target repository merely to make a query easier.
7. Do not claim dynamic runtime execution from static call-hierarchy evidence.
8. Stop only when teardown or a clean replacement is part of the task.
9. For `locus`, inspect only returned anchors and keep every open obligation or
   declared gap visible in the handoff.
10. Treat exit 2 and every `operational-failure` outcome as unresolved evidence,
    never as an empty successful check.

## Going deeper

- [references/commands.md](references/commands.md) — complete public command,
  selector, limit, output, and lifecycle reference.
- [references/recipes.md](references/recipes.md) — focused workflows for feature
  tracing, implementation exploration, ambiguity, reuse, live edits, large
  graphs, recovery, and Modular's Read tool.
- [references/locus.md](references/locus.md) — typed placement-evidence cases,
  status semantics, discovery rules, completeness, and replay receipts.
