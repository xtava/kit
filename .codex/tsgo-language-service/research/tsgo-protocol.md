# Native `tsgo` LSP protocol research

Research lane: native TypeScript 7 language-server lifecycle and protocol. The executable was
observed read-only; no Modular files were changed.

## Evidence and version resolution

Observed on 2026-08-10:

```text
/home/tvx/Desktop/projects/modular/node_modules/.bin/tsgo
 -> ../@typescript/native-preview/bin/tsgo.js
Version 7.0.0-dev.20260418.1
```

The package file `/home/tvx/Desktop/projects/modular/node_modules/@typescript/native-preview/package.json`
declares version `7.0.0-dev.20260418.1`, bin `./bin/tsgo.js`, and optional platform packages
`@typescript/native-preview-${platform}-${arch}` at the same version. Its `bin/tsgo.js` resolves
the platform executable through `lib/getExePath.js` and replaces/executes it with the supplied
arguments. `getExePath.js` computes the package name from Node `process.platform` and
`process.arch`, resolves that package's `package.json`, then appends `lib/tsgo` (or `tsgo.exe` on
Windows), and errors if the executable is absent. This means Kit must resolve an executable path
from the selected workspace's installation, record the resolved canonical path and `--version`
output, and reject a service identity when either path or version differs. Do not trust a bare
`tsgo` in `PATH` as the identity.

Commands used:

```sh
readlink -f /home/tvx/Desktop/projects/modular/node_modules/.bin/tsgo
/home/tvx/Desktop/projects/modular/node_modules/.bin/tsgo --version
/home/tvx/Desktop/projects/modular/node_modules/.bin/tsgo --help
```

The help output identifies this as `tsc` version `7.0.0-dev.20260418.1`; the native preview
README says it is experimental and intended for testing. The TypeScript Go repository is the
package's declared repository: <https://github.com/microsoft/typescript-go>.

## Process and wire lifecycle (verified capture)

Starting the exact command below in `/home/tvx/Desktop/projects/modular` produced a live LSP
server:

```sh
/home/tvx/Desktop/projects/modular/node_modules/.bin/tsgo --lsp --stdio
```

Messages are JSON-RPC 2.0 objects framed by `Content-Length: <UTF-8 byte count>\r\n\r\n` and a
JSON body. The probe used byte length, not character count. JSON-RPC request IDs may be numbers or
strings: the server's dynamic registration request used string ID `"ts1"`; client requests used
numeric IDs. The broker must maintain independent client-facing IDs and a pending map keyed by
each upstream request ID; responses can be interleaved with notifications and server requests.

The successful sequence was:

1. Client sends `initialize` with `rootUri` and `workspaceFolders`.
2. Server emits `window/logMessage` and responds to ID 1 with capabilities and `serverInfo`.
3. Client sends `initialized` notification.
4. Server may send a request such as `client/registerCapability`; the client must answer it (the
   probe answered ID `"ts1"` with `result: null`). A broker cannot only read responses to its own
   calls; it must dispatch and answer every server request.
5. Client sends document notifications and feature requests.
6. Client sends `shutdown` **without a params member**. The observed response is ID-matched
   `result: null`; sending `{}` as params was rejected with `-32602 InvalidParams`.
7. Client sends `exit` notification. The clean process exited with status 1 in the probe after
   stderr reported `context canceled`; therefore graceful teardown should treat the exit and
   reaped child as the invariant, while recording the observed exit code/stderr rather than
   assuming zero.

The LSP 3.17 specification defines client-managed lifecycle and stdio as a supported transport:
<https://github.com/Microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/specification.md#language-server-protocol>
and defines initialize/initialized/shutdown/exit in the same specification. A primary TypeScript
Go discussion confirms that `--lsp --stdio` was the only public API during the native-preview
period: <https://github.com/microsoft/typescript-go/discussions/455>.

The observed initialize result included:

```json
{
  "positionEncoding": "utf-16",
  "textDocumentSync": {"openClose": true, "change": 2, "save": true},
  "callHierarchyProvider": true,
  "workspaceSymbolProvider": true,
  "diagnosticProvider": {"interFileDependencies": true, "workspaceDiagnostics": false},
  "serverInfo": {"name": "typescript-go", "version": "7.0.0-dev.20260418.1"}
}
```

