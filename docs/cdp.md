# kit cdp — the warm CDP debugger

A warm, attach-based Chrome DevTools Protocol debugger for running Electron fleets.
Built on the shared `cdp` engine (`src/cdp/`); this is the daemon, the thin client,
and the command surface. For the vocabulary see [`CONTEXT.md`](../CONTEXT.md); for
the *why* of the shape see [`docs/adr/`](./adr).

## The model in three sentences

Every command talks to a warm **Attachment** — a background daemon bound to one
Instance's browser endpoint that holds the live connection and accumulates a
**Timeline**. The first command lazily spawns it; the rest are warm. It survives
HMR reloads (browser endpoint is stable) and app restarts (re-discovers by
selector), addresses **Targets** by stable **selector** (never a volatile id), and
disposes itself through idle timeout, bounded reconnect, signals, and registry
reconciliation; controlled close verifies ownership and retains recovery state when
shutdown cannot be proven.

## The agent contract

```
kit cdp                       # orient: instances available + live attachments
kit cdp launch http://localhost:3000 --name app
                              # launch isolated Chrome, attach before navigation
kit cdp eval 'location.href'  # just works — lazy-attaches, stays warm
kit cdp tail --since 3s       # all tracks on one clock — "what just happened"
kit cdp brief --since 30s     # agent-safe compact timeline with omission counts
kit cdp ls                    # health + the kill switch
```

And the verification loop — act, then ask what happened, in as few round trips
as possible:

```
kit cdp click 'button:Save settings'   # locator resolved live; waits for the dust to settle
kit cdp verify                         # PASS/FAIL since that click: ready · errors · failed net
kit cdp snap --diff                    # what changed on screen since the last snap
kit cdp do "click 'button:Save'; expect text 'Saved'; verify"
                                       # a whole sequence in ONE round trip
kit cdp flow run checkout-smoke       # the same sequence, saved and reusable
```

No attach ritual, no cached handles, no efficiency rules to remember. Add `--json`
to any command for structured output; pipe text output to `grep`/`head` freely.
Failed assertions exit non-zero, so `&&` chains behave.

## Commands

| Group | Command | What |
|---|---|---|
| Launch | `launch <url> --name <n>` · `launch-electron` · `launched` · `close <n>` · `profile …` | create and clean up controlled browser sessions |
| Lifecycle | `attach [--track …]` · `detach [--all]` · `ls` · `gc` | manage Attachments (attach is optional) |
| Observe | `tail [--since 5s] [--track …]` · `brief [--tail N] [--groups N]` · `console` · `net` · `ws` | slice or compact the Timeline |
| Triage | `errors [--explain]` | what's broken — error-shaped events deduped to `error (N×)`, with a `⚠` banner when the view is lossy |
| Probe | `state [--visual]` · `launch-log` · `snap [-i] [--diff]` · `eval <expr>/--file` · `heap` · `targets` | live one-shot query |
| Interact | `click <locator>` · `fill <locator> <text>` · `press <chord>` · `select <locator> <option>` | drive a Target; every interaction settles and (re)sets the `last-action` mark |
| Assert | `wait '<expr>'` · `expect text/eval/net/no-errors` · `verify` | self-verification: poll a condition, assert one fact, or get a composite PASS/FAIL — all exit non-zero on failure |
| Batch | `do "<step>; <step>"` · `flow ls/run/show` | run whole sequences daemon-side in one round trip; flows are saved, parameterized step files |
| Subscribe | `watch add/ls/rm/clear` | record a `watch` Timeline event whenever an expression's value changes |
| Instrument | `trace fn '<path>'` · `trace add <file:line> ['<expr>'] [--when …]` · `trace find '<text>'` · `trace ls/rm/clear` | record execution: fn calls (args · outcome · duration) and never-pausing logpoints, source-map aware, bound site read back at arm |
| Action window | `mark <name>` · `after <name>` · `bundle <name>` | summarize and export what changed after an action |
| Lens | `lens <name> [-- args]` | run a scriptable lens in a Target |
| Extensions | `ext doctor <id>` · `ext bundle <id>` | diagnose a Modular extension runtime view |

