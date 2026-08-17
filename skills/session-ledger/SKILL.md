---
name: session-ledger
description: Build and maintain durable local context for a long engineering effort.
disable-model-invocation: true
---

# Session Ledger

A session ledger lets another agent resume work without reading the chat.

## Start

```text
sessions/<task-slug>-<YYYYMMDD>-<NN>/
├── index.md
├── objective.md
├── dev-loop.md
├── tasks/
├── context.md
└── adr/
```

Generate the scaffold:

```sh
~/.agents/skills/session-ledger/scripts/create-session-ledger.sh \
  sessions \
  <task-slug> \
  '<objective>' \
  '<observable-finish-line>'
```

The script selects the next free sequence number. Reuse an existing ledger when
one already owns the work.

## Owners

| Path | Owns |
| --- | --- |
| `index.md` | High-level session status, phase, blockers, next action, and links |
| `objective.md` | One objective and one observable finish line |
| `dev-loop.md` | The repeated edit, run, inspect, and recovery path |
| `context.md` | The current system and owner tree |
| `tasks/` | Executable work packets |
| `adr/` | Settled architecture decisions |

One fact has one owner. Other files link to it instead of copying it.

## `index.md`

Keep the index short. It is the session dashboard, not the work log.

Track only:

- one-line status;
- current phase;
- blockers or `none`;
- high-level scope;
- exact next action;
- links to current tasks and decisions;
- recovery read order.

Use these phases:

```text
grounding -> designing -> ready -> executing -> verifying -> complete
```

## `objective.md`

Keep this file isolated. It contains only:

1. the objective;
2. the observable finish line.

Do not add status, context, tasks, rationale, or evidence.

## `context.md`

Draw one tree with the real product path, owners, boundaries, and consumers.
Put each fact on its node.

Do not add `verified`, `inference`, or `open` prefixes. Do not repeat the tree as
a bullet list. Add an `Open questions` branch only when an answer can change the
design.

## `dev-loop.md`

Record the shortest repeatable path for this session:

- target and starting state;
- edit, build, reload, or restart commands;
- product action and inspection surface;
- narrow check and expected result;
- known failure and recovery command.

Replace stale commands. Keep commands copy-pasteable.

## `tasks/`

Create one Markdown file per executable unit. Each task owns:

- status and exact outcome;
- files or systems it can change;
- protected boundaries;
- checklist;
- check and expected result;
- next action.

## `adr/`

Create one Architecture Decision Record (ADR) per settled decision. Record the
context, decision, consequences, and any decision it replaces. The index links
to the current ADR.

## Update loop

1. Read `index.md`.
2. Read the linked owner for the current phase.
3. Do the next action.
4. Update that owner.
5. Refresh the index status, blockers, and next action.
6. Delete superseded paths and stale text.

Add other files only when they own a distinct body of work that later turns must
read independently.

## Recovery

A fresh agent reads:

```text
index.md
├── objective.md
├── context.md
├── dev-loop.md
├── current tasks
└── current ADRs
```

The ledger is healthy when that read order gives the current status and exact
next action without chat history.
