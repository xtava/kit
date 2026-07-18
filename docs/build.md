# Build provider client

`kit build` runs a repository-owned build provider under Kit's process supervisor. Kit owns the
process tree, cancellation, transcripts, and protocol validation. The repository continues to own
its build graph, cache, concurrency, artifacts, verification, and release policy.

## Interactive console

Run bare `kit build` from anywhere inside a Git worktree to open the interactive Build console.
It discovers the same nearest manifest as `kit build run`, lists every workflow with its current
host eligibility, and starts the selected workflow in the same in-process execution engine. It
does **not** shell out to `kit build run`, run a second provider parser, or weaken process
supervision for terminal presentation. Bare `kit build` requires a terminal on both stdin and
stdout; use `kit build run WORKFLOW` for scripts, pipes, and JSON output.

The running view shows the generated run identity, elapsed time, the validated nested stage
hierarchy, recent protocol events, and independent bounded live tails for supervisor-recorded
stdout and stderr. Every pane follows its newest validated row until you scroll it, then clamps its
own viewport independently; stages retain the latest 512 rows and protocol presentation retains the
latest 2,048 event lines without weakening the protocol's larger validation bounds. Those tails come
from the supervisor's `RecordedOutputTail` handles; they are diagnostics only. The versioned event
stream and final result remain the only sources of build meaning.

Keys are intentionally small and uniform:

- Workflow chooser: `j`/`k` or arrows select, `Enter` starts an eligible workflow, `e` opens
  evidence, and `q`/`Esc` exits.
- Running build: `c` or `Ctrl-C` requests cancellation through the actual `ProcessControl`;
  `Tab`/`Shift-Tab` or `h`/`l` chooses Stages, Events, stdout, or stderr; `j`/`k` and Page Up/Down
  scroll only that pane; `q` requests cancellation and waits for the owned process tree to reach a
  terminal state before leaving the console.
- Terminal build: the same pane keys include the terminal-diagnostics pane; `r`/`Esc` returns to
  workflow selection, `e` opens evidence, and `q` exits with the build's truthful success/failure
  status after the alternate screen has been restored.
- Evidence: `j`/`k` selects a record, `Enter`/`i` validates and views it, `f` asks for an exact
  record deletion confirmation, `y` confirms, `b`/`Esc` goes back, and `q`/`Ctrl-C` exits the
  console. Evidence navigation never sends another process-control request after the Build is
  terminal.

The UI consumes a closed typed update/control boundary: `Started`, validated protocol events,
bounded transcript-tail revisions, and one terminal truth (`Succeeded`, `Failed`, or
`InfrastructureFailure`) flow from the engine; its only interactive command is `Cancel`.
Automation and the TUI are therefore presenters over one lifecycle. Failed-run evidence is
captured before any terminal failure update or final event presentation, preserving the primary
failure and retained evidence even when human output cannot be written.

Workflow platform membership is necessary but not sufficient for a `ready` row. Build first checks
the supervisor's private prepared-storage capability, then performs a bounded real `CompleteTree`
setup probe (including Linux guardian readiness and empty teardown). The noninteractive runner uses
that same probe. A host without delegated cgroup access is therefore shown unavailable before a
provider or protocol workspace is created.

## Repository configuration

Create `.kit/build.toml` inside a Git worktree:

```toml
version = 1

[provider]
program = "pnpm"
args = ["exec", "tsx", "tools/build-provider.ts"]

[[workflows]]
id = "check"
label = "Check the workspace"
platforms = ["linux", "macos", "windows"]

[[workflows]]
id = "release-linux"
label = "Build and verify the Linux release"
platforms = ["linux"]
```

Run a workflow from anywhere below the worktree root:

```bash
kit build run check
kit build run check --project path/to/nested/package
kit build run release-linux --deadline 3600
```

Every run has a finite deadline. The default is 7,200 seconds (two hours); `--deadline SECONDS`
overrides it, subject to the supervisor clock's representable maximum.

Kit selects the nearest `.kit/build.toml` without walking beyond the canonical Git worktree. A
provider may be a bare executable resolved through `PATH`, or a repository-relative executable.
Repository-relative executables are canonicalized and rejected if they escape the worktree. Kit
never executes manifest commands through an implicit shell. Discovery continues only when a
candidate is genuinely absent: a malformed, unreadable, non-regular, or broken nearer candidate
fails closed instead of falling through to a parent provider. Kit opens candidates nonblocking,
proves they are regular files, and rejects a manifest larger than 262,144 bytes before UTF-8 or
TOML parsing.

