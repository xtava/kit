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
disposes itself (idle timeout, bounded reconnect, signals) so it can never stray.

## The agent contract

```
kit cdp                       # orient: instances available + live attachments
kit cdp eval 'location.href'  # just works — lazy-attaches, stays warm
kit cdp tail --since 3s       # all tracks on one clock — "what just happened"
kit cdp ls                    # health + the kill switch
```

No attach ritual, no cached handles, no efficiency rules to remember. Add `--json`
to any command for structured output; pipe text output to `grep`/`head` freely.

## Commands

| Group | Command | What |
|---|---|---|
| Lifecycle | `attach [--track …]` · `detach [--all]` · `ls` · `gc` | manage Attachments (attach is optional) |
| Observe | `tail [--since 5s] [--track …]` · `console` · `net` · `ws` | slice the Timeline |
| Probe | `snap [-i]` · `eval <expr>/--file` · `heap` · `targets` | live one-shot query |
| Interact | `click @ref` · `fill @ref <text>` | drive a Target (refs come from `snap`) |
| Lens | `lens <name> [-- args]` | run a scriptable lens in a Target |

Selectors: `--app <instance>` picks the Attachment (app name / worktree / instance
id / port); `--target <selector>` picks the Target (defaults to the main app
window). `--since` takes `500ms` / `2s` / `5m`.

## Tracks

Captured by default from attach: `console`, `exception`, `log`, `network`, `ws`.
Heap is **not** a track — `kit cdp heap` probes on demand (continuous heap sampling
would perturb the very memory it measures). Restrict any Timeline slice with
`--track net,ws`.

## Lenses

Generic core, scriptable edge: the engine ships **no** app knowledge. A lens is a
JS file in `~/.config/kit/cdp/lenses/<name>.js`, run inside a Target, that decodes
app-specific state the engine deliberately doesn't know — a custom editor's document
model, a sync engine's state, the app's routes. The script body receives `args` and
`return`s a JSON value:

```js
// ~/.config/kit/cdp/lenses/title.js   →   kit cdp lens title
return { title: document.title, url: location.href };
```

Starters ship under that dir: `styles`, `overflow`.

## How it stays out of your way

- **Reloads** don't drop the Attachment — it re-binds the recreated Target and
  records the navigation on the Timeline (`docs/adr/0002`).
- **Restarts** trigger bounded, capped-backoff re-discovery by selector, not an
  infinite `/proc` spin.
- **Disposal** is guaranteed: idle timeout (~30 min), reconnect give-up, SIGTERM,
  and registry reconciliation — `kit cdp ls` always shows what's live, `detach`
  /`gc` kill it (`docs/adr/0003`).
