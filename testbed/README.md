# kit testbed

A tiny Electron playground that emits errors, network traffic, websocket frames, and state
changes **on demand**, so `kit cdp` features can be verified against a real live app instead
of mocks. The main process embeds an HTTP API (ok / 500 / 404 / slow / flaky / save) and a
minimal WebSocket echo; the page exposes `window.testbed` for `eval`/`watch` probing.

## Run it

```bash
cd testbed && npm install --save-dev electron   # once
kit cdp launch-electron --name testbed --cwd testbed \
  --cdp-env TESTBED_CDP_PORT --renderer-target testbed -- npx electron .
```

Set `TESTBED_BOOT_ERROR=1` (via `--env TESTBED_BOOT_ERROR=1`) to throw during boot — the
startup-capture scenario.

## What each section exercises

| Section | kit cdp surface |
|---|---|
| Console buttons | console/exception tracks, `errors`, dedup groups, `burst logs` → idle detection |
| Network buttons | network track, `net failed`, `net slow`, block/mock rules |
| WebSocket | ws track |
| Async save | settle windows, spinner→toast (`role=alert`) for `wait`/`expect text` |
| Trace | `trace fn` on `testbed.fns.*`: sync (`compute`), async (`saveQuote`), caught throw (`failing`), hot loop (rate-cap suppression) |
| State | `watch` deltas: counter, ticker (1s auto-increment), cart items; `renderer.js:<line>` logpoint targets |
| Form | `fill`, `select`, checkbox, validation error path |
| Navigation | ref invalidation, role:name re-resolution across pages |

## The source-map fixture

`bundle.js` + `bundle.js.map` are a hand-maintained "build" of `src/cart.js`: the
bundle prepends a 3-line banner and the map encodes exactly that line shift, so
`kit cdp trace add src/cart.js:5` must arm at `bundle.js:8`, and
`window.testbed.cart.brokenTotal()` throws an exception whose stack resolves back
to `src/cart.js:14` via `errors --resolve`. If you edit `src/cart.js`, re-paste it
under the banner and keep the map's `AACA` run equal to the source's line count.