## Provider protocol

The provider receives one environment variable:

```text
KIT_BUILD_REQUEST=/absolute/private/kit/run/path/request.json
```

The versioned request contains the run and workflow identity, canonical repository root, append-only
event path, and atomic final-result path. Kit places those protocol artifacts in a private
capability directory separate from the supervisor's stdout and stderr transcript files, and it
discloses only the protocol paths. This prevents ordinary provider path mistakes from overwriting
transcripts. It is not a sandbox or a security boundary: a same-UID, unsandboxed provider is trusted
and can inspect or modify other files available to that user, including deliberately locating and
writing Kit's transcripts. Providers use stdout and stderr normally; Kit drains and records those
streams without blocking the process tree. Each stream retains at most 67,108,864 durable bytes while also
tracking its final 16,384 bytes independently, so a noisy provider cannot make transcript storage
unbounded and the true final diagnostics remain available after durable truncation. The process
supervisor always removes its private run directory after the report and its leases are released;
Build never retains or inventories supervisor storage.

After the supervisor returns a complete `ProcessReport`, a failed Build run may publish a separate
Build-owned evidence record under Kit's private state directory. The record contains bounded copies
of the stdout/stderr transcripts, their independent final tails, the request/event/result protocol
artifacts that were available within their protocol evidence limits, and a typed terminal-process
marker. Setup/spawn errors and supervisor infrastructure-failure reports are not complete terminal
reports, so they never create retained evidence; their available final tails and byte accounting are
printed inline without temporary transcript paths.

The evidence store admits at most 8 records and 536,870,912 logical payload bytes in aggregate.
Admission never evicts an old record or deletes data by age. If the next record would exceed either
limit, the build failure remains primary, existing evidence is untouched, the final tails are printed
inline, and Kit explicitly says that the new evidence was not retained. Published records are
atomically moved into the Build-owned store only after their files and marker have been synchronized.
An interrupted publication can appear as `incomplete`; it is counted against both limits and is
never treated as a completed record.

Manage evidence deliberately:

```bash
kit build evidence list
kit build evidence inspect RUN_ID
kit build evidence forget RUN_ID
```

Failure output names the retained run and prints its `inspect` and `forget` commands. `list` reports
stored, corrupt, and incomplete entries plus aggregate usage. `inspect` revalidates the typed marker
and every referenced regular-file payload and exact byte length before printing durable paths.
`forget` accepts only a canonical run UUID and removes only that exact Build-owned record. Existing
evidence roots must already be owner-only real directories; Kit rejects symlinks, wrong owners, and
group/world-accessible modes rather than repairing them implicitly. Build evidence storage currently
requires Unix owner/mode and directory-sync semantics; it fails explicitly on unsupported platforms.

Generate the canonical JSON Schema with:

```bash
kit build schema > .kit/build-provider.schema.json
```

Provider implementations should generate their types from this schema or validate the schema
directly. Do not maintain a second handwritten protocol model. The root
`KitBuildProviderProtocol` schema includes the `manifest`, `request`, `event`, and `final_result`
models, all derived from the Rust types Kit actually reads.

The manifest schema requires version 1, a provider program containing at least one non-whitespace
character, and at least one workflow. Workflow IDs contain 1 to 64 letters, numbers, `-`, or `_`; labels
must contain a non-space printable ASCII character and contain at most 128 printable ASCII
characters; and every workflow has at least one supported platform. This deliberately portable
label alphabet keeps Rust, JSON Schema, JavaScript/Zod, TOML tooling, and terminal rendering
acceptance-equivalent. The same `ProcessLabel` domain boundary enforces the general display-label
constraints at runtime. Platform entries are
membership declarations, so repeated entries are accepted as semantically inert rather than
requiring a second cross-language `uniqueItems` validator. Kit additionally performs named runtime
checks for constraints JSON Schema
cannot express, including unique workflow IDs, repository-relative provider resolution, and
canonical worktree containment.

