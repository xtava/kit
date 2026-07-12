# CDP Launcher Design

## Goal

The launcher solves one practical problem: create a browser session that an
agent can debug without guessing what was launched, what was captured, what
changed, or where the raw evidence lives.

The target workflow is:

```bash
kit cdp launch http://localhost:3000 --name checkout
kit cdp state --app checkout
kit cdp mark before-order --app checkout
kit cdp click "button:Place order" --app checkout
kit cdp after before-order --app checkout
kit cdp bundle checkout --since before-order
```

That gives the agent four things:

- a known isolated browser session
- capture from before app code runs
- a compact current state
- evidence for what changed after an action

## Controlled Launch

`launch` creates an isolated browser, attaches CDP immediately, enables
capture, then navigate. Startup capture is the default because missing boot logs
is the failure mode this feature exists to prevent.

```bash
kit cdp launch http://localhost:3000 --name checkout
kit cdp launch http://localhost:3000 --name checkout --headless
kit cdp launch http://localhost:3000 --name checkout --fresh
kit cdp launch http://localhost:3000 --name checkout --profile authed-user
kit cdp launch http://localhost:3000 --name checkout --viewport 1440x1000
kit cdp launch http://localhost:3000 --name checkout --no-startup-capture
```

Launch output prints the facts an agent needs:

```text
checkout
url        http://localhost:3000
browser    Chrome for Testing
profile    temp
target     page
capture    startup, console, exceptions, network, ws, lifecycle
debug      127.0.0.1:49321
raw        kit cdp tail --app checkout
```

No command should silently use the user's normal Chrome profile.

## Session Identity & Lifecycle

A launched browser is a named session. `--app` selects that session for existing
commands. Profiles and bundles are related data, but they are not the session
identity.

```bash
kit cdp launched
kit cdp close checkout
kit cdp close --all
kit cdp launch http://localhost:3000 --name checkout --reuse
kit cdp launch http://localhost:3000 --name checkout --replace
```

Rules:

- `launch --name checkout` fails if `checkout` already exists.
- `--reuse` keeps the existing browser and navigates/focuses it.
- `--replace` closes the existing launched browser and starts a new one.
- launch records store the browser websocket identity so stale records do not
  close or reuse unrelated processes on the same port, plus pid start times and the
  controlled process group/session so an unreachable endpoint can still be cleaned
  up without signalling a reused pid.
- the ownership record is durable before Attachment spawn. All subsequent failures
  use one cleanup boundary that verifies both the controlled session and exact daemon
  identity; incomplete cleanup retains recovery state instead of returning through
  an untracked window.
- `detach` stops capture but does not close a launched browser.
- `close` closes the launched browser and stops its attachment only after verifying
  endpoint/process ownership. It verifies session shutdown after graceful close,
  SIGTERM, and SIGKILL.
- temporary profiles are deleted after verified close unless `--keep-profile` was
  used. Ambiguous ownership or surviving processes produce a non-zero failure and
  retain the record/profile for recovery.
- artifacts remain after `close` until explicitly removed.

## Startup Capture Contract

Default launch order:

1. start the browser on a controlled blank page
2. connect to the browser-level CDP endpoint
3. enable target auto-attach
4. enable console, exception, network, websocket, and lifecycle capture
5. create or select the app page
6. navigate to the requested URL

Then this is inspectable:

```bash
kit cdp launch-log --app checkout
```

```text
0ms    mark launch
20ms   navigate http://localhost:3000
95ms   GET /main.js 200
110ms  console.warn missing feature flag checkout_v2
140ms  exception TypeError: config.checkout is undefined
raw    kit cdp tail --since-mark launch --app checkout
```

`--no-startup-capture` exists only for cases where the user wants a simpler
ambient browser launch and accepts that early events may be missed.

## Command Surface Contract

The launcher adds only commands that answer a distinct debugging question.

| Command | Question | Notes |
|---|---|---|
| `state` | What is true right now? | Compact summary, always has JSON output. |
| `launch-log` | What happened during startup? | Read-only view over launch marks and Timeline. |
| `mark` | Where should a later query start? | Records a named point on the Timeline. |
| `after` | What changed after a mark/action? | Bounded action window with raw escape hatch. |
| `bundle` | What evidence should be handed off? | Exports redacted evidence by default. |
| `close` | Stop the launched browser. | Different from `detach`, which only stops capture. |

Existing commands should remain the raw escape hatches: `tail`, `errors`, `snap`,
`click`, `fill`, and `eval`.

## State

Agents need a current-state command that is broader than `ready` but not noisy.

```bash
kit cdp state --app checkout
kit cdp state --app checkout --visual
kit cdp state --app checkout --json
```

It includes:

- selected target
- URL/title/readiness
- recent errors
- recent failed network requests
- current focused element
- screenshot path when `--visual` is used
- active environment overrides
- active network block/mock rules
- raw commands attached to each summarized row

Example:

```text
target    page "Checkout" /checkout
ready     complete, visible
errors    1 recent exception  raw: kit cdp errors --explain --app checkout
network   POST /api/checkout 500  raw: kit cdp net show req_42 --app checkout
focus     button "Place order"
screen    artifacts/checkout/latest.png
env       timezone America/New_York
rules     mock GET /api/me
```

