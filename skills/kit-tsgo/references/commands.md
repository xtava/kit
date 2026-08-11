# `kit tsgo` command reference

## Public surface

```text
kit tsgo trace
kit tsgo inspect
kit tsgo stop
kit tsgo prune
```

`__serve` is an internal detached-process entry and must never be called by an
agent or user workflow.

## Trace by semantic name

```bash
kit tsgo trace \
  --symbol 'Container.function' \
  [--in path/inside/worktree] \
  [--direction callers|callees] \
  [--max-depth 12] \
  [--max-nodes 512] \
  [--workspace /canonical/worktree] \
  [--tsgo /exact/path/to/tsgo] \
  [--json]
```

- `--symbol`: exact semantic name, optionally qualified by its container.
- `--in`: canonical file or directory scope inside the worktree; valid only
  with `--symbol`.
- `--direction`: defaults to `callers`.
- `--max-depth`: maximum call edges followed from the selected target; maximum
  accepted value is 64.
- `--max-nodes`: maximum unique semantic nodes; accepted range is 1–4096.
- `--workspace`: resolve the canonical Git worktree from this path.
- `--tsgo`: select an exact executable instead of the worktree default.

Symbol discovery uses bounded textual scanning only to activate candidate
TypeScript projects. Native workspace symbols and call-hierarchy preparation
remain the semantic authority. A truncated discovery cannot prove global
uniqueness.

## Trace by exact position

```bash
kit tsgo trace \
  --at path/inside/worktree.ts \
  --line 141 \
  --character 16 \
  [--direction callers|callees] \
  [--max-depth 12] \
  [--max-nodes 512] \
  [--workspace /canonical/worktree] \
  [--tsgo /exact/path/to/tsgo] \
  [--json]
```

- Positions are zero-based and measured in UTF-16 code units.
- `--line` and `--character` are both required with `--at`.
- The file must canonicalize inside the selected worktree.
- `--at` conflicts with `--symbol`; `--in` applies only to symbols.

## Direction semantics

The canonical graph always stores `caller → callee` edges.

- `callers`: begin at the target and project incoming neighbors outward toward
  semantic entry points.
- `callees`: begin at the target and project outgoing neighbors outward toward
  implementation leaves.

The visual orientation is always target-centered. Do not reverse the printed
tree to imitate a runtime stack.

## Text output

Text contains:

- trace title;
- workspace/direction/status;
- exact resolved target;
- elapsed time;
- daemon and child identity;
- endpoint/node/edge counts;
- cyclic-component/boundary state;
- one merged call tree;
- explicit limit reasons when truncated.

Tree markers:

| Marker | Meaning |
| --- | --- |
| `[n]` | First semantic-node occurrence; expanded here |
| `↩ [n]` | Shared reference to a completed node |
| `⇄ [n]` | Recursive reference to an active node |
| `endpoint` | Complete semantic end in this direction |
| `external` | Node retained but not expanded outside the workspace |
| `max-depth` | Depth cut with omitted relation count |
| `max-nodes` | Node-budget cut with omitted relation count |

Every normalized edge contributes exactly one non-target tree row. One symbol
may therefore appear more than once: one expanded occurrence and later explicit
references.

## Structured output

`--json` returns:

```text
action
service
result
ascii
```

Important `service` fields:

```text
key
protocol_version
instance_id
started_at_ms
request_count
state
workspace
child.run_id
child.generation
child.started_at_ms
child.launcher
child.server_version
```

Important `result` fields:

```text
status
selector
direction
target
candidates
nodes
edges
endpoints
cycle_components
boundaries
summary
timing
discovery
truncation_reasons
```

`nodes` and `edges` are the normalized machine graph. Edge callsites are sorted
and deduplicated. `ascii` is a deterministic projection of that graph, not a
second semantic owner.

## Inspect

```bash
kit tsgo inspect [--workspace /path/to/worktree | --all] [--json]
```

`inspect` never starts a service. It queries the owned service protocol where
possible and reports live, completed, failed, or stale state plus detached
receipt identity.

## Stop

```bash
kit tsgo stop [--workspace /path/to/worktree | --all] [--json]
```

`stop` sends LSP `shutdown`, then `exit`, waits for the native child, terminates
the daemon owner, reconciles its detached receipt, and removes owned state. A
subsequent trace starts a replacement with new instance and child identities.

## Prune

```bash
kit tsgo prune [--workspace /path/to/worktree | --all] [--json]
```

`prune` retains live services. It reconciles stale state through detached
receipts and removes owned registry/socket files without guessing from a PID.

## Status interpretation

| Status | Meaning | Next action |
| --- | --- | --- |
| `complete` | Acquired graph reached semantic ends/boundaries without a guard cut | Read the tree or JSON |
| `truncated` | Discovery, depth, or node guard cut the result | Inspect typed cuts; raise only the relevant guard |
| `ambiguous` | Multiple semantic targets matched | Qualify, narrow `--in`, or use `--at` |
| `not-found` | No semantic target prepared | Check name, scope, project activation, or exact position |

## Trust boundary

- Socket and registry state live under Kit's private user runtime directory.
- Local socket permissions are owner-only.
- Service identity binds canonical workspace, exact launcher, and server version.
- The required protocol version detects an incompatible warm daemon and routes
  replacement through its owned detached receipt.
- Target repositories receive no daemon registry, socket, or cache files.
