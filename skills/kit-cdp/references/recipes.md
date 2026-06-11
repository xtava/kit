# kit cdp — worked recipes

Each recipe is a complete sequence you can adapt. `--app` is shown explicitly;
drop it when only one instance exists.

## Verify a feature you just built

You changed save behavior in a web app served at localhost:3000. Prove it works:

```bash
kit cdp launch http://localhost:3000 --name dev --headless
kit cdp snap --app dev                       # find the elements; note @refs
kit cdp do "fill 'textbox:Name' Grace; click 'button:Save settings'; expect text 'Saved'; expect net '/api/save' --status 2xx; verify" --app dev
kit cdp snap --diff --app dev                # the UI delta, no assertions needed
kit cdp close dev
```

If the `do` fails, the output already contains the failing step's evidence
(events, errors, network since the step began). Pull more with
`kit cdp tail --since-mark do-start --app dev`.

## Diagnose "it breaks when I reload"

Capture starts at attach — CDP cannot replay the past. Warm first, reproduce second:

```bash
kit cdp attach --app dev
# …save the file / hit reload / trigger HMR…
kit cdp brief --since 30s --app dev          # the digest: what broke, what repeated
kit cdp errors --since 30s --explain --app dev
kit cdp tail --track exception --since 30s --app dev
```

`brief` prints exact omission counts — if it says rows were grouped or evicted,
fall back to `tail` with the same filters before concluding anything.

## Drive a form like a user

```bash
kit cdp snap --app dev
kit cdp fill @e12 'grace@example.com' --app dev
kit cdp press Tab --app dev
kit cdp fill 'textbox:Password' 'hunter2' --app dev
kit cdp press Enter --app dev
kit cdp verify --app dev
```

Refs (`@e12`) are valid until the document changes; after a navigation or
re-render, snap again or switch to `role:name` locators, which resolve fresh on
every run.

## Watch state while you interact

"Does the cart count actually update when the API responds?"

```bash
kit cdp watch add cart 'document.querySelectorAll(".cart-item").length' --app dev
kit cdp click 'button:Add to cart' --app dev
kit cdp tail --since 5s --app dev
```

The tail interleaves the click, the network response, and the `watch cart 2 → 3`
row on one clock — causality is readable directly.

## "Did my code even run?" — trace instead of console.log

You changed `app.api.save` and want proof the new path executes, with what data,
and what it triggers — without touching the code:

```bash
kit cdp trace fn 'app.api.save' --app dev
kit cdp click 'button:Save settings' --app dev
kit cdp tail --track trace --since-mark trace-save --app dev
```

```
+1241ms [app] trace save ({name: "Grace"}) → {ok: true} 38ms
+1290ms [app] net ← 200 POST /api/save
```

`trace fn` needs a `globalThis`-reachable path. For module-scoped functions and
specific lines (loop bodies, branches), use a logpoint — and **verify the arm
against its readback** before reproducing:

```bash
kit cdp trace add src/cart/store.ts:84 '({n: items.length, t: action.type})' --app dev
# logpoint 'store.ts-84' at src/cart/store.ts:84 → bundle.js:9214:18 (1 site), rate cap 20/s
# line      const next = reduce(items, action);
```

The `line` echo is the original source at that line — if it reads `return;` or a
comment, the coordinate is wrong; fix it now instead of staring at silence later.
Don't grep build output for line numbers (they drift on every rebuild); search the
code that is actually executing:

```bash
kit cdp trace find 'reduce(items, action)' --app dev   # → url:line + the line's text, always current
kit cdp trace ls --app dev                             # armed/awaiting · hits + last-hit age · stalled reasons
```

When you're done: `kit cdp trace clear`. Traces are observation only — a caught
throw shows as `✗` but never fails `verify`.

## Map a minified stack back to the repo

```bash
kit cdp errors --since 5m --resolve --app dev
```

```
exception TypeError: Cannot read properties of undefined → src/cart.js:14
```

The original location lands on the headline; the raw bundle stack stays below it.
Resolution is automatic (and free) on every view once a map is loaded.

## Author a flow worth committing

When a verification sequence works, save it so every future session (human or
agent) inherits it. `.kit/cdp/flows/save-smoke.flow`:

```
# Save settings round-trip: UI ack + API + clean verdict
fill 'textbox:Name' ${user}
click 'button:Save settings'
expect text 'Saved'
expect net '/api/save' --status 2xx
verify
```

```bash
kit cdp flow run save-smoke user=Grace --app dev
```

Steps are the exact CLI grammar — develop them interactively as single commands,
then paste the lines into the flow file. Commit the file.

## Bound an action and hand off evidence

```bash
kit cdp mark before-save --app checkout
kit cdp click @e5 --app checkout
kit cdp after before-save --app checkout       # waits for idle, summarizes the window
kit cdp bundle checkout --since before-save    # redacted folder: summary, timeline, errors, HAR
```

The bundle directory is the handoff artifact — attach it to an issue or pass the
path to another agent.

## Electron: split main from renderer

The main process needs `--inspect` to be visible; the renderer needs a CDP port
(`launch-electron` arranges this).

```bash
kit cdp launch-electron --name app --cwd app-dir --cdp-env APP_CDP_PORT -- ./node_modules/.bin/electron .
kit cdp tail --source main --app app           # Node-side only
kit cdp console --source renderer --app app    # page-side only
```

## Diagnose a Modular extension view

```bash
kit cdp ready --app modular-dev --json
kit cdp ext doctor modular.local-sdk-view-showcase --app modular-dev
# …reproduce the issue…
kit cdp ext bundle modular.local-sdk-view-showcase --since 60s --app modular-dev --json
```

`ext doctor` reads the workbench test bridge plus live webview probes: view
health, document load, bridge status, HMR state, blockers. `ext bundle` adds the
Timeline slice filtered to that extension — capture it on a pre-warmed attachment.

## Quiet a noisy timeline without lying to yourself

```bash
kit cdp ignore 'ResizeObserver loop'
kit cdp brief --since 2m
```

Ignored rows still count in the omission banners, so a suppressed-but-relevant
error can't vanish silently. `ignore --list` / `--clear` to audit.

## Script hygiene

- Assertions exit non-zero: `kit cdp click 'button:Save' && kit cdp verify` is a
  complete gated check.
- Everything takes `--json` for parsing; prefer it over scraping text.
- One instance per concern: name launches (`--name checkout`) and pass `--app`
  explicitly in anything saved or shared.
- Clean up what you launched (`close <name>`), detach only your own attachments,
  and never `detach --all` on a machine you share.