## Action Windows

`after` answers "what changed after this mark?" without dumping a giant
Timeline.

```bash
kit cdp mark before-save --app checkout
kit cdp click "button:Save" --app checkout
kit cdp after before-save --app checkout
kit cdp after before-save --idle 500ms --timeout 5s --app checkout
```

`after` reports events from the mark until the Timeline is quiet for `--idle` or
the `--timeout` is reached. The output states which condition ended the window,
counts total/error/network events, shows a short recent tail, and always prints a
raw `tail --since-mark` command. Unknown marks fail explicitly instead of falling
back to a time window.

```text
after before-save
ended     network idle after 620ms
network   POST /api/settings 500  req:req_18 raw: kit cdp net show req_18
console   error Save failed: missing timezone
raw       kit cdp tail --since-mark before-save --app checkout
```

## Raw Evidence Follow-Through

Summaries should never dead-end. Rows that summarize raw data should carry enough
identity to inspect the source event.

Network rows should expose:

- request id
- URL, method, status, timing
- raw command, for example `kit cdp net show req_18`

Error rows should expose:

- source URL and line/column when available
- raw command, for example `kit cdp errors --explain`

## Evidence Bundle

When the agent is stuck, it should be able to export the session.

```bash
kit cdp bundle checkout --since before-save
kit cdp bundle checkout --since before-save --include trace,har,screenshots
kit cdp bundle checkout --since before-save --include-secrets
```

Default bundle contents are boring and inspectable:

```text
summary.md
timeline.json
errors.txt
network.har
screenshots/
snapshots/
environment.json
redactions.json
```

Secrets are redacted by default. Cookies, auth-like keys, token-like keys,
request/response body-like fields, and sensitive URL query parameters require
`--include-secrets`.

## Focused Capabilities

### Profiles

Profiles are useful because auth and local state affect bugs.

```bash
kit cdp profile ls
kit cdp profile new clean
kit cdp profile clone authed-user --from checkout
kit cdp launch http://localhost:3000 --profile authed-user
```

Keep profile selection explicit. Never borrow the default user profile by accident.

### Environment Overrides

The launcher supports overrides that commonly explain bugs:

```bash
kit cdp launch http://localhost:3000 --timezone America/New_York
kit cdp launch http://localhost:3000 --locale fr-FR
kit cdp launch http://localhost:3000 --dark
kit cdp launch http://localhost:3000 --offline
kit cdp launch http://localhost:3000 --throttle slow-3g
```

Every override must be session-scoped and visible in `kit cdp state`.

Render mode and GPU launch evidence are also visible in launch/state output. Kit
records whether it passed `--headless=new` and whether GPU selection remains the
browser/application default; it does not guess a GPU policy or add `--disable-gpu`.

### Network Debugging

Network commands answer practical questions:

```bash
kit cdp net failed --app checkout
kit cdp net slow --app checkout
kit cdp net show req_18 --app checkout
kit cdp net block '*analytics*' --app checkout
kit cdp net mock GET /api/me fixtures/me.json --app checkout
kit cdp net rules --app checkout
kit cdp net rules clear --app checkout
```

Mock, block, offline, and throttle rules must be session-scoped and listed in
`state`.

### Visual And DOM Debugging

Keep the visual surface small:

```bash
kit cdp state --visual --app checkout
kit cdp snap -i --app checkout
```

`state --visual` combines screenshot path, visible URL/title, focused element,
and recent failures. `snap -i` remains the detailed element-ref command.

## Safety & Redaction

- Bind debug endpoints to localhost only.
- Use ephemeral ports by default.
- Never use the default Chrome profile unless explicitly requested.
- Redact cookies, auth headers, storage, and sensitive request bodies from bundles by default.
- Require `--include-secrets` for sensitive bundle export.
- Make environment and network overrides session-scoped.
- Print active overrides in `state`.
- Provide `net rules clear`.
- Provide `close --all` for cleanup.
- Keep raw events available; compact summaries are views, not filters.

## Non-Goals

- Do not build a general browser automation framework. Flows are bounded
  verification transcripts with evidence — no retries, no parallelism, no CI
  semantics, no waiting strategies beyond settle/idle. The moment a flow needs
  control flow, it should be an agent reading evidence and deciding, not a DSL.
- Do not expose every Chrome flag as a first-class CLI flag.
- Do not pretend compact summaries replace raw logs.
- Do not add AI interpretation inside `kit`; return structured evidence for an agent to interpret.
- Do not make tracing, coverage, or performance capture default. They should be explicit because they add cost and noise.

## Minimum Final Shape

The feature is successful if this workflow feels reliable:

```bash
kit cdp launch http://localhost:3000 --name checkout
kit cdp state --app checkout
kit cdp mark before --app checkout
kit cdp click "button:Place order" --app checkout
kit cdp after before --app checkout
kit cdp bundle checkout --since before
kit cdp close checkout
```

The important promise is not breadth. It is that the agent can always answer:

- what browser session am I debugging?
- did capture start before the app?
- what is true right now?
- what changed after my action?
- where is the raw evidence?
