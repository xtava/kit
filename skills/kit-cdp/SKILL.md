---
name: kit-cdp
description: Drive, verify, and instrument live Electron and Chrome apps with `kit cdp`, a warm Chrome DevTools Protocol debugger CLI. Use when you need to self-verify UI work in a running app (click/fill/press, then get a PASS/FAIL verdict), record a video of the app actually being driven (screencast → mp4) or take timestamped screenshots as evidence, trace whether a function or code line actually ran and with what values (fn traces and never-pausing logpoints — no console.log edits, no rebuilds; arms read back the exact bound site), search the running app's parsed scripts for code to instrument (live url:line coordinates, immune to bundle drift), resolve minified stack traces back to original source files, reproduce and diagnose console errors, exceptions, failed network requests, or websocket traffic, inspect a live page's accessibility tree, watch an app value change over time, or capture a redacted evidence bundle of what an interaction caused. One command attaches lazily and stays warm — no setup step, no driver scripts; every command takes --json.
license: MIT
metadata:
  source: https://github.com/xtava/kit
---

# kit cdp — verify your work against the live app

`kit cdp` keeps a **warm attachment** to a running app: a background daemon holds
the CDP connection and accumulates one correlated **Timeline** (console · exceptions
· network · websocket · lifecycle) on a single clock. The first command attaches
lazily — there is no setup step — and the attachment survives HMR reloads and app
restarts. You act on the app, then ask what happened, in as few round trips as
possible.

Requires the `kit` binary on PATH (`cargo install --path .` from the kit repo).

## Orient first

```bash
kit cdp                     # instances available + live attachments
kit cdp ready --app dev     # is the app up? which target won, and why
kit cdp state --app dev     # readiness, recent failures, focus; --visual adds a screenshot
kit cdp targets --app dev   # every target, with the selector that addresses it
```

`--app <name>` selects the attachment (app name / worktree / instance id / port).
With a single instance it can be omitted; in scripts, always pass it.

## Three ways in

```bash
# 1. The app is already running with a debug port → just use any command (lazy attach):
kit cdp eval 'location.href' --app dev

# 2. You control the browser → launch isolated Chrome, attached before navigation:
kit cdp launch http://localhost:3000 --name checkout --headless

# 3. It's an Electron app → launch it and attach to the renderer it exposes:
kit cdp launch-electron --name app --cwd app-dir -- ./node_modules/.bin/electron .
```

Launched sessions are cleaned up with `kit cdp close <name>` (stops the browser);
`detach` only stops capture and leaves the app running. Never `detach --all` on a
shared machine — other attachments may belong to someone else's session.

Extra Chrome flags: `--chrome-arg <flag>` (repeatable) and `KIT_CDP_CHROME_ARGS`
(whitespace-split) append after the built-in flags — env first, then flags.
Headless Linux containers typically need `--no-sandbox --disable-dev-shm-usage`.

## The one rule about capture

**CDP never replays the past.** The Timeline records from attach onward. To capture
errors that fire during load, compile, or reload: attach *first*, then reproduce.

```bash
kit cdp attach --app dev          # warm BEFORE the error fires
# …save the file / reload the window / trigger the bug…
kit cdp errors --since 30s --app dev
```

## The verification loop

Every interaction resolves its element live, waits for the Timeline to settle, sets
the `last-action` mark, and reports what it caused. `verify` defaults its window to
"since the last interaction", so the minimal self-check is two commands:

```bash
kit cdp click 'button:Save settings' --app dev
kit cdp verify --app dev            # PASS/FAIL: document ready · no errors · no failed net
```

Sharper assertions when the composite isn't enough — all exit non-zero on failure,
so `&&` chains and CI steps behave:

```bash
kit cdp wait '!document.querySelector(".spinner")' --timeout 10s
kit cdp expect text 'Saved'                                   # on-screen text, polled
kit cdp expect eval 'app.cart.items.length' --equals 3
kit cdp expect net '/api/save' --status 2xx --since-mark last-action
kit cdp expect no-errors --since-mark last-action
kit cdp snap --diff                 # what changed on screen since the last snap — no assertions needed
```

`expect net` failures print near-misses (same URL, different status) — the
diagnosis, not just the verdict.

## Locators

```
@e5                ref from the last `snap` — fast, but dies on navigation/re-render
button:Save        role-scoped accessible-name match, resolved live at execution time
'Save settings'    bare name across all interactive roles
```

`kit cdp snap` prints the accessibility tree with `@eN` refs. Use refs immediately
after a snap; use `role:name` when scripting (each run takes a fresh snapshot, so it
survives re-renders). Exact matches beat substring; ambiguity fails listing the
candidates. Interactions: `click <loc>` · `fill <loc> <text>` · `press <chord>`
(e.g. `Enter`, `Meta+S`) · `select <loc> <option>`.

## Batch it: do and flows

`do` runs a whole sequence *inside the daemon* — one round trip, one line per
passing step, full evidence at the first failure:

```bash
kit cdp do "click 'button:Save'; expect text 'Saved'; verify" --app dev
```

A **flow** is the same grammar saved as a file (one step per line, `#` comments,
`${param}` placeholders) in `.kit/cdp/flows/<name>.flow`. Commit project flows —
they are verification knowledge every future session inherits:

```bash
kit cdp flow ls
kit cdp flow run save-smoke user=Grace --app dev
```

## Record video and take shots — evidence humans can watch

