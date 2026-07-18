---
name: kit-cdp
description: >-
  Drive, verify, and instrument live Electron or Chrome apps with `kit cdp`, a
  warm Chrome DevTools Protocol debugger CLI. Use for UI self-verification
  against a running app, CDP clicks/fills/presses, screenshots, accessibility
  snapshots, console or network error diagnosis, live logpoints, function traces,
  source-map stack resolution, websocket inspection, value watches, or evidence
  bundles. Resolves the app owned by the current Git worktree automatically,
  supports intentional cross-instance selection with `--app <selector>`, and
  every command supports `--json`.
license: MIT
metadata:
  source: https://github.com/xtava/kit
---

# kit cdp — verify your work against the live app

`kit cdp` keeps a **warm attachment** to a running app: a background daemon holds
one CDP connection **per instance** and accumulates a correlated **Timeline**
(console · exceptions · network · websocket · lifecycle) on a single clock per
instance. The first command attaches lazily — there is no setup step — and the
attachment survives HMR reloads and app restarts. You act on the app, then ask what
happened, in as few round trips as possible.

**Expect several instances at once.** Multiple worktrees, dev ports, or launched
sessions run side by side, each its own attachment and its own Timeline — they never
merge. Normal commands select the instance owned by the current Git worktree. Use
`--app <selector>` only as an intentional cross-instance or launched-session override.

Requires the `kit` binary on PATH (`cargo install --path .` from the kit repo).

## Start with the operation

```bash
kit cdp screenshot          # current worktree, lazy attachment
kit cdp state --visual      # readiness, recent failures, focus, screenshot
kit cdp ready               # is the app up? which target won, and why
```

There is no orientation or attach prerequisite. Use bare `kit cdp` only when you
actually need the fleet overview.

### Override the instance

`--app <selector>` overrides worktree selection. It matches the internal attachment
name, app name, worktree label/path, instance id, or exact port:

```bash
kit cdp --app my-feature-worktree eval 'location.href'   # by worktree
kit cdp --app instance-8 ready                            # by instance id
kit cdp --app 9333 ready                                  # by debug port
```

- **Inside a Git worktree, omit `--app` for the normal case.** Kit considers only
  processes owned by that exact worktree; it never falls back to the main checkout.
- If that worktree has zero or multiple CDP endpoints, Kit errors with the scoped
  candidates rather than guessing. Outside Git, omission is allowed only when exactly
  one instance exists.
- Pass `--app` in scripts only when the script intentionally targets another worktree
  or a named launched session.
- **Avoid bare digits as a name** — `--app 8` is too loose (it can collide with a port
  or an id fragment). Use the worktree name, `instance-8`, or the full port.
- `--target <selector>` picks a target *within* the chosen instance (a specific window
  / webview). You rarely need it — every command defaults to the main workbench
  window, the same one `ready` resolves. Use `targets` to see the selectors.

`--app` selects *which app*; `--target` selects *which window inside that app*. Keep
them straight: a wrong `--app` reads a different Timeline entirely; a wrong `--target`
drives the wrong window of the right app.

## Three ways in

```bash
# 1. The current worktree's app is already running → just use a command (lazy attach):
kit cdp eval 'location.href'

# 2. You control the browser → launch isolated Chrome, attached before navigation:
kit cdp launch http://localhost:3000 --name checkout --headless

# 3. It's an Electron app → launch it and attach to the renderer it exposes:
kit cdp launch-electron --name app --cwd app-dir -- ./node_modules/.bin/electron .
```

Launched sessions are cleaned up with `kit cdp close <name>` (stops the browser);
`detach` only stops capture and leaves the app running. Never `detach --all` on a
shared machine — other attachments may belong to someone else's session.

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
kit cdp wait '!document.querySelector(".spinner")' --timeout 10s --app dev
kit cdp expect text 'Saved' --app dev                         # on-screen text, polled
kit cdp expect eval 'app.cart.items.length' --equals 3 --app dev
kit cdp expect net '/api/save' --status 2xx --since-mark last-action --app dev
kit cdp expect no-errors --since-mark last-action --app dev
kit cdp snap --diff --app dev       # what changed on screen since the last snap — no assertions needed
kit cdp screenshot --app dev        # what the window literally looks like — prints the image path
```

Per-instance state — the `last-action` mark, the `snap --diff` baseline, named
`mark`s, watches, and traces all live **on the instance you set them on**. Reuse the
same `--app` across a loop or the continuity breaks: `snap --diff --app a` diffs
against `a`'s last snap, never `b`'s.

`expect net` failures print near-misses (same URL, different status) — the
diagnosis, not just the verdict.

`screenshot` (alias `shot`) writes the active window's pixels to a timestamped
file in the artifact dir, or `-o <path>` (format from the extension: png/jpeg/webp,
`--quality` for the lossy ones). `--target` picks a window from `kit cdp targets`;
`--full` captures the whole scrollable page; `--raise` brings an occluded window
to front first (visible side effect, so never the default). It captures the
renderer surface over CDP — page pixels, not the OS window chrome.

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
kit cdp trace fn 'app.api.save' --app dev                       # every call: args → outcome, duration
kit cdp trace find 'groupId: opts.group.id' --app dev           # live url:line coordinates from parsed scripts
kit cdp trace add src/cart.js:84 'items.length' --app dev       # logpoint at a repo path (source maps)
kit cdp trace add renderer.js:108 '({ counter })' --when 'counter > 2' --app dev
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
kit cdp brief --since 30s --app dev   # agent-safe digest: errors grouped, noise counted, never silently lossy
kit cdp errors --since 5m --app dev   # error-shaped events deduped to `error (N×)`; --explain expands groups
kit cdp tail --since 3s --app dev     # raw rows, all tracks on one clock
kit cdp console / net / ws            # single-track slices; --grep, --source main|renderer, --limit
```

Each instance has its own Timeline — `errors --app a` never shows `b`'s errors. When
two instances misbehave, read them separately; there is no merged view.

Scope every read: `--since 5s|2m`, `--since-mark <name>`, or bound an action
yourself with `mark <name>` → act → `after <name>` (waits for idle, summarizes).
`do` and flow runs set a `do-start` mark automatically.

## Hand off evidence

```bash
kit cdp bundle checkout --since before-save --app dev
```

Writes `summary.md`, `timeline.json`, `errors.txt`, `network.har`,
`environment.json` — secrets redacted by default. This is the artifact to attach to
an issue or hand to another agent.

## Going deeper

- [references/commands.md](references/commands.md) — the full command surface:
  launcher flags, trace/instrumentation semantics (rate caps, Debugger side
  effects, disclosed limits), network rules (block/mock/throttle), profiles,
  heap, lenses, extension diagnosis, marks.
- [references/recipes.md](references/recipes.md) — worked end-to-end sequences:
  verifying a feature, diagnosing a reload crash, tracing instead of
  console.log, mapping minified stacks to the repo, form workflows,
  watch-driven debugging, flow authoring.