The protocol schema constrains versions, canonical run UUIDs, workflow IDs, nonempty payload
strings, and all integer bounds. `sequence`, `event_count`, and `last_event_sequence` are unsigned
32-bit values from 1 through 100,000. `timestamp_ms` is a nonnegative integer no greater than
9,007,199,254,740,991, so generated JavaScript and TypeScript consumers can represent it exactly.
Kit's named Rust validation remains authoritative for filesystem semantics, cross-field identity and
path relationships, event lifecycle, and final process/result agreement.

Events are newline-delimited JSON. Each record contains the common protocol, run, workflow,
sequence, and timestamp fields plus a required `event` object. Sequence numbers begin at one, are
contiguous, and bind every record to the request's protocol version, run ID, and workflow ID. For
example, the first record is shaped as:

```json
{"protocol_version":1,"run_id":"<request run UUID>","workflow_id":"check","sequence":1,"timestamp_ms":0,"event":{"kind":"run_started"}}
```

The event stream is limited to 16,777,216 bytes, 100,000 records, and 65,536 bytes per JSON record
excluding its newline delimiter. While the contained provider tree is running, Kit incrementally
reads newly appended bytes, validates every complete record through the same lifecycle state
machine used at final EOF, and prints newly validated events in human-readable mode. An invalid
complete record or an oversized partial record causes Kit to cancel and await the owned process tree
before returning the protocol failure. After the tree stops, Kit consumes the final bytes and rejects
an unterminated partial record.

The reader also enforces append-only integrity for the bounded stream. On Unix, the first open
requires one hard link and pins the file's device, inode, and link count. Every subsequent read and
finalization reopens the capability path and proves that both descriptors still identify that same
object, detecting observed unlink, relink, and pathname-replacement corruption. Independently of
platform identity metadata, Kit retains the exact consumed bytes in a bounded 16,777,216-byte
verification snapshot. After the provider tree stops, it rereads the final path once within that
same bound and compares every byte, detecting prefix mutation and truncate-then-rewrite even when
the final length recovered past the consumed offset.

Stable Rust does not expose Windows volume/file-index/link identity. On Windows, Kit rejects
reparse/non-regular files, pins creation time, revalidates the pathname and length at every read, and
still performs the exact final byte comparison. A Windows replacement that preserves creation time
and produces byte-identical final contents cannot be distinguished from the original object. On any
platform, a replacement or mutation fully restored between two observations and before the final
comparison is not observable. These checks detect corruption of the trusted same-UID provider
protocol; they are not a sandbox or security boundary.

The first event must be `run_started`. The nested event union is:

- `run_started`
- `stage_started`
- `stage_progress`
- `stage_finished`
- `artifact_reported`
- `evidence_reported`

There is no `run_finished` event. The provider atomically replaces the result path with the sole
terminal result after its event stream is complete. Kit accepts success only when request identity,
event identity and sequence, event counts, final result, and provider exit status all agree.
Kit does not parse compiler text from stdout or stderr. Those streams are diagnostic transcripts;
only the versioned event and result artifacts carry protocol meaning. JSON mode emits no live mixed
output; it returns the deterministic final envelope after validation.

Kit validates the event stream as one lifecycle, not as independent JSON records:

- Timestamps never decrease.
- Every stage ID is unique for the run.
- A child can start only after its parent has started and while that parent remains active.
- Progress and finish events refer to an active stage.
- A stage cannot finish while it has active children, and every started stage is finished before the
  stream ends.
- A successful final result is invalid if any stage failed. A failed final result remains valid with
  or without a failed stage because provider, configuration, or finalization failures can occur at
  run level and the protocol has no separate run-failure event. A provider may emit no stages and
  report either terminal outcome; the required `run_started` event and final counts still apply.

In JSON mode, the run result contains the complete validated structured event list as `events` in
addition to `event_count` and the terminal outcome. Human-readable mode prints concise stage,
progress, artifact, and evidence lines from those same validated events as they become available,
with any final records printed before the terminal summary. Neither mode derives protocol meaning
from compiler or tool text in stdout or stderr.

## Ownership boundary

Kit does not infer cache hits, artifacts, readiness, or stage results from filenames or human build
output. A provider may report repository-relative artifact and evidence claims, but the repository's
build system remains responsible for proving them. Cancellation, deadlines, and process failures are
Kit process outcomes; a provider result cannot override them.

On platforms where complete descendant containment is unavailable, a workflow requesting that
guarantee fails explicitly. Kit never silently relabels a process-group fallback as complete-tree
ownership.