## Locators & settle

An interaction names its element one of three ways:

```
@e5                  ref from the last snap — fast, document-scoped
button:Save          role-scoped accessible-name match, resolved live at execution time
'Save settings'      bare name across all interactive roles
```

Exact name matches beat substring matches; an ambiguous locator fails listing the
candidates, and a missing one names the role it searched. Role:name locators take a
fresh accessibility snapshot when they run, so they survive navigations and re-renders
that kill `@refs` — use refs when you just snapped, locators when you're scripting.

After dispatching, `click`/`fill`/`press`/`select` wait for the Timeline to go quiet
(`--idle 300ms`, capped by `--timeout 2s`) and report what the action caused — event,
error, and network counts plus the recent lines. `--no-settle` returns immediately.
Every interaction also lands on the Timeline itself and (re)sets the `last-action`
mark, so `after last-action`, `tail --since-mark last-action`, and `verify` need no
bookkeeping. Clicks on a hidden/occluded window are delivered as synthetic
`el.click()` (real compositor input stalls without frames) — the reply says so.

## The verification loop

```bash
kit cdp wait '!document.querySelector(".spinner")' --timeout 10s
kit cdp expect text 'Saved'                                    # on-screen text, polled
kit cdp expect eval 'cart.items.length' --equals 3
kit cdp expect net '/api/save' --status 2xx --since-mark last-action
kit cdp expect no-errors --since-mark last-action
kit cdp verify                                                 # ready + errors + failed net
```

`verify` is the composite verdict: document complete, zero error-shaped events, zero
failed requests in the window — defaulting to "since the last interaction", so
`click … && verify` is a complete self-check. `expect net` failures print the
near-misses (same URL, different status): the diagnosis, not just the verdict.

## Batches and flows

`do` runs a sequence of session commands *inside the daemon* — one CLI round trip for
the whole interaction, one line of output per passing step, full evidence at the first
failure, remaining steps reported as skipped:

```bash
kit cdp do "click 'button:Save'; expect text 'Saved'; verify"
```

A **flow** is the same thing as a file: one step per line, `#` comments, `${param}`
placeholders. Steps are the exact CLI grammar — the same parser drives the CLI, the
interactive prompt, and flow files. Project flows live in `.kit/cdp/flows/<name>.flow`
(commit them: they are project knowledge every future session inherits); user flows in
the kit config dir. Project shadows user.

```bash
kit cdp flow ls
kit cdp flow run save-smoke user=Grace --app testbed
kit cdp flow show save-smoke
```

Each `do`/flow run sets a `do-start` mark — `tail --since-mark do-start` replays the
whole run's evidence, and `bundle --since do-start` exports it.

## Watches

`watch add` subscribes to a value: a daemon-side poller re-evaluates the expression
(default every 300ms) and records a `watch` Timeline event when the value changes —
on the same clock as console and network rows, so causality reads straight off `tail`:

```
+1410ms [app] net ← 200 POST /api/cart
+1422ms [app] watch cart 2 → 3
```

```bash
kit cdp watch add cart 'document.querySelectorAll(".cart-item").length'
kit cdp tail --track watch --since 2m
kit cdp watch ls · rm <name> · clear
```

Watches survive reloads (the poller re-resolves its target every tick); failed
evaluations are skipped, not recorded. Filter any Timeline slice with `--track watch`.

## Traces — execution on the Timeline

Where a watch records *values*, a trace records *behavior*: every run of a function
or a code location lands as a `trace` row interleaved with the console and network
rows it causes, with no code edits, no rebuilds, and **no pauses**:

```
+1203ms [app] trace store.js-84 {n: 2, t: "ADD"}
+1241ms [app] trace save (2 args) → {ok: true} 38ms
+1290ms [app] net ← 200 POST /api/save
```

```bash
kit cdp trace fn 'app.api.save'                       # wrap a live function: args · outcome · duration
kit cdp trace find 'groupId: options.group.id'        # live url:line coordinates — never grep a stale build
kit cdp trace add src/cart.js:84 'items.length'       # logpoint, resolved through source maps
kit cdp trace add renderer.js:108 '({ counter })' --when 'counter > 2'
kit cdp tail --track trace --since-mark trace-counter-108
kit cdp trace ls · rm <name> · clear
```

