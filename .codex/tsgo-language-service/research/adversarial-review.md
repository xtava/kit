# Adversarial review: Kit-managed warm `tsgo`

Research is read-only and deliberately hostile to false “warm” claims. The repository already has
the useful primitives: `RepositoryLocator` canonicalizes the start path before walking `.git`
([`src/framework/repository.rs:35-49`](../../../src/framework/repository.rs#L35-L49)); CDP records are
per-user runtime JSON plus Unix sockets ([`src/tools/cdp/registry.rs:141-151`](../../../src/tools/cdp/registry.rs#L141-L151));
and detached process authority is a receipt resolved by systemd invocation identity, not a PID
([`src/framework/process/detached.rs:198-224`](../../../src/framework/process/detached.rs#L198-L224)).
`tsgo` is currently `/home/tvx/Desktop/projects/modular/node_modules/.bin/tsgo`, reporting
`7.0.0-dev.20260418.1`; invocation is `tsgo --lsp --stdio`.

## Architecture attack and recommendation

| Candidate | What breaks under adversarial load | Verdict |
|---|---|---|
| One global `tsgo` for all workspaces | A single TypeScript project graph, compiler options, module resolution, and open-document set cannot safely represent unrelated roots. A client can observe another workspace's files; one crash or incompatible version invalidates every client. Version-keyed routing becomes impossible if roots require different `tsgo`. | Reject. Violates workspace isolation and makes trust boundary global. |
| One Kit daemon + one `tsgo` per workspace | Smallest ownership graph: one durable record/socket owns exactly one child and one canonical root/version. Requests serialize through one LSP pump while independent records run concurrently. | **Choose.** Keep daemon as the sole lifecycle owner; Kit commands are clients. |
| Broker with workspace-scoped children | Broker adds a second liveness and routing authority, fan-out backpressure, child registry, and crash-recovery matrix. It is only justified by a demonstrated need for one broker process to amortize many clients; the per-workspace daemon already does that. | Defer/non-goal. Reconsider only after measured client/daemon startup overhead. |

The selected identity is `(canonical_workspace_root, resolved_tsgo_realpath, exact_version)` and the
record contains a random instance id, start time, request count, child receipt/identity, socket path,
and protocol/schema version. A query must match all identity fields; never attach by name or PID alone.

## Findings (each is an adversarial acceptance condition)

### A1 — workspace and symlink collision (high severity, high confidence)

* Evidence/inference: `RepositoryLocator` canonicalizes (`repository.rs:35-42`), but callers may pass
  a file, symlink, or deleted path. The identity requirement therefore needs canonical root plus a
  stable `tsconfig`/workspace probe; a global process cannot enforce it.
* Trigger: two clients use a symlink and real path, or rename the root while a request is in flight.
* Broken invariant/user symptom: one client receives symbols from the other workspace, or a stale
  daemon is reused after a move.
* Safe behavior/detector: resolve existing directory, canonicalize once, reject missing/non-directory;
  compare stored canonical root and inode/device (where available) on connect. Test symlink aliases and
  rename/delete between status and query.
* Owner/revision: workspace resolver/registry; record canonical root and an explicit “root changed”
  error, never silently retarget.

### A2 — executable/version drift (high, high)

* Evidence: baseline is a repository-local shim (`node_modules/.bin/tsgo`) and currently reports a
  dev build. PATH lookup or a replaced symlink can select another binary.
* Trigger: install/upgrade TypeScript while a record remains, or two roots have different `tsgo`.
* Broken invariant/user symptom: protocol capability or result changes mid-session; query appears warm
  but is backed by a different server.
* Safe behavior/detector: resolve realpath and `--version` before start, persist both, and refuse reuse
  on mismatch; status returns both identities. Test replacing the shim and a fake version command.
* Owner/revision: resolver and record schema; do not accept a caller-supplied version as proof.

### A3 — start stampede (high, high)

* Trigger: two first queries race on absent record/socket.
* Broken invariant/user symptom: two `tsgo` children compete, one record overwrites the other, and a
  query is routed to a process whose initialization belongs to another client.
* Safe behavior/detector: atomic create/lock keyed by identity; loser waits for a ready record and then
  verifies instance id, child identity, and initialize completion. Stress two simultaneous first calls;
  assert one start and two successful replies.
* Owner/revision: registry/start transaction; use atomic files/locks (CDP `write` is currently a plain
  write at `registry.rs:182-187`, so do not copy it without an atomic publication policy).

### A4 — LSP bidirectionality deadlock (critical, high)

* Trigger: `tsgo` sends `workspace/configuration`, `client/registerCapability`, diagnostics, or progress
  while Kit is waiting synchronously for a response.
* Broken invariant/user symptom: call-hierarchy request hangs despite a live child.
* Safe behavior/detector: one framed reader demultiplexes responses, notifications, and server requests;
  route supported requests to deterministic answers and reject/record unsupported methods. Use a fixture
  that sends an unsolicited request before the result; timeout every correlation id.
* Owner/revision: LSP pump, not command handlers; bounded queues and explicit request deadlines.

### A5 — correlation and concurrent clients (critical, high)

* Trigger: two clients issue overlapping hierarchy requests; ids collide or responses are delivered in
  arrival order.
* Broken invariant/user symptom: result A is printed for request B, or one client waits forever.
* Safe behavior/detector: daemon allocates namespaced monotonically increasing ids, keeps a map with
  deadline/cancellation, and writes only the matching client response. Required verifier must assert
  two concurrent clients receive their own sentinel results, not merely exit 0.
* Owner/revision: daemon protocol boundary; include request id and instance id in structured output.

### A6 — initialize/teardown ordering (high, high)

* Trigger: query before `initialize`/`initialized`, `shutdown` races an active request, or process is
  killed without `exit`.
* Broken invariant/user symptom: child emits “server not initialized”, remains as a zombie, or next
  query attaches to a half-closed socket.
* Safe behavior/detector: explicit state machine `Starting -> Initializing -> Ready -> Stopping ->
  Exited`; send `shutdown`, await response, then `exit`, close socket, and reap receipt. Stop test must
  prove both daemon and child terminal, not just socket deletion.
* Owner/revision: daemon lifecycle owner; persist terminal reason and cleanup only after authority proof.

### A7 — document synchronization and file edits (critical, high)

* Trigger: caller changes a TypeScript file between queries, sends `didChange` out of order, or closes
  a document that was never opened.
* Broken invariant/user symptom: second result is unchanged or reflects a phantom buffer; restarting
  accidentally masks the bug.
* Safe behavior/detector: maintain per-document versioned text; send `didOpen` once, incremental/full
  `didChange` in order, `didClose` on eviction; use filesystem watcher or explicit content API and flush
  before query. Verification edits a real file and proves same instance/child plus changed result.
* Owner/revision: workspace daemon document store; define whether file watching is debounce-based and
  expose pending-sync count in status.

### A8 — stale socket/registry and PID reuse (critical, high)

* Trigger: crash leaves JSON/socket, socket inode is replaced, or an old PID is reused.
* Broken invariant/user symptom: Kit connects to an unrelated process, reports “warm” for a dead child,
  or deletes a newly-created record during GC.
* Safe behavior/detector: connect, perform authenticated hello containing instance id and schema, and
  validate receipt authority plus child start identity; on mismatch quarantine/remove only the stale
  record/socket. Never signal by PID. Test fabricated stale files and a replacement socket.
* Owner/revision: registry reconciliation; reuse `ProcessSupervisor` receipt inspection semantics,
  not CDP's filename-only `remove` (`registry.rs:246-249`).

### A9 — crash loop and false recovery (high, medium)

* Trigger: `tsgo` exits immediately (bad config, unsupported flag), and every query blindly respawns.
* Broken invariant/user symptom: CPU spike and an apparently successful first query that actually timed
  out; repeated crash loop hides actionable stderr.
* Safe behavior/detector: bounded restart budget with exponential backoff/circuit breaker; retain last
  bounded stderr and terminal report; status says `crashed` and next explicit query may reset it.
* Owner/revision: daemon supervisor; proposal must specify restart policy and a test with a crashing fake.

### A10 — idle timeout race (high, medium)

* Trigger: idle timer fires while a request, document flush, or new client is queued.
* Broken invariant/user symptom: in-flight call is cut off or the next client sees a dead instance.
* Safe behavior/detector: lease/refcount plus timer generation; stop only after no active requests,
  no pending sync, and an atomic recheck. Test query exactly at timeout boundary.
* Owner/revision: daemon lifecycle; make idle timeout configurable and report `last_activity`/active count.

### A11 — permissions and trust (critical, high)

* Trigger: runtime directory is group/world readable, a sibling user connects, or workspace content
  includes malicious compiler/plugin configuration.
* Broken invariant/user symptom: source text/diagnostics leak or an attacker asks the daemon to read
  arbitrary files/execute project plugins.
* Safe behavior/detector: per-user runtime dir and socket mode 0600, refuse insecure parent; peer UID
  check where supported; do not expose TCP; constrain root and document that `tsgo` executes with the
  caller's privileges. Test mode/ownership and a second-user connect denial.
* Owner/revision: socket/launch boundary; proposal needs explicit trust model and plugin/config policy.

### A12 — output instability and protocol leakage (medium, high)

* Trigger: human text is parsed by automation, server logs interleave with JSON, or an LSP error is
  swallowed.
* Broken invariant/user symptom: scripts break and users cannot distinguish stale, failed, or partial
  hierarchy results.
* Safe behavior/detector: all management/query commands have stable structured JSON fields (`ok`,
  `instance_id`, `started_at`, `request_count`, `child`, `state`, `error`); stderr is bounded and never
  mixed into stdout. Snapshot the JSON schema and malformed-frame behavior.
* Owner/revision: framework output adapter plus tsgo tool policy; avoid bespoke human-only status.

### A13 — stop/GC semantic split (high, high)

* Trigger: `stop` deletes registry first, fails to reap, then `gc` cannot recover; or GC kills a live
  process based on age.
* Broken invariant/user symptom: orphan `tsgo`, “stopped” lie, or another client's active service is
  removed.
* Safe behavior/detector: stop is an idempotent receipt-authorized operation: mark stopping, request
  graceful shutdown, force after bounded grace, await/reconcile child, then atomically remove record
  and socket. GC only removes records proven terminal/stale by receipt + hello, never guesses from PID.
* Owner/revision: lifecycle/registry owner; command vocabulary can be `query`, `inspect`, `stop`, and
  `prune` (GC only if stale artifacts are observable). Do not add `warm` as a separate lifecycle verb.

### A14 — broker overreach (medium, high)

* Trigger: implementation adds a central broker “for reuse” before measuring per-workspace daemon cost.
* Broken invariant/user symptom: broker crash takes all workspaces down; routing and child cleanup become
  a second state machine.
* Safe behavior/detector: reject broker in first cut; benchmark client-to-daemon startup only after the
  per-workspace proof passes.
* Owner/revision: proposal scope gate; document broker as deferred, not an unimplemented hidden path.

### A15 — verification false positive (critical, high)

* Trigger: tests compare only PIDs, issue a second request before the first service is persisted, or
  restart the daemon after editing the file.
* Broken invariant/user symptom: claimed warm reuse while CPU/startup regression remains in production.
* Safe behavior/detector: every reply carries instance id, daemon start time, request count, and child
  identity (receipt/run id plus child start evidence). The acceptance script must execute through `kit`
  commands: first query, second query, edit, two concurrent clients, stop, later replacement, and stale
  record/socket recovery.
* Owner/revision: integration verifier and proposal; no goal completion from unit tests alone.

## Scope cuts and command vocabulary

Keep one public operation family, chosen after research rather than assuming names:

* `kit tsgo query <symbol> [--workspace <path>]` lazily starts/reuses and returns hierarchy data;
* `kit tsgo inspect [--workspace]` reports state, identity, version, child receipt, counters, and
  pending synchronization;
* `kit tsgo stop [--workspace]` performs graceful, receipt-authorized teardown;
* `kit tsgo prune` is optional and only sweeps proven stale artifacts (otherwise fold it into
  `inspect --repair`).

`warm`, `status`, and `hierarchy` are intentionally not separate mandatory verbs: “warm” is an
implementation property, status is `inspect`, and hierarchy is the query payload. No TCP listener,
global server, cross-user service, remote workspaces, arbitrary LSP proxy, project-wide watcher,
automatic dependency installation, compiler plugin sandbox, or broker belongs in the first cut. File
watching should be limited to opened/queried TypeScript documents with explicit synchronization; broad
indexing and every LSP method are non-goals.

## Decision capsule

Choose one receipt-owned Kit daemon and one exact-version `tsgo` child per canonical workspace. Make
the daemon the sole LSP pump and lifecycle state machine; expose only query/inspect/stop plus proven
stale cleanup. Atomic identity matching, bidirectional framing, versioned document sync, and receipt-
based reconciliation are prerequisites. The proposal must include the seven end-to-end verifiers and
must not claim completion until they pass through the real Kit command path.

## Decision capsule
