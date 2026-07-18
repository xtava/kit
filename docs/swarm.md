# Codex Swarm Orchestrator

`kit swarm` runs a fixed, host-enforced Codex council and lets interactive and headless viewers
observe the same durable run independently.

## Runtime model

Each run has one detached owner process. That owner executes:

```text
planner -> isolated parallel experts -> optional same-thread rebuttals -> devil -> synthesis
```

The graph, stage barriers, thread isolation, schemas, retry limit, event ordering, and concurrency
limit are deterministic. Model-generated text is not deterministic.

The immutable `spec.json` and append-only `events.jsonl` Journal are the only authorities. The
owner writes them; viewers only replay events and publish explicit cancellation requests. Closing a
TUI or events client does not stop a run. A verified owner that disappears without a terminal event
is shown as `orphaned` and is never guessed back into motion.

## Commands

```bash
# Open the independent tree/detail viewer
kit swarm

# Start and wait; omit the prompt argument to read all of stdin
kit swarm run "Question to analyze"
printf '%s' "Question to analyze" | kit swarm run

# Start independently and print the run ID after the owner handshake
kit swarm run --detach --reasoning high "Question to analyze"

kit swarm list
kit swarm show swarm-1
kit swarm events swarm-1
kit swarm events swarm-1 --follow
kit swarm wait swarm-1
kit swarm cancel swarm-1
kit swarm delete swarm-1

# Machine-readable list, show, run, wait, and cancel projections
kit --json swarm list
kit --json swarm show swarm-1
```

`events` always emits canonical JSONL without terminal decoration. `run` and `wait` return success
only for a succeeded run; failed, cancelled, and orphaned terminal states are printed and return a
non-zero exit. `cancel` succeeds only after the owner acknowledges cancellation and publishes the
cancelled terminal state. `delete` refuses a live run.

Run options select the Codex model, reasoning effort (`low`, `medium`, `high`, or `xhigh`), working
directory, retry limit, and whether Debate is disabled. If no model is supplied, the user's Codex
configuration chooses it.

## TUI

The left region is a collapsible Run → Stage → Agent tree. The right region shows the selected
prompt or agent status, thread IDs, accumulated token usage, errors, chronological stream items,
and final synthesis. Completed unselected runs use a terminal-record-validated summary; details are
replayed lazily. Active runs use incremental Journal tails.

| Key | Action |
| --- | --- |
| `Up` / `Down` | Traverse visible tree nodes |
| `Left` / `Right` | Collapse or expand; move to parent or first child |
| `Tab` | Switch tree/detail region; toggle the visible region on narrow terminals |
| `PageUp` / `PageDown` | Scroll detail |
| `n` | Create a run using the current directory and standard defaults |
| `c` | Confirm cancellation of the selected run |
| `d` | Confirm deletion of the selected terminal/orphaned run |
| `q` | Exit this viewer only |

Filesystem notifications are refresh hints, not state. Periodic reconciliation against canonical
files ensures missed or coalesced notifications do not make the view authoritative.

## Persistence and privacy

State is stored under Kit's platform XDG state directory, in `swarm/runs/swarm-N/`. On Linux this is
normally `~/.local/state/kit/swarm/runs/`. Directories and files are owner-only.

Durable independent viewing requires storing the full prompt, generated stage prompts, normalized
Codex item streams, thread IDs, usage, and final output. Treat the state directory as sensitive.
Kit does not copy Codex authentication, tokens, authorization headers, or environment variables
into the store. `kit swarm delete ID` explicitly removes a non-live run; there is no automatic
retention policy yet.

Codex children use the existing user authentication and run read-only with approvals, web search,
and network access disabled. They cannot edit the working directory through this module.
