# kit cdp — full command surface

Every command accepts `--json` (structured output) and `--app <selector>` (which
attachment: app name, worktree, instance id, or port). Failed assertions and failed
steps exit non-zero. Text output pipes cleanly to `grep`/`head`.

## Launch & lifecycle

| Command | What |
|---|---|
| `launch <url> --name <n>` | Start isolated Chrome, attach **before** navigation, configure capture, then navigate. |
| `launch-electron --name <n> --cwd <dir> -- <cmd…>` | Launch an Electron app, wait for the renderer CDP endpoint, attach. `--cdp-env <VAR>` passes the chosen port to the app's environment; `--renderer-target <sel>` pins which target is "the app". |
| `launched` | List launched sessions (endpoint identity verified — stale records can't lie). |
| `close <name>` / `close --all` | Stop launched browser(s), remove records, delete temp profiles (`--keep-profile` to keep). |
| `attach [--track …]` | Pre-warm an attachment (optional — every command lazy-attaches). Use it to start capture *before* reproducing a bug. |
| `detach [--app <n>]` / `detach --all` | Stop capture only; the app keeps running. Prefer targeted detach over `--all`. |
| `ls` / `gc` | Live attachments + health / sweep dead ones. |
| `profile ls/new/clone` | Named browser profiles for launches. Never touches the real Chrome profile. |

Launch flags: `--headless`, `--viewport 1440x1000`, `--timezone`, `--locale`,
`--dark`, `--offline`, `--throttle slow-3g|fast-3g`, `--profile <name>`, `--fresh`,
`--reuse` (navigate existing session), `--replace` (close + relaunch),
`--no-startup-capture`. Names: ASCII letters, digits, `-`, `_`.

## Observe (Timeline slices)

All slices accept `--since 500ms|2s|5m`, `--since-mark <name>`, `--track <t,…>`,
`--source main|renderer`, `--target <selector>`, `--grep <text>`,
`--extension <id>`, `--limit <n>`.

| Command | What |
|---|---|
| `tail` | Raw rows, all tracks on one clock — "what just happened". |
| `brief` | Agent-safe digest: error groups, repeated noise collapsed to counts, short raw tail, and **exact omission counts**. Compresses presentation only — it tells you when to fall back to `tail`. |
| `errors [--explain]` | Error-shaped events (exceptions · console.error · log/error · failed requests) deduped to `error (N×)`. A `⚠` banner fires if the view is lossy; `--explain` expands what each group absorbed. |
| `console` / `net` / `ws` | Single-track slices. `net` also hosts the network tools below. |
| `launch-log` | The Timeline from before navigation (startup capture). |

Tracks captured from attach: `console`, `exception`, `log`, `network`, `ws`,
`lifecycle`. The `watch` track is daemon-generated. Heap is not a track —
`kit cdp heap` probes on demand.

## Probe

| Command | What |
|---|---|
| `eval '<expr>'` / `eval --file <js>` | Evaluate JS in a target, return the value. |
| `snap [-i] [--diff]` | Accessibility-tree snapshot with `@eN` refs. `--diff` prints added/removed semantic lines vs the previous snap (reorder ≠ change; every explicit snap resets the baseline). |
| `ready` | Is the app up? Selected target, document state, recent errors, ranked candidates with why each won. |
| `state [--visual]` | Readiness, recent failures, focus, active net rules; `--visual` writes a screenshot and prints its path. |
| `targets` | Every target with its stable selector. |
| `heap` | JS heap + DOM counters, on demand. |

## Interact

Each interaction resolves its locator live, dispatches, waits for the Timeline to
go quiet (`--idle 300ms`, capped by `--timeout 2s`; `--no-settle` to skip), reports
what it caused (events, errors, network), and (re)sets the `last-action` mark.
Clicks on hidden/occluded windows fall back to synthetic `el.click()` — the reply
discloses it.

| Command | What |
|---|---|
| `click <locator>` | Click. Locators: `@e5` (last snap), `button:Save` (role:name, live), `'Save settings'` (bare name, interactive roles). |
| `fill <locator> <text>` | Focus + set value + input events. |
| `press <chord>` | Key into the focused element: `Enter`, `Tab`, `Escape`, arrows, `Ctrl/Meta/Alt/Shift+<key>`. |
| `select <locator> <option>` | Choose a select/combobox option by visible label (or value). |

## Assert

| Command | What |
|---|---|
| `wait '<expr>' [--timeout 10s]` | Poll until a JS expression is truthy — the precise form of "sleep and check". |
| `expect text '<s>'` | On-screen text appears (polled). |
| `expect eval '<expr>' [--equals <v>]` | Expression truthy, or equals a value. |
| `expect net '<url-substr>' [--status 2xx]` | A matching response in the window. Failure prints near-misses (same URL, different status). |
| `expect no-errors` | Zero error-shaped events in the window. |
| `verify` | Composite PASS/FAIL: document ready · no errors · no failed requests. Window defaults to since `last-action`. |

## Batch & flows

| Command | What |
|---|---|
| `do "<step>; <step>; …"` | Run steps daemon-side in one round trip. Steps are the exact CLI grammar. Stops at first failure with full evidence; remaining steps reported skipped. Sets a `do-start` mark. |
| `flow ls / show <name> / run <name> [k=v …]` | Saved step files: one step per line, `#` comments, `${param}` placeholders. Project flows in `.kit/cdp/flows/` (commit them); user flows in the kit config dir. Project shadows user. |

## Subscribe

| Command | What |
|---|---|
| `watch add <name> '<expr>' [--interval 300ms]` | Daemon-side poller; records a `watch` Timeline event on value change. Survives reloads; failed evals are skipped, not recorded. |
| `watch ls / rm <name> / clear` | Manage watches. |

## Instrument (trace)

| Command | What |
|---|---|
| `trace fn '<path>' [--name n] [--rate 20]` | Wrap the live function at a dotted path — **must be reachable from `globalThis`**; module-scoped functions need a logpoint instead (the error redirects you). Every call records args, return/throw preview, and duration. Preserves `this`/args/return/throw and constructors; survives reloads via the keeper (≤1s re-install gap). Disclosed limits: calls through pre-wrap saved references are unseen; thenables return as derived promises; a call landing while another record is mid-flight passes through unrecorded. |
| `trace add <file:line[:col]> ['<expr>'] [--when '<cond>'] [--name n] [--rate 20]` | Logpoint: a breakpoint whose condition records and returns false — the app never pauses. Location is a script-URL suffix (query-string tolerant), absolute URL, or repo path resolved through the source-map registry. The reply **reads back the bound site** (`src/cart.js:5 → bundle.js:8:3 (1 site)`) and the source line's text when the map embeds content, and sets a `trace-<name>` mark. Expressions compile-checked at arm (pieces and assembled condition); runtime throws ship as the row's value, never a silent skip. One breakpoint per location. Re-arms automatically when the script re-parses (`re-armed N×` in `ls`). |
| `trace find '<text>' ` | Search the live session's *parsed* scripts for a literal string → `url:line` plus the line's text, capped at 20 hits. The coordinate source for `trace add` — never grep a build output (it drifts; the parsed script can't). |
| `trace ls / rm <name> / clear` | `ls` distinguishes states: `armed, awaiting first hit` / `N hit(s) · last 4s ago` / `0 sites — no parsed script matches` / `⚠ stalled: <reason>`, plus suppressed and transport-loss counts. `rm` restores the original function or removes the breakpoint. Max 32 traces per attachment. |
| `errors --resolve` | Force-load source maps so exception stacks resolve to original files on the headline. A map that fails to load leaves one Timeline marker saying why. |

Rate caps count in-page and emit exact `suppressed N` Timeline rows — never silent
loss. Arming a logpoint enables the Debugger domain: `debugger;` statements then
pause and are auto-resumed (recorded on the Timeline) unless DevTools is open, and
the containing function runs deoptimized while armed. Trace rows are never
error-shaped: observing a caught throw cannot flip `verify`.

## Action windows & evidence

| Command | What |
|---|---|
| `mark <name>` | Named Timeline mark. Typos in `--since-mark` fail loudly — never a silent fallback window. |
| `after <name>` | Wait for idle (or timeout), then summarize events since the mark. Always prints the raw `tail --since-mark` escape hatch. |
| `bundle [name] [--since <mark>]` | Redacted evidence folder: `summary.md`, `timeline.json`, `errors.txt`, `network.har`, `environment.json`, `redactions.json`. Cookies, tokens, auth-ish keys, body-like fields, sensitive query params redacted unless `--include-secrets`. |

## Network rules (launched sessions)

```bash
kit cdp net failed            # failed requests
kit cdp net slow              # slowest requests
kit cdp net show req_18       # one request in full
kit cdp net block analytics   # block by substring
kit cdp net mock GET /api/me fixtures/me.json
kit cdp net rules clear
```

## Lenses & extensions

A **lens** is a JS file run inside a target that decodes app-specific state the
engine deliberately doesn't know. User lenses live in the kit config dir under
`cdp/lenses/<name>.js` (macOS: `~/Library/Application Support/kit`, Linux:
`~/.config/kit`); the script body gets `args`, may `await`, and `return`s JSON.

```bash
kit cdp lens <name> [-- args]
```

Built-ins: `workbench` (Modular workbench orientation) and `extensions` (extension
runtime diagnosis). Extension shortcuts:

```bash
kit cdp ext doctor <extension-id>                  # view health, bridge, HMR, blockers
kit cdp ext bundle <extension-id> --since 60s      # diagnosis + bounded Timeline slice
```

## Noise control

```bash
kit cdp ignore '<substring>'   # suppress matching rows (per attachment)
kit cdp ignore --list / --clear
```

Ignored rows count toward `brief`/`errors` omission banners — suppression is never
silent.