```bash
kit cdp record start --app dev          # screencast of the selected target → frames on disk
# …drive the app: click / fill / flows…
kit cdp record stop --app dev --json    # {"video": "<mp4>", "frames": N, "durationMs": N, "framesDir": "<dir>"}
kit cdp flow run smoke --record --app dev   # whole run wrapped: start → steps → stop + assemble
kit cdp shot --app dev --json           # {"screenshot": "…/shots/shot-<unix-ms>.png"} — never overwrites
```

Frames are throttled to `--fps` (default 4) and buffered to
`recordings/rec-<unix-ms>/` with a `frames.json` manifest; `stop` assembles an mp4
(h264/yuv420p/+faststart) using the real inter-frame durations when ffmpeg is on
PATH — without ffmpeg the frames dir is kept and the reply says so (`video` is
`null`, never an error). One recording per attachment; it lives in the warm daemon
across CLI calls and re-arms itself across HMR reloads. A `--record` run that fails
mid-way still stops and assembles — the video of the failure is the evidence.
`record start` / `record stop` are also valid flow/do steps for scoping.

## Watch values change

```bash
kit cdp watch add cart 'document.querySelectorAll(".cart-item").length' --app dev
kit cdp tail --track watch --since 2m --app dev
```

A daemon-side poller records a `watch` event whenever the value changes — on the
same clock as console and network rows, so causality reads straight off `tail`.
Watches survive reloads. `watch ls / rm <name> / clear` manage them.

## Trace execution — instead of adding console.logs

The replacement for the edit-log-rebuild-reproduce loop. Instrument the *running*
app — no code edits, no rebuilds, no pauses — and read execution interleaved with
its side effects:

```bash
kit cdp trace fn 'app.api.save' --app dev            # every call: args → outcome, duration
kit cdp trace find 'groupId: opts.group.id'          # live url:line coordinates from parsed scripts
kit cdp trace add src/cart.js:84 'items.length'      # logpoint at a repo path (source maps)
kit cdp trace add renderer.js:108 '({ counter })' --when 'counter > 2'
kit cdp tail --track trace --since-mark trace-counter-108 --app dev
kit cdp trace ls / rm <name> / clear
```

```
+1241ms [app] trace save (2 args) → {ok: true} 38ms
+1290ms [app] net ← 200 POST /api/save
```

**`trace fn` only reaches paths on `globalThis`** — in bundled ESM apps most
functions are module-scoped, so use a logpoint: `trace find '<fnName>('` gives the
line, `trace add <url:line> '([...arguments])'` records the calls.

**Verify the arm, don't assume it.** The `add` reply reads back where V8 actually
bound the breakpoint (`src/cart.js:5 → bundle.js:8:3 (1 site)`) and the source
line's text when the map embeds content — if it shows `return;`, your line guess
was wrong; fix it now, not after six silent repros. Every arm sets a `trace-<name>`
mark and prints the exact `tail` command to read results. `trace ls` separates
`armed, awaiting first hit` / `N hit(s) · last 4s ago` / `0 sites — no parsed
script matches` / `⚠ stalled: <reason>` — `0 hit(s)` is never ambiguous. Never grep
a build output for line numbers; `trace find` searches what is *actually executing*.

Traces survive reloads (the keeper re-arms on script re-parse and says `re-armed
N×`; fn re-installs have a ≤1s gap). Expressions are compile-checked at arm time —
a typo fails the add, never arms a silently-dead trace — and a runtime throw in the
expression ships as the row's value (`(expr threw: …)`), so a wrong variable name is
evidence, not silence. Past `--rate` (default 20/s, max 32 traces) the page counts
drops and the Timeline shows exact `suppressed N` rows. A traced *caught* throw
shows as `✗` but never flips `verify`. Log several values with `'({a, b})'`.

One side effect, disclosed: arming a logpoint enables the Debugger domain, so
`debugger;` statements pause — kit auto-resumes them unless DevTools is open.

Exception stacks resolve through the same source maps: `kit cdp errors --resolve`
turns `bundle.js:48211` into `src/cart.js:14` on the error's headline.

## Read the Timeline cheaply

```bash
kit cdp brief --since 30s          # agent-safe digest: errors grouped, noise counted, never silently lossy
kit cdp errors --since 5m          # error-shaped events deduped to `error (N×)`; --explain expands groups
kit cdp tail --since 3s            # raw rows, all tracks on one clock
kit cdp console / net / ws         # single-track slices; --grep, --source main|renderer, --limit
```

Scope every read: `--since 5s|2m`, `--since-mark <name>`, or bound an action
yourself with `mark <name>` → act → `after <name>` (waits for idle, summarizes).
`do` and flow runs set a `do-start` mark automatically.

## Hand off evidence

```bash
kit cdp bundle checkout --since before-save
```

Writes `summary.md`, `timeline.json`, `errors.txt`, `network.har`,
`environment.json`, plus `screenshots/` (shots taken this session) and
`recordings/` (the most recent completed recording's mp4, or its frames dir if
unassembled) — secrets redacted by default; the `--json` reply lists every
artifact. This is the artifact to attach to an issue or hand to another agent.

## Going deeper

- [references/commands.md](references/commands.md) — the full command surface:
  launcher flags, trace/instrumentation semantics (rate caps, Debugger side
  effects, disclosed limits), network rules (block/mock/throttle), profiles,
  heap, lenses, extension diagnosis, marks.
- [references/recipes.md](references/recipes.md) — worked end-to-end sequences:
  verifying a feature, diagnosing a reload crash, tracing instead of
  console.log, mapping minified stacks to the repo, form workflows,
  watch-driven debugging, flow authoring.
