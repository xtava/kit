---
name: kit-dev-log
description: >-
  Create, update, and resume durable Kit engineering session ledgers. Use when
  the user asks to write down development work, preserve investigation context,
  make a handoff, remember a confirmed fix, resume prior Kit work, or pick up
  where another agent stopped.
license: MIT
metadata:
  source: https://github.com/xtava/kit
---

# Kit development logs

Maintain structured current truth under `sessions/` so another agent can resume without reading
the chat.

## Ground first

1. Read `CONTRIBUTING.md` and `docs/canonical/dev-logs.md`.
2. Search `sessions/*/index.md` and `sessions/*/objective.md` for the task's subsystem, symptom, and
   objective.
3. Reuse the matching ledger. Never create a second ledger for the same coherent effort.
4. If no ledger owns the work, create one with:

   ```bash
   skills/kit-dev-log/scripts/create-session-ledger.sh \
     sessions \
     <task-slug> \
     '<objective>' \
     '<observable-finish-line>'
   ```

## Owners

- `index.md` owns summary, phase, blockers, scope, links, and the exact next action.
- `objective.md` owns only the objective and observable finish line.
- `context.md` owns one current product and owner tree.
- `dev-loop.md` owns the shortest copy-pasteable edit, run, inspect, and recovery path.
- `tasks/` owns executable work packets, protected boundaries, checks, outcomes, and next actions.
- `adr/` owns settled decisions and the decisions they replace.

One fact has one owner. Link to it elsewhere instead of duplicating it.

## Update loop

1. Read the ledger in recovery order: index, objective, context, dev loop, linked tasks, linked ADRs.
2. Perform the exact next action.
3. Update the file that owns the resulting fact.
4. Refresh the index summary, phase, blockers, links, and exact next action.
5. Delete stale or superseded text. Do not append a chat-style timeline.
6. Before handoff, read the recovery order again and confirm it is sufficient without the chat.

Use only these phases:

```text
grounding -> designing -> ready -> executing -> verifying -> complete
```

## Promotion

- Promote settled subsystem behavior to the applicable file under `docs/canonical/`.
- Promote a confirmed, misleading hard failure to `docs/canonical/pitfalls/` using the existing
  pitfall entry and index format.
- Keep incomplete investigation, current blockers, and the next action in the ledger.
- Link promoted documents from the ledger; do not retain a competing copy.

## Safety

Never record secrets, access tokens, authentication URLs, SSH keys, cookies, environment values,
private terminal contents, or full chat transcripts. Record stable owners and sanitized evidence.
Preserve unrelated worktree changes.