`change: 2` is LSP `Incremental`, not full synchronization. The server nevertheless accepted a
single content-change item containing the complete new text in the probe; a production client
should honor the advertised incremental contract and send range/rangeLength/text edits (or use
one full replacement only when the server's behavior has been separately proven). The LSP source
defines `None=0`, `Full=1`, and `Incremental=2`, plus open/close semantics:
<https://github.com/Microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/specification.md#text-document-synchronization>.

The server emitted `textDocument/publishDiagnostics` notifications after opening the project,
including an empty diagnostics array for `scripts/tsconfig.json`, and emitted numerous
`window/logMessage` notifications while loading the configured project. These are normal
server-to-client notifications and must not be treated as query results.

## Documents and changes while warm (verified capture)

The probe opened the existing file
`file:///home/tvx/Desktop/projects/modular/scripts/upstream-catalog.ts` with `version: 1`, then
sent `textDocument/didChange` with `version: 2`, followed by another call-hierarchy request.
The server logged `handled method 'textDocument/didChange'`, marked the configured project dirty,
cloned a new snapshot, and returned the same symbol from the subsequent request. This proves that
the same warm process accepts edits and refreshes project state without a restart. The client must
track a monotonically increasing version per URI and serialize changes per document; concurrent
queries may proceed, but a query that depends on an edit must wait until that notification has been
written and processed by the broker's ordering discipline.

The LSP specification requires `didOpen`, `didChange`, and `didClose` to be implemented as a set
when synchronization is enabled, and describes the `TextDocumentContentChangeEvent` range form:
<https://github.com/Microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/specification.md#textdocument-didchange>.
Disk changes are not automatically equivalent to open-document changes. For files not open in the
client, use the server's workspace file-watcher registration (`workspace/didChangeWatchedFiles`) or
send an explicit open/change/close cycle. The observed server dynamically requested
`workspace/didChangeConfiguration` registration; the broker must answer registrations and may
choose to implement a conservative watched-file policy for `.ts`, `.tsx`, `.js`, `.jsx`, `.json`,
and config files. Never scan or mutate a workspace from the LSP child without an explicit owner
policy.

## Call hierarchy shapes (verified against native server and LSP)

The server advertised `callHierarchyProvider: true`. For `posix` at line 222, character 9 in the
file above, `textDocument/prepareCallHierarchy` returned an array of `CallHierarchyItem` values:

```json
{
  "name": "posix", "kind": 12,
  "uri": "file:///.../upstream-catalog.ts",
  "range": {"start":{"line":222,"character":0},"end":{"line":224,"character":1}},
  "selectionRange": {"start":{"line":222,"character":9},"end":{"line":222,"character":14}}
}
```

`callHierarchy/incomingCalls` returned objects with `from` (a call hierarchy item) and
`fromRanges` (one or more ranges). `callHierarchy/outgoingCalls` returned `to` plus `fromRanges`.
The observed outgoing result included two standard-library `split` items and one `join` item;
URIs percent-encoded `@` as `%40`. The LSP primary definitions and parameter shapes are here:
<https://github.com/Microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/specification.md#call-hierarchy-requests>.

The broker should expose a typed command that accepts workspace-relative or canonical file URI,
line, and UTF-16 character, then forwards the three standard methods. Preserve arrays, ranges,
URI encoding, and `null` results exactly; do not flatten hierarchy data into display strings.

## Correlation, concurrency, errors, and cancellation

JSON-RPC permits out-of-order responses. A single upstream `tsgo` child can serve multiple
requests, but only one process-wide LSP stream exists; writes need a mutex/queue and reads need one
framing decoder. Each Kit client connection needs its own pending map and cancellation behavior.
Forward `$/cancelRequest` with the original upstream ID when a client disconnects or times out, and
return a bounded timeout error if the server does not respond. Handle JSON-RPC errors (`code`,
`message`, optional `data`) as structured output rather than string matching. Server requests
(`client/registerCapability`, `workspace/configuration`, `client/applyEdit`, and similar) require
policy-gated replies; unsupported requests should receive a JSON-RPC `-32601` response, not block
the reader. Notifications have no response and must still be delivered in order.

LSP JSON-RPC and cancellation definitions are in the Microsoft specification's base protocol:
<https://github.com/Microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/specification.md#base-protocol>.

## Architecture implications

| Architecture | Protocol consequence | Verdict |
|---|---|---|
| One global `tsgo` for all workspaces | One initialize root and one project graph cannot safely represent unrelated roots; document URIs and config/project ownership collide; one crash loses every client. | Reject. |
| One Kit daemon + child per workspace | Straight mapping of one LSP stream to one canonical root; simple pending map, document versions, idle timer, and teardown. | Smallest viable design. |
| Broker managing workspace-scoped children | Adds a second routing layer and useful only when one long-lived broker must multiplex many roots. It must still enforce the same `(workspace, resolved executable, version)` key and child ownership. | Defer until a real multi-workspace consumer exists. |

Recommendation: a Kit-managed daemon instance owns exactly one `tsgo --lsp --stdio` child and one
canonical workspace root. A local Unix socket exposes a small Kit protocol; the daemon serializes
upstream writes, correlates responses, services server requests, tracks document versions and
request count, and returns an instance ID/start time/child identity with every query. A registry
record keyed by canonical workspace plus resolved executable path and `--version` is authoritative;
socket connect plus an authenticated instance handshake proves liveness. Stale registry/socket
files are removed only after connect/handshake failure and ownership receipt validation, never by
guessing a PID.

## Practical probe and verification plan

1. Resolve the workspace and executable, run `--version`, and start once on first query.
2. Send initialize/initialized, answer server requests, and capture the daemon's random instance
   ID, child start time, and child identity (receipt/process metadata).
3. Run two concurrent Kit queries; assert both IDs correlate to their own results and daemon
   request count increases without a second child or instance.
4. Open a file, send an incremental change, query hierarchy, and assert changed ranges/results while
   child identity and start time remain unchanged.
5. Stop through Kit: send shutdown with no params, exit, wait/reap the owned child, remove socket
   and registry atomically, and report exit/stderr.
6. Query again: assert a new instance ID/start time/child identity and request count reset to one.
7. Leave stale registry/socket state, then query: handshake failure plus receipt ownership check
   must clean it and start a replacement; no PID probing or killing an unrelated process.

Non-goals for this protocol lane: a global cross-workspace index, a second custom semantic engine,
an LSP compatibility shim that keeps old paths alive, or exposing arbitrary LSP methods before a
typed Kit command has an ownership and verification story. The native preview package itself warns
that features are incomplete; unsupported capability behavior must remain visible in structured
errors and logs.

## Decision capsule
