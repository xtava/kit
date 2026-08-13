# `kit tsgo` recipes

## Run the agent edit loop

After editing a bounded set of TypeScript files, get warm local feedback:

```bash
kit tsgo diagnose src/a.ts src/b.ts --json
```

Use the returned diagnostics immediately, but do not describe
`no-local-diagnostics` as project success. At handoff, run the configured
project gate:

```bash
kit tsgo check --project tsconfig.json --json
```

If either command exits 2, resolve its explicit freshness, coverage, output,
capability, or operational reason instead of treating an empty diagnostics
array as success. Exit 0 from `check` means the compiler reported no diagnostics
for one nonempty root project at invocation time; it does not prove project-
reference closure or a replayable input snapshot.

## Trace a feature upward

Use callers when the question is “How can this function be reached?”

```bash
kit tsgo trace \
  --symbol 'CheckoutService.submit' \
  --direction callers
```

Read from `[1]` outward. Continue through real functions until endpoints,
external boundaries, cycles, or typed cuts. Do not stop interpretation at a
class name or guessed runner category.

## Explore implementation calls

Use callees when the question is “What does this function depend on?”

```bash
kit tsgo trace \
  --symbol 'CheckoutService.submit' \
  --direction callees
```

Use the result for implementation orientation and static impact assessment.
Confirm behavioral execution separately when runtime truth matters.

## Narrow an ambiguous name

Start semantically:

```bash
kit tsgo trace --symbol run --direction callers --json
```

If `status` is `ambiguous`, use returned candidate details instead of choosing
the first item:

```bash
kit tsgo trace \
  --symbol 'CodexAppServerRunner.run' \
  --in packages/tvx/ai \
  --direction callers
```

If qualification remains unclear, target the declaration position.

## Target an exact function

```bash
kit tsgo trace \
  --at src/service.ts \
  --line 140 \
  --character 9 \
  --direction callers
```

Coordinates must point at the symbol and use zero-based UTF-16 units. Derive
them from the editor/LSP position when non-ASCII text makes byte columns unsafe.

## Prove warm reuse

Capture two equivalent structured traces:

```bash
kit tsgo trace --symbol 'CheckoutService.submit' --direction callers --json
kit tsgo trace --symbol 'CheckoutService.submit' --direction callers --json
```

Require equality for:

```text
service.protocol_version
service.instance_id
service.started_at_ms
service.child.run_id
service.child.generation
service.child.started_at_ms
```

Require the second `service.request_count` to be larger. PIDs are neither
necessary nor sufficient.

## Find a shared semantic seam

When two feature paths should converge, create a locus case with one seed and
one bounded call acquisition per path. Use `call-witness-intersection` with both
seed IDs, then run:

```bash
kit tsgo locus --case shared-seam.case.json --workspace . --json
```

Inspect only anchors that carry a witness from every declared path. If any
participating acquisition is cut and `require_complete` is true, the discovery
receipt stays incomplete and returns no intersection. Do not replace the
missing witness with a textual match.

## Preserve a non-TypeScript gap

If static TypeScript operations cannot prove an event-to-reducer transition,
DI registration, generated-code edge, resource flow, or runtime observation,
declare that gap in the case and attach it to an obligation. The result remains
`investigation-required` until the gap is resolved outside this tool. Do not
pretend a nearby definition or reference closes it.

## Verify a live edit

When the user's task already authorizes editing a participating TypeScript
file:

1. Capture the relevant trace with `--json`.
2. Make the intended source edit.
3. Run the same trace again.
4. Confirm nodes, edges, callsites, or endpoints changed as expected.
5. Confirm daemon and child identities stayed unchanged.

Do not create source edits solely to exercise the service unless the repository
is an explicit disposable fixture or the user authorized a temporary verifier.

## Expand a cut graph

Start with defaults. If JSON reports a `max-depth` boundary:

```bash
kit tsgo trace \
  --symbol 'CheckoutService.submit' \
  --direction callers \
  --max-depth 20 \
  --json
```

If it reports `max-nodes`, raise only `--max-nodes`. Stay within the public
maximums. There is no path limit because Kit does not enumerate paths.

## Recover stale state

Inspect first without starting anything:

```bash
kit tsgo inspect --workspace /path/to/worktree --json
```

Ordinary traces already perform one owned recovery attempt. If stale state
remains:

```bash
kit tsgo prune --workspace /path/to/worktree --json
```

Never remove the socket, registry, process, or PID manually.

## Request a clean replacement

```bash
kit tsgo stop --workspace /path/to/worktree --json
kit tsgo inspect --workspace /path/to/worktree --json
kit tsgo trace --symbol 'CheckoutService.submit' --direction callers --json
```

Require graceful/reaped stop evidence, an empty post-stop inspect, and new
instance/child identities on the replacement trace.

## Trace Modular's Read tool

From `/home/tvx/Desktop/projects/modular`:

```bash
kit tsgo trace \
  --symbol readTextFile \
  --in packages/tvx/ai/src/tools/builtin/read/read-text.ts \
  --direction callers \
  --max-depth 18 \
  --max-nodes 512
```

The merged tree should continue through actual Read/executor/router functions
and expose Codex, graph, Claude, Orchestrate, and direct invocation branches
when those edges are present in the acquired semantic graph. Shared tails use
`↩ [n]`; recursion uses `⇄ [n]`.

For the canonical Modular-specific guide, see
`docs/canonical/kit/tsgo-call-tracing.md` in the Modular worktree.

## Hand off evidence

For an engineering report, retain:

- exact command and canonical workspace;
- selector and direction;
- status, summary, boundaries, and cycles;
- target node and relevant normalized edges/callsites;
- complete ASCII tree or a clearly labeled excerpt;
- service/child identity and request-count evidence;
- teardown result when teardown was required.

State explicitly that the graph is static TypeScript semantic evidence rather
than a captured runtime stack.
