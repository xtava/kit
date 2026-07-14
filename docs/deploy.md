# kit deploy — design and configuration

`kit deploy` is an interactive launcher for operator-defined deployment plans. Kit owns selection,
process lifecycle, streamed output, timing, and presentation; the configuration owns every Target,
Step, command, path, and environment-specific value.

## Design

The data model is deliberately small and typed:

- A **Deployment plan** is one versioned TOML document.
- A **Target** is one operator-selectable deployment outcome with a stable `id`, display `name`,
  optional `description`, `working_dir`, `env_file`, and Backend, plus an ordered non-empty list of
  Steps.
- A **Step** is one named unit of work with an optional `working_dir` and exactly one Action.
- An **Action** is either a direct `command` (`program` plus an argument vector) or an explicit
  `shell` script. Direct commands avoid shell parsing; shell Actions exist for pipelines and other
  intentionally shell-shaped work.
- A **Run** executes selected Targets and their Steps sequentially in configuration order. It stops
  at the first failed or cancelled Step and records every completed Step's duration.
- A **Version** is owned by the Target's history Backend. Local Targets use the working tree's Git
  commit when available, otherwise a monotonic Target-local `run-N` identifier. Cloudflare Pages
  Targets use Cloudflare deployment IDs and metadata directly.
- A **Rollback strategy** is an optional typed Target capability. `steps` runs a dedicated ordered
  Step set; `redeploy` runs the Target's deployment Steps again. Both receive the selected Version
  as `KIT_DEPLOY_VERSION` and `KIT_DEPLOY_REF`, and replace `{{version}}` / `{{ref}}` in command
  arguments and shell scripts.
- A **Journal** is typed persisted Run history for local Targets, grouped by Target. Each entry records Version,
  timestamp, status, total duration, and the Steps that ran with their outcomes and durations.
- A **Summary** is the terminal Run state: succeeded, failed, cancelled, or rolled back, with Target
  and Step timings.

The TUI has five phases. **Browse** is a two-pane Target picker: navigation moves through Targets,
Space toggles selection, and the detail pane previews the focused Target's Steps. Enter opens
**Review**, which makes the exact ordered Run plan explicit. `v` opens **Versions** for the focused
Target. Local history comes from the Journal; Cloudflare Pages history loads asynchronously from
the platform with explicit loading, empty, and error states. Choosing an eligible entry opens a
rollback Review when that Target has a rollback owner. A
second Enter starts **Run**, where
Target and Step status, elapsed time, and a bounded live output tail update continuously. Completion
opens **Summary** with the final outcome and timing breakdown. Escape moves back before execution;
`Ctrl-C` during execution cancels the active process group and stops the plan cleanly.

## Resizable panel layout

Browse, Versions, and Run each have a draggable vertical divider. Hold the left mouse button on the
divider and drag it to resize the two panels; release commits the preference. Press Escape during a
drag to restore the size from before that drag. Press `=` to reset the current view's split.

The three views retain independent sizes in Kit's XDG state location:
`$XDG_STATE_HOME/kit/deploy-layout.json` (normally
`~/.local/state/kit/deploy-layout.json`). If the platform has no state directory, Kit uses its XDG
local-data directory. The versioned JSON file contains only three normalized split ratios. It never
contains terminal dimensions, rendered coordinates, config paths, Target data, commands, output, or
secrets.

Kit clamps the rendered panels to useful minimum widths when the terminal is narrow without changing
the saved preference, so widening the terminal restores the chosen balance. A missing state file uses
the built-in defaults. Invalid, unreadable, or newer-schema state produces a visible warning and uses
defaults without overwriting the file; press `=` in a resizable view to replace it deliberately. A
failed save leaves the current in-memory layout active and is retried once when the TUI closes.

## Configuration resolution

Kit checks these locations in order:

1. `kit deploy --config <path>`
2. `.kit/deploy.toml` in the current working directory
3. Kit's XDG configuration file (`$XDG_CONFIG_HOME/kit/deploy.toml`, normally
   `~/.config/kit/deploy.toml`)

Relative `working_dir` values are resolved from the directory containing the selected configuration
file. Relative `env_file` values use the same base directory. Kit does not ship deployment defaults
and never guesses a command. If no configuration is
found, the error lists the searched paths and points to the example. Invalid TOML and invalid plans
fail before the terminal enters interactive mode.

Unknown fields are rejected. Target IDs must be non-empty and unique; every Target and Step must
have a non-empty name; every Target must contain at least one Step; command programs and shell
scripts must be non-empty. Version `1` is the only supported schema version.

## Cloudflare Pages backend

A Target can delegate Version history and rollback to Cloudflare Pages while keeping its configured
deployment Steps unchanged:

```toml
[[targets]]
id = "pages"
name = "Cloudflare Pages"
env_file = "<path-to-env-file>"

[targets.backend]
type = "cloudflare-pages"
account_id = "<account-id>"
project = "<pages-project>"
token_env = "CLOUDFLARE_API_TOKEN"

[[targets.steps]]
name = "Publish site"
action = { type = "command", program = "<your-pages-deploy-command>", args = ["<output-directory>", "--project-name=<pages-project>"] }
```

