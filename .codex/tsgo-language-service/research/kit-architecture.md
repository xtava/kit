# Kit architecture research for an external `tsgo` service

## Scope and evidence method

This is a read-only survey of the Kit checkout. Evidence below is from direct source reads and
the scoped history (`975126d4d cdp: bind daemons to canonical worktrees`, plus recent process
history); no production files were changed. Line references are to the current checkout.

## Verified Kit owners and reusable seams

### Process ownership, receipts, and reaping

* `ProcessSupervisor::bootstrap` chooses Kit's per-user state root (`ProjectDirs`, then
  `processes`), validates private storage, and reconciles abandoned attached runs
  ([`src/framework/process/supervisor.rs:278-318`](../../../src/framework/process/supervisor.rs#L278-L318)).
  Its supervisor instance UUID is internal and prevents a prepared run from being used by another
  supervisor ([`supervisor.rs:311-317`, `442-451`](../../../src/framework/process/supervisor.rs#L311-L317)).
* `spawn`/`spawn_prepared` return a `StartedProcess` containing writable input, output handles, and
  a `ProcessSession`; the latter has a stable `ProcessRunId`, `wait`, and cloneable control
  ([`supervisor.rs:432-451`](../../../src/framework/process/supervisor.rs#L432-L451),
  [`session.rs:15-31`, `69-119`](../../../src/framework/process/session.rs#L15-L119)).
  `ProcessControl::cancel` and `force_kill` are acknowledged operations; dropping a session sends
  `OwnerDropped` ([`session.rs:123-157`](../../../src/framework/process/session.rs#L123-L157)).
* Linux detached launches are explicitly durable: `launch_detached` prepares a private run
  directory, publishes launch intent under a lock, starts through the systemd authority, and
  creates a `DetachedProcessReceipt` ([`detached.rs:68-110`, `113-187`](../../../src/framework/process/detached.rs#L68-L110)).
  The receipt contains a run ID plus opaque authority/invocation identity, not a guessed PID
  ([`receipt.rs:14-41`](../../../src/framework/process/receipt.rs#L14-L41)).
* `stop_detached` resolves the exact authority from that receipt, observes terminal state or asks
  the backend to stop, publishes a terminal report, releases the authority, and records release
  completion ([`detached.rs:254-323`](../../../src/framework/process/detached.rs#L254-L323)). This is
  the correct stop/reap owner for a long-lived `tsgo` child (the service owns the child receipt;
  Kit owns the daemon receipt).
* Failed launches are recoverable without PID guessing. `recover_detached_launch` validates the
  run directory and launch lock before recovering the persisted authority; listing/recovering all
  pending intents skips locked runs ([`detached.rs:433-480`](../../../src/framework/process/detached.rs#L433-L480)).
  `tools/process.rs` exposes this existing recovery capability as `kit process pending` and
  `recover-detached` ([`src/tools/process.rs:15-35`, `64-121`](../../../src/tools/process.rs#L15-L35)).
* A detached run's public status/report includes `ProcessRunId`, completion cause, leader exit,
  containment, descendant disposition, termination, and elapsed time
  ([`report.rs:101-131`](../../../src/framework/process/report.rs#L101-L131)). This can back service
  status and crash evidence. It does not itself provide a daemon instance UUID or request counter;
  those belong in the tsgo service's own durable record/protocol.

### Durable state and permissions

`AtomicFileWriter` is the shared state primitive: lock files are opened with `O_NOFOLLOW`, mode
`0600`, regular-file and single-hard-link checks; replacements write a synced sibling and rename
atomically ([`src/framework/atomic_file.rs:12-18`, `63-121`, `129-165`](../../../src/framework/atomic_file.rs#L12-L165)).
Use this for the service registry and identity record rather than ad-hoc JSON writes.

The process supervisor creates private state/run directories and rejects non-private ownership or
mode; `RunDirectoryLease` deletes an unretained run directory on drop
([`supervisor.rs:240-249`, `291-309`, `388-429`](../../../src/framework/process/supervisor.rs#L240-L429)).
The service socket should use the same per-user runtime root and private-directory checks. Existing
Console code is a concrete Unix-socket policy: it rejects symlinks/non-sockets, requires effective
UID ownership, forbids group/other write bits, and removes only a validated stale socket
([`src/tools/console/client.rs:172-205`, `208-288`](../../../src/tools/console/client.rs#L172-L288)). Its
runtime directory is created `0700` and validates ownership/mode; it also checks Unix socket path
length ([`src/tools/console/runtime.rs:70-121`](../../../src/tools/console/runtime.rs#L70-L121)).
These checks are reusable policy, but currently live under Console; a tsgo peer module should copy
the mechanism into a genuinely shared runtime/socket owner only if a second consumer appears.

### Repository/workspace resolution

`RepositoryLocator::nearest_worktree_root` canonicalizes the start path, walks ancestors for `.git`,
and returns a typed `WorktreeRoot`; errors distinguish non-worktrees, canonicalization, and marker
inspection ([`src/framework/repository.rs:5-28`, `30-57`](../../../src/framework/repository.rs#L5-L57)).
The tsgo tool should resolve and canonicalize the workspace before deriving service identity. The
locator handles Git worktree boundaries, not arbitrary TypeScript project roots; tsgo policy must
then find the relevant `tsconfig`/workspace while retaining the canonical worktree path in its key.

### Unix-socket daemon/client pattern and concurrency

CDP has the closest daemon precedent. Its registry stores one JSON record plus one Unix socket per
attachment, with an opaque detached-daemon receipt and explicit `started_at_ms`; comments state
reconciliation must use supervisor receipts, never PIDs ([`src/tools/cdp/registry.rs:1-7`, `13-32`](../../../src/tools/cdp/registry.rs#L1-L32)).
Records are keyed by instance name, stored under a per-user runtime directory, enumerated, and
removed together with their socket ([`registry.rs:141-152`, `174-249`](../../../src/tools/cdp/registry.rs#L141-L249)).
CDP's daemon binds a Unix socket, accepts clients, and spawns one task per connection
([`src/tools/cdp/daemon/mod.rs:343-370`](../../../src/tools/cdp/daemon/mod.rs#L343-L370)); its handler parses a
request, updates activity, dispatches, and writes one newline-delimited JSON reply
([`daemon/mod.rs:812-843`](../../../src/tools/cdp/daemon/mod.rs#L812-L843)). A subscription path
demonstrates a long-lived streaming connection and bounded per-client channel
([`daemon/mod.rs:828-875`](../../../src/tools/cdp/daemon/mod.rs#L828-L875)).

This pattern supports concurrent Kit clients, but CDP's one-line request/reply is not sufficient for
LSP: tsgo needs `Content-Length` framing, a monotonically unique request ID map, and a reader loop
that routes server requests/notifications while serializing writes. The service daemon should own
one LSP child connection and multiplex all client requests through that broker.

### Framework output and registration

`Output` owns the global text/JSON decision and serializes any `Serialize` value as pretty JSON;
text presentation remains tool-owned ([`src/framework/output.rs:4-37`](../../../src/framework/output.rs#L4-L37)).
`Context` injects `Output`, `RepositoryLocator`, and `ProcessSupervisor` into every tool
([`src/framework/context.rs:1-10`](../../../src/framework/context.rs#L1-L10)). `Tool` is the
single command contract (`meta`, clap `command`, async `run`) ([`src/framework/tool.rs:7-34`](../../../src/framework/tool.rs#L7-L34));
`Registry` mounts tools and constructs the shared context once per invocation
([`src/framework/registry.rs:9-27`, `39-71`](../../../src/framework/registry.rs#L9-L71)). `main.rs`
keeps wiring-only registration, so a future `tsgo` module is a peer tool registered there, never
imported from another tool ([`src/main.rs:46-67`](../../../src/main.rs#L46-L67)).

## Architecture comparison

| Option | Fit with Kit | Failure/isolation cost | Verdict |
| --- | --- | --- | --- |
| One global `tsgo` for all workspaces | Small process count, but one LSP root/config and document graph would mix unrelated workspaces; a crash or incompatible server version invalidates every client. Identity cannot be keyed cleanly by workspace. | High cross-workspace leakage and version conflict; cleanup/idle policy becomes global. | Reject. |
| One Kit daemon + one `tsgo` child per workspace | Natural registry key `(canonical workspace, resolved tsgo version)` and exact one-child ownership. Simple routing and lifecycle; concurrent clients share one daemon. | Multiple workspaces mean multiple children, but each failure is isolated and stale records are independently reconcilable. | Viable, but daemon-per-workspace records must be managed atomically. |
| One broker managing workspace-scoped children | One long-lived Kit broker can multiplex many workspaces and expose one management endpoint; child identity stays workspace-scoped. | Broker becomes a large second supervisor: child routing, per-workspace locks, idle timers, crash recovery, and protocol isolation all become shared state. | Overbuilt for first consumer; defer until a second real service needs a broker. |

## Recommendation (inference from the seams)

Choose **one detached Kit daemon and exactly one attached/native `tsgo --lsp --stdio` child per
`(canonical workspace, resolved executable version)`**. The daemon is the canonical owner of the
child, LSP document state, request correlation, client fan-out, request count, and idle timer. Kit's
`ProcessSupervisor` owns daemon launch/stop/reap through the opaque detached receipt; the daemon's
child should be spawned under a supervised process session (or a child receipt if detached within the
daemon) and reported by a non-PID child identity (run ID plus executable/version/start timestamp).

Use a registry record shaped like CDP's record but keyed by a hash of canonical workspace plus
server-version identity. Store canonical workspace, exact executable path, resolved version string,
daemon instance UUID, daemon start time, request count, daemon detached receipt, child run/instance
identity, socket path, and protocol/schema version. Publish with `AtomicFileWriter`; acquire a per-key
lock before deciding whether a record/socket is live. To recover stale state, validate ownership,
socket type/mode, registry schema, and a daemon handshake containing instance UUID/start time before
deleting a record; never infer liveness from a PID. A failed handshake or dead socket is stale and can
be removed safely only when path ownership checks pass.

Expose the smallest public surface that proves management and use: a query command (first query
starts/reuses), `inspect` (identity, child identity, request count, state), and `stop`; add `gc` only
if stale-record cleanup cannot be safely folded into those commands. Keep call-hierarchy operations
as query subcommands rather than a separate daemon verb. Every command emits structured JSON through
`Context::out` when `--json` is selected.

The daemon protocol should be newline-delimited JSON for Kit client envelopes, carrying an operation
ID and service instance ID; inside the daemon, use strict LSP `Content-Length` framing. A single child
reader routes responses by LSP ID and forwards server requests/notifications to a daemon policy
handler; client operations are queued so two clients cannot corrupt writes. Initialization is one
state transition (`initialize` -> response -> `initialized`), shutdown is `shutdown` response then
`exit`, and unexpected EOF is a crash transition followed by receipt-backed cleanup/restart on the
next query. The query path must synchronize `didOpen`/`didChange` for edited files and preserve the
same child until idle timeout or explicit stop.

## Decision capsule

* **Canonical owner:** new `src/tools/tsgo/` command policy plus a small domain-neutral daemon/LSP
  peer module only where mechanics are reusable; no tool-to-tool imports.
* **Lifecycle authority:** `ProcessSupervisor` detached receipts and `stop_detached`; no PID-based
  control. Service registry is advisory discovery, reconciled by receipt and authenticated socket
  handshake.
* **Smallest architecture:** per-workspace/version daemon with one warm child; do not build a
  global process or multi-workspace broker yet.
* **Proof required before completion:** first query starts; second query returns the same daemon and
  child identities (plus start time/request count); file edit changes a later result without child
  restart; two concurrent clients correlate independently; stop reaps; a later query creates a clean
  replacement; stale registry/socket recovery succeeds without PID guessing.
