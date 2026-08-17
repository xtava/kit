#!/usr/bin/env bash

set -euo pipefail

usage() {
  printf 'Usage: %s <sessions-root> <task-slug> <objective> <observable-finish-line>\n' "$0" >&2
}

if [[ $# -ne 4 ]]; then
  usage
  exit 2
fi

sessions_root=$1
task_slug=$2
objective=$3
finish_line=$4

if [[ ! $task_slug =~ ^[a-z0-9]+(-[a-z0-9]+)*$ ]]; then
  printf 'Task slug must contain lowercase letters, numbers, and single hyphens only.\n' >&2
  exit 2
fi

if [[ -z $objective || -z $finish_line ]]; then
  printf 'Objective and observable finish line must not be empty.\n' >&2
  exit 2
fi

date_stamp=$(date +%Y%m%d)
sequence=1

while true; do
  printf -v sequence_stamp '%02d' "$sequence"
  ledger_root="$sessions_root/$task_slug-$date_stamp-$sequence_stamp"
  [[ ! -e $ledger_root ]] && break
  ((sequence += 1))
done

mkdir -p "$ledger_root/tasks" "$ledger_root/adr"

cat > "$ledger_root/objective.md" <<EOF
# Objective

$objective

# Observable finish line

$finish_line
EOF

cat > "$ledger_root/index.md" <<'EOF'
# Session Overview

## Status

- Summary: Scaffold created. Context and development loop are not set yet.
- Phase: `grounding`
- Blockers: none recorded

## Objective

See [objective.md](./objective.md).

## Scope

- In scope: TODO
- Protected boundaries: TODO

## Context

See [context.md](./context.md).

## Next action

Complete the ground tree and identify the development loop.

## Structure

```text
.
├── index.md      State, scope, routing, and next action
├── objective.md  Objective and observable finish line
├── dev-loop.md   Repeatable edit, inspect, and proof workflow
├── tasks/          Executable task packets
├── context.md      Current system and owner tree
└── adr/            Settled architecture decisions
```

## Recovery

Read this file first. Then read `objective.md`, `context.md`, and `dev-loop.md`.
EOF

cat > "$ledger_root/dev-loop.md" <<'EOF'
# Development Loop

## Status

`identifying`

## Target

- Product or service: TODO
- Runtime or app variant: TODO
- Workspace: TODO

## Starting state

TODO: record the deterministic seed, fixture, or setup.

## Loop

1. TODO: edit or build.
2. TODO: reload or restart.
3. TODO: perform the product action.
4. TODO: inspect the visible or durable result.

## Narrow check

- Command or action: TODO
- Expected evidence: TODO

## Limits and recovery

- Known limit or failure signature: TODO
- Recovery command: TODO
EOF

cat > "$ledger_root/context.md" <<'EOF'
# Context

```text
TODO: draw the current product and owner tree with real names.
```

Add an `Open questions` branch only when an unresolved fact can change the
design.
EOF

printf '%s\n' "$ledger_root"