`account_id`, `project`, and `token_env` must be non-empty. `token_env` names the variable containing
the API token; the token value never belongs in deploy TOML. Kit checks the real process environment
first, then this Target's `env_file`. If neither contains a non-empty value, the Versions view shows
an actionable error and supports `r` to retry. The token needs Cloudflare Pages read access to browse
deployments and write access to roll back.

For this Backend, `v` calls Cloudflare's deployments API and shows newest-first deployment ID,
commit SHA when present, creation time, production/preview environment, latest-stage status, and
URL. Rollback calls Cloudflare's rollback API and appears in the normal Run and Summary views.
Cloudflare permits rollback only to successful production deployments, which Kit enforces before
Review. Deploy itself still runs the configured Steps; Kit does not upload Pages artifacts.

Do not add `[targets.rollback]` to a Cloudflare Pages Target. Platform-backed and local rollback are
mutually exclusive so there is one authoritative owner. Cloudflare Pages Runs are not copied into
Kit's local Journal; reopening Versions reads the platform's current history.

## Target environment file

Set `env_file` on any Target to load deployment configuration and secrets without exporting them
before every Run:

```toml
[[targets]]
id = "pages"
name = "Cloudflare Pages"
env_file = "<path-to-env-file>"
```

Relative paths resolve from the directory containing `deploy.toml`; absolute paths are used as-is.
Kit loads and validates the file before entering the TUI. A missing file, malformed line, invalid
key, or unmatched value quote names the Target, path, and line where applicable.

The supported dotenv form is deliberately explicit: blank lines and lines beginning with `#` are
ignored; other lines are `KEY=VALUE`; surrounding whitespace is trimmed; and matching single or
double quotes around a value are removed. Values may contain additional `=` characters. Keys use
letters, numbers, and `_`, and must begin with a letter or `_`.

Every Step for the Target receives the loaded values, including deploy and local rollback Steps.
Backend credential names such as `token_env` resolve through the same environment. An explicitly
exported process value always overrides the file value. Keep the dotenv file out of source control;
Kit never writes its values to the Journal, output view, or debug formatting.

## Deploy journal

The journal lives outside the project tree at Kit's XDG state location:
`$XDG_STATE_HOME/kit/deploy-journal.json` (normally
`~/.local/state/kit/deploy-journal.json`). If the platform has no state directory, Kit uses its XDG
local-data directory. This keeps runtime history out of source control without requiring project
`.gitignore` entries.

The file is a versioned, typed JSON document for Targets without a Backend. Entries are grouped by
Target ID and contain no
machine identity, username, hostname, command body, path, or captured output. They contain only the
configured Target/Step names, Version, Unix timestamp, status (`success`, `failed`, `cancelled`, or
`rolled_back`), elapsed milliseconds, and Step results. Writes create the state directory and
replace the journal atomically. An unreadable, invalid, or unwritable journal is an actionable
error; Kit never silently discards history.

## Schema

```toml
version = 1

[[targets]]
id = "staging"
name = "Staging"
description = "Build and publish the staging service"
working_dir = "../service"

[[targets.steps]]
name = "Build release"
action = { type = "command", program = "cargo", args = ["build", "--release"] }

[[targets.steps]]
name = "Publish artifact"
working_dir = "scripts"
action = { type = "shell", script = "./publish.sh <your-host>" }

[targets.rollback]
type = "steps"

[[targets.rollback.steps]]
name = "Restore selected release"
action = { type = "command", program = "./restore", args = ["{{version}}"] }
```

`working_dir` on a Step overrides its Target's directory. Child processes inherit Kit's environment,
then receive missing values from the Target's `env_file`, plus `KIT_DEPLOY_VERSION` and
`KIT_DEPLOY_REF`. Standard input is terminal-independent; stdout and stderr are captured and streamed
into the Run view. Put secret values in the dotenv file or an external secret manager, never in the
deployment plan.

Rollback supports two typed shapes:

```toml
# Dedicated rollback Steps.
[targets.rollback]
type = "steps"
steps = [
  { name = "Restore", action = { type = "command", program = "./restore", args = ["{{ref}}"] } },
]

# Or rerun this Target's deployment Steps pinned to the selected Version.
[targets.rollback]
type = "redeploy"
```

`redeploy` is rejected unless at least one deployment Action references `{{version}}` or `{{ref}}`;
this prevents a rollback selection from silently running an unpinned deployment. Dedicated rollback
Steps may instead consume `KIT_DEPLOY_VERSION` / `KIT_DEPLOY_REF`. A Target with no `rollback`
configuration shows history but cannot start rollback; the TUI points to this section.

## Run

Copy [`examples/deploy.toml`](../examples/deploy.toml), replace every placeholder, then run:

```bash
mkdir -p .kit
cp examples/deploy.toml .kit/deploy.toml
kit deploy
```

Use `kit deploy --config path/to/deploy.toml` to select a different plan explicitly.