**Arming is a readback, not an echo.** The reply shows where V8 actually bound the
breakpoint (`src/cart.js:5 → bundle.js:8:3 (1 site)`), the original line's text when
the map embeds `sourcesContent` (`line  items.push(name);` — proof you're on the
code you meant), and the exact command to read results (a `trace-<name>` mark is set
at arm). `trace ls` then distinguishes the states that matter: `armed, awaiting
first hit` vs `N hit(s) · last 4s ago` vs `0 sites — no parsed script matches` vs
`⚠ stalled: <why>` when the keeper can't re-arm.

**Fn traces** wrap the function at a dotted path — which must be reachable from
`globalThis`, so in bundled ESM apps most functions need a logpoint instead (the
error says so). The wrapper preserves `this`/args/return/throw (constructors go
through `Reflect.construct`), records a bounded preview of each call, and the keeper
re-installs it after reloads. A traced *caught* throw renders as `✗` but is never
error-shaped — observation must not flip a `verify`. Honest limits, disclosed in the
reply: calls through references saved before wrapping are invisible, and thenables
return as a *derived* promise (same settlement, different identity).

**Logpoints** are breakpoints whose condition records and returns false — V8 never
pauses. Expressions are compile-checked at arm time — pieces *and* the assembled
condition, so a syntax error fails the `add` instead of arming a breakpoint that V8
would silently never fire — and an expression that *throws at runtime* (TDZ, a name
not in that frame's scope) ships the error as the row's value, never a silent skip.
Locations resolve three ways: a script-URL suffix (`renderer.js:144`, query-string
tolerant for dev servers), an absolute URL, or a **repo path through the source-map
registry** (`src/cart.js:5 → bundle.js:8`), which the daemon builds from
`Debugger.scriptParsed` and feeds by fetching maps through the page (so `modular://`
and asar apps resolve too). `--when` gates recording; one breakpoint per location
(the error names the trace holding it). When the script under a logpoint re-parses
(HMR rebuild, rotating `?t=` stamps), the keeper re-arms it and `ls` shows
`re-armed N×` — coordinates heal instead of dying silently.

**`trace find`** searches the *parsed* scripts of the live session for a literal
string and returns `url:line` plus the line's text — coordinates from the code that
is actually executing, immune to the build-output drift that makes grepping a bundle
on disk a trap.

Both kinds rate-cap **in the page** (default 20 hits/s, `--rate`): past the cap the
page counts drops and the Timeline shows `trace hot ⚠ 495 hit(s) suppressed` rows —
exact counts, never silent loss. The cap protects the wire and the Timeline, not the
app's CPU: V8 still evaluates the condition per hit, and the containing function
runs deoptimized while a logpoint is armed.

Arming a logpoint enables the Debugger domain (lazily, per session). That makes
`debugger;` statements real pauses — kit auto-resumes them (and records that it did)
unless a DevTools window is attached, in which case pauses belong to the human.

**Source-mapped errors** ride the same registry: exception stacks gain their
original location on the headline — `TypeError: … → src/cart.js:14` — automatically
when a map is already loaded, and `kit cdp errors --resolve` force-loads maps for
the frames in view (enabling Debugger where needed, same disclosure).

## Snap diff

`snap --diff` answers "what did the UI just do?" with no authored assertions: it
compares ref-free semantic lines (role, name, value, text) against the previous
explicit snap and prints what appeared and disappeared:

```
snap diff vs +42130ms
- text "5"
+ text "6"
+ listitem level=1
+ text "item 3"
4 added · 1 removed · 93 unchanged
```

A value change reads as a remove/add pair; reordering alone is not a change (refs
renumber and layout shifts — identity is the line, not the position). Every explicit
`snap` resets the baseline.

Selectors: `--app <instance>` picks the Attachment (app name / worktree / instance
id / port); `--target <selector>` picks the Target (defaults to the main app
window). Timeline slices accept `--source main|renderer`, `--target <selector>`,
`--grep <text>`, `--extension <id>`, and `--limit <n>`. `--since` takes `500ms`
/ `2s` / `5m`.

Console rows keep their compact one-line text for `tail`, but `tail --json`,
`console --json`, and bundle `timeline.json` also include each console argument
under `args`. Primitive args carry `value`; object/function args preserve CDP's
bounded `preview` and, when the live handle can be read immediately, a JSON-safe
`snapshot` capped by depth, property count, array length, and string length.
Snapshot failures stay on the arg as `snapshotError` so the original console row
is never dropped. Page-side snapshots are budgeted at eight objects per event and
sixteen per second per page; previews/text remain intact and rate-limited objects are
annotated rather than silently skipped.

`brief` is the low-context handoff for coding agents. It does not delete or
semantically filter the Timeline: it groups errors with the same integrity banners
as `errors`, collapses repeated non-error rows to counts, shows a short raw tail,
and prints exactly how many rows were not shown verbatim. If ignored rows, ring
eviction, undecoded error-domain events, `--limit`, or older one-off logs could
hide useful detail, the brief says so and points back to `tail` / `errors
--explain`.

## Controlled launcher

`kit cdp launch <url> --name <session>` starts Chrome on an isolated profile,
binds the DevTools endpoint to localhost on an ephemeral port, attaches the daemon,
enables capture, then navigates. Startup capture is on by default; use
`--no-startup-capture` only when early boot logs do not matter.

Useful launch flags:

```bash
kit cdp launch http://localhost:3000 --name checkout --headless
kit cdp launch http://localhost:3000 --name checkout --viewport 1440x1000
kit cdp launch http://localhost:3000 --name checkout --timezone America/New_York
kit cdp launch http://localhost:3000 --name checkout --profile authed-user
kit cdp launch http://localhost:3000 --name checkout --reuse
kit cdp launch http://localhost:3000 --name checkout --replace
```

Names are session identities: `launch` fails when a name already exists unless
`--reuse` or `--replace` is passed, and explicit names may contain only ASCII
letters, digits, `-`, and `_`. `detach` stops capture only; `close` closes the
browser, removes the launch record, and deletes temporary profiles unless
`--keep-profile` was used. Artifacts remain under the CDP artifact directory.

Launch records store the browser-level websocket URL plus the controlled session's
pid start times, process group/session, and CDP-port owner. `close` uses CDP only when
both endpoint and process identity match, then verifies session shutdown. It can
terminate a verified owned session when CDP is unreachable, but never signals an
unverified or reused pid. A failed/ambiguous close is non-zero, does not print
`closed`, and retains the launch record/profile for recovery. Cloned profiles skip
Chrome singleton files and `DevToolsActivePort`; launch removes stale launch-state
files before waiting for the new browser.

The ownership record is persisted before the Attachment is spawned. Every later
failure—daemon spawn/readiness, launch mark, configuration, navigation, renderer
selection, or final output—uses the same post-ownership cleanup boundary. That
boundary verifies both the controlled process session and the exact spawned daemon;
on incomplete cleanup it keeps the launch record/profile. If the initial registry
write itself fails, an artifact-side `launch-recovery.json` preserves the ownership
proof only as recovery evidence; it is never an alternate runtime registry.

Launch and `state` output report render mode and GPU evidence. `gpu browser-default
(no Kit GPU flag)` means Kit passed no GPU policy; `--headless` records
`headless=new`. Kit does not default to `--disable-gpu`.

The agent loop is:

```bash
kit cdp state --app checkout
kit cdp launch-log --app checkout
kit cdp mark before-save --app checkout
kit cdp click @e5 --app checkout
kit cdp after before-save --app checkout
kit cdp bundle checkout --since before-save
kit cdp close checkout
```

`state` reports the selected target, document readiness, recent errors, failed
network, focus, active overrides/rules, and a screenshot path with `--visual`.
`after` waits until idle or timeout and always prints a raw `tail --since-mark`
escape hatch. `bundle` writes `summary.md`, `timeline.json`, `errors.txt`,
`network.har`, `environment.json`, and `redactions.json`; cookies, auth-like keys,
tokens, request/response body-like fields, and sensitive URL query params are
redacted unless `--include-secrets` is passed. A typoed mark fails explicitly with
`unknown mark '<name>'`; it never falls back to an unrelated time window.

Network helpers stay session-scoped:

```bash
kit cdp net failed --app checkout
kit cdp net slow --app checkout
kit cdp net show req_18 --app checkout
kit cdp net block analytics --app checkout
kit cdp net mock GET /api/me fixtures/me.json --app checkout
kit cdp net rules clear --app checkout
```

### Launcher command reference

| Command | Use |
|---|---|
| `launch <url>` | Start an isolated Chrome session, attach CDP, configure capture, then navigate. |
| `launched` | List active launched sessions after verifying endpoint identity. |
| `close <name>` / `close --all` | Stop verified launched browser session(s); remove records/profiles only after verified shutdown. |
| `state [--visual]` | Print current target, readiness, failures, focus, rules, and optional screenshot path. |
| `launch-log` | Show the Timeline from the built-in `launch` mark. |
| `mark <name>` | Add a named Timeline mark for later `--since-mark`, `after`, and `bundle --since`. |
| `after <mark>` | Wait for idle or timeout, then summarize events since the mark. |
| `bundle [name]` | Export a redacted evidence folder for handoff. |
| `profile ls/new/clone` | Manage explicit named browser profiles; never uses the normal Chrome profile. |
| `net failed/slow/show/block/mock/rules` | Inspect and mutate session-scoped network behavior. |

Key launch flags:

| Flag | Meaning |
|---|---|
| `--name <name>` | Session identity; required for predictable multi-session work. |
| `--headless` | Use Chrome's modern headless mode. |
| `--profile <name>` | Use an explicit kit CDP profile. |
| `--fresh` | Require a temporary profile; cannot be combined with `--profile`. |
| `--keep-profile` | Keep the temporary profile after `close`. |
| `--viewport <WxH>` | Set window size and device metrics. |
| `--timezone`, `--locale`, `--dark` | Apply session-scoped environment overrides. |
| `--offline`, `--throttle slow-3g|fast-3g` | Apply session-scoped network emulation. |
| `--reuse` | Reuse and navigate an existing launched session with matching identity. |
| `--replace` | Close the existing launched session, then launch a new one. |
| `--no-startup-capture` | Skip the about:blank attach-before-navigation flow. |

## Tracks

Captured by default from attach or launch: `console`, `exception`, `log`,
`network`, `ws`, `lifecycle`. The `watch` track is daemon-generated (see Watches),
filterable like any other but never "enabled". Heap is **not** a track — `kit cdp
heap` probes on demand (continuous heap sampling would perturb the very memory it
measures). Restrict any Timeline slice with `--track net,ws`.

## Lenses

Generic core, scriptable edge: the engine ships **no** app knowledge. A lens is a
JS file in the kit config dir under `cdp/lenses/<name>.js` (macOS:
`~/Library/Application Support/kit`, Linux: `~/.config/kit`), run inside a Target,
that decodes app-specific state the engine deliberately doesn't know — a custom
editor's document model, a sync engine's state, the app's routes. The script body
receives `args`, may `await` app promises (it runs as an async function), and
`return`s a JSON value:

```js
// <config>/cdp/lenses/title.js   →   kit cdp lens title
const fonts = await document.fonts.ready;
return { title: document.title, url: location.href, fonts: fonts.size };
```

Starters ship in the binary and can be shadowed by files of the same name in that
directory.

Built-in lenses:

- `workbench` — generic Modular workbench orientation: page state, workspace id,
  active editor when the test bridge exposes it, and recent app errors when present.
- `extensions` — Modular extension runtime diagnosis from the workbench test
  bridge. It reads `window.__testAPI.runtimeGraph.getSnapshot()` plus webview-live
  probes when available, joins that with CDP webview target metadata, and reports
  view health, document load, bridge status, HMR state, blockers, actions, and
  recent runtime events.

Useful extension flows:

```bash
kit cdp attach --app modular-dev
kit cdp ready --app modular-dev --json
kit cdp targets --app modular-dev --json
kit cdp lens extensions --app modular-dev -- modular.local-sdk-view-showcase
kit cdp ext doctor modular.local-sdk-view-showcase --app modular-dev --json
kit cdp ext bundle modular.local-sdk-view-showcase --since 60s --limit 50 --app modular-dev --json
kit cdp console --extension modular.local-sdk-view-showcase --since 5m --app modular-dev
```

`ext bundle` is the agent handoff shape: it returns the same extension diagnosis as
`ext doctor` plus a bounded Timeline slice filtered to that extension id. Use it
after pre-warming the Attachment and reproducing the issue so console/network/HMR
events are captured on the same clock.

## Testbed

`testbed/` in this repo is a tiny Electron playground that emits errors, network
traffic, websocket frames, and state changes on demand — the live environment every
`kit cdp` feature is verified against. `testbed/README.md` has the launch line and a
map of which UI section exercises which command.

## Interactive mode

`kit cdp -i` drops you *inside* an Instance: a full-screen split with the Timeline
streaming live on top and a command line on the bottom. The first command resolves
and lazy-attaches like any other; from then on every line runs against that same
warm Attachment, and command output lands inline on the feed — so an `eval` and the
network calls it triggers sit next to each other on one clock.

```
┌─ kit cdp ─ modular-dev :9223 ─ ● live ──────────────────┐
│ track: all   ·   source: all   ·   target: * main       │
├─ timeline ─ ● live ─────────────────────────────────────┤
│ +1203ms [main]  console.log user {id: 5}                 │
│ +1410ms [main]  net ← 200 /api/me                        │
│ ┌ eval document.title                                    │
│ │ "Workspace · ari"                                      │
│ └                                                        │
├─────────────────────────────────────────────────────────┤
│ cdp› snap -i                                             │
└─ ⏎ run · ⇥ complete · ↑↓ history · PgUp/PgDn · ^D quit ──┘
```

Two grammars meet at the prompt:

- **Session commands** are the *exact* CLI grammar — `eval 'location.href'`,
  `tail --since 3s`, `snap -i`, `click @e5`, `ignore <substr>`. Flags and `--help`
  work identically; a bad flag prints the error into the feed, it never crashes the
  session.
- **Meta commands** are interactive-only view state:
  - **`Tab`** (empty prompt) opens the **target picker** — a fuzzy, activity-ranked
    list of every target in the instance. Type to narrow (`work`→workspace), `↑↓`
    to move, `⏎` to focus, `Esc` to cancel. **Focus** is the DevTools-context model:
    the chosen target both *filters the feed* and becomes the *default `--target`*
    for `eval`/`snap`/`click`. The `✸ all targets` row clears focus.
  - `target <text>` focuses by fuzzy text directly (no modal); `target main` clears.
  - `track net,ws` / `track all` — filter the live pane by track (instant, no
    re-subscribe).
  - `source main` / `source renderer` / `source all` — filter by process side.
  - `clear` · `help` · `quit`.

Keys: `⏎` run · suggestions appear as you type (ghost hint inline; `⇥`/`↓` select,
`⏎`/`→` accept, `Esc` hide; `@` narrows to live elements; empty-prompt `⇥` opens the
target picker) · `^P`/`^N` history (persisted under the
config dir) · `PgUp`/`PgDn` scroll the feed · `End`/`Esc` re-pin to live · `^L`
clear · `^D` quit. The feed header shows `● live` when pinned and `▲ N below` when
you've scrolled up. The live pane survives HMR reloads and app restarts on the same
subscription (`docs/adr/0004`).

## How it stays out of your way

- **Reloads** don't drop the Attachment — it re-binds the recreated Target and
  records the navigation on the Timeline (`docs/adr/0002`).
- **Restarts** trigger bounded, capped-backoff re-discovery by selector, not an
  infinite `/proc` spin.
- **Disposal** is verified when ownership is known: idle timeout (~30 min), reconnect
  give-up, signals, and registry reconciliation cover Attachments; controlled close
  retains recovery state and fails explicitly when process ownership or shutdown is
  ambiguous (`docs/adr/0003`).
