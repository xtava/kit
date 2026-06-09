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
| Triage | `errors [--explain]` | what's broken — error-shaped events deduped to `error (N×)`, with a `⚠` banner when the view is lossy |
| Probe | `snap [-i]` · `eval <expr>/--file` · `heap` · `targets` | live one-shot query |
| Interact | `click @ref` · `fill @ref <text>` | drive a Target (refs come from `snap`) |
| Lens | `lens <name> [-- args]` | run a scriptable lens in a Target |
| Extensions | `ext doctor <id>` · `ext bundle <id>` | diagnose a Modular extension runtime view |

Selectors: `--app <instance>` picks the Attachment (app name / worktree / instance
id / port); `--target <selector>` picks the Target (defaults to the main app
window). Timeline slices accept `--source main|renderer`, `--target <selector>`,
`--grep <text>`, `--extension <id>`, and `--limit <n>`. `--since` takes `500ms`
/ `2s` / `5m`.

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

Keys: `⏎` run · `⇥` complete the command word · `↑↓` history (persisted under the
config dir) · `PgUp`/`PgDn` scroll the feed · `End`/`Esc` re-pin to live · `^L`
clear · `^D` quit. The feed header shows `● live` when pinned and `▲ N below` when
you've scrolled up. The live pane survives HMR reloads and app restarts on the same
subscription (`docs/adr/0004`).

## How it stays out of your way

- **Reloads** don't drop the Attachment — it re-binds the recreated Target and
  records the navigation on the Timeline (`docs/adr/0002`).
- **Restarts** trigger bounded, capped-backoff re-discovery by selector, not an
  infinite `/proc` spin.
- **Disposal** is guaranteed: idle timeout (~30 min), reconnect give-up, SIGTERM,
  and registry reconciliation — `kit cdp ls` always shows what's live, `detach`
  /`gc` kill it (`docs/adr/0003`).
