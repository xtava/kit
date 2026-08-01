# kit deploy — design and configuration

`kit deploy` runs operator-defined deployment plans through either the interactive TUI or an exact,
single-Target headless command. Kit owns selection, process lifecycle, streamed output, timing, and
presentation; the configuration owns every Target, Step, command, path, and environment-specific
value.

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
- A **Run** executes selected Targets and their Steps sequentially in configuration order. Its
  operation remains explicitly production deploy, preview deploy, or rollback through Review, Run,
  and Summary; production and preview never collapse into an ambiguous generic deploy. It stops at
  the first failed or cancelled Step and records every completed Step's duration. Every Step is a
  child of a masking-on `op run`; the full-screen Kit TUI itself remains attached to the real
  terminal, while the headless production path needs no terminal.
- A **Version** is owned by the Target's history Backend. Local Targets use the working tree's Git
  commit when available, otherwise a monotonic Target-local `run-N` identifier. Cloudflare Pages
  Targets use Cloudflare deployment IDs and metadata directly.
- A **Preview deploy** publishes a Cloudflare Pages Target to a branch alias instead of production.
  It substitutes `{{branch}}` in command arguments and shell scripts, and sets `KIT_DEPLOY_BRANCH`,
  with the branch you enter. A production deploy fetches and substitutes the Cloudflare project's
  current production branch.
- An **Annotation** is a local, per-deployment `error` flag and free-text `note`, kept outside the
  platform because Cloudflare Pages has no annotation surface of its own.
- A **Rollback strategy** is an optional typed Target capability. `steps` runs a dedicated ordered
  Step set; `redeploy` runs the Target's deployment Steps again. Both receive the selected Version
  as `KIT_DEPLOY_VERSION` and `KIT_DEPLOY_REF`, and replace `{{version}}` / `{{ref}}` in command
  arguments and shell scripts.
- A **Journal** is typed persisted Run history for local Targets, grouped by Target. Each entry records Version,
  timestamp, status, total duration, and the Steps that ran with their outcomes and durations.
- A **Summary** is the terminal Run state: succeeded, failed, cancelled, or rolled back, with Target
  and Step timings.

The TUI has five phases. **Browse** is a two-pane Target picker: navigation moves through Targets,
Space toggles selection, and the detail pane previews the focused Target's Steps. Enter opens an
explicit **Production deployment** Review for the selected Targets; a second Enter confirms the
production Run. `p` starts a **Preview deployment** of the focused Cloudflare Pages Target: a modal
prompts for a branch (prefilled with the current Git branch), then Review and Run remain labeled with
that preview branch alias. `v` opens **Versions** for the
focused Target. Local history comes from the Journal; Cloudflare Pages history loads asynchronously
from the platform with explicit loading, empty, and error states. In Cloudflare Versions the live
production deployment is marked `● LIVE`; `o` opens the selected deployment in the browser; `e`
toggles a local `⚠ ERROR` mark on the selected deployment, `n` attaches a note, and `d` deletes the
deployment after a confirmation (the live deployment cannot be deleted). Choosing an eligible entry
opens a rollback Review when that Target has a rollback owner. A second Enter starts asynchronous
Run preparation; reference resolution and Backend requests never block the TUI event loop. **Run**
then shows Target and Step status, elapsed time, and a bounded live output tail. Completion
opens **Summary** with the final outcome, destination environment, and timing breakdown. When the
Run published a Cloudflare Pages deployment, Summary shows the immutable deployment URL and `o`
opens it; a production deployment also updates the project's canonical production domains. Summary
stays open: `Enter` or `Esc` returns to Browse to keep working, and `q` quits. `Ctrl-C` during
execution cancels the active process group and stops the plan cleanly.

## Headless production deploy

Run one exact Target without opening the TUI:

```shell
kit --json deploy --config ~/.config/kit/deploy.toml \
  run --target server --confirm-production
```

`run` accepts exactly one Target ID. `--confirm-production` is mandatory because the command mutates
the Target's production destination. The headless path loads the same plan and Target environment,
prepares the same production Version and Backend inputs, uses the same 1Password-masked process
runner, reduces the same events, and writes the same local Journal as the TUI. Cloudflare
Pages-backed Targets continue to use the provider's deployment history instead of the local
Journal.

Child output streams to stderr after `op run` masking. With `--json`, stdout contains one terminal
report with the operation, Target ID, Version, status, timings, Step outcomes, and whether a local
Journal entry was written. It contains no commands, paths other than the Journal path, environment
references, or resolved values. `Ctrl-C` requests cancellation through the same process supervisor
used by the TUI.

When a running Step prints a `https://login.tailscale.com/...` authentication URL, Deploy
immediately opens that fresh link in Google Chrome. It also highlights the live-output row; clicking
the highlighted link or pressing `o` retries the shared platform launcher.

Browse, Versions, and Run are keyboard-navigable split surfaces. `Left` / `Right` move between the
visible panels, while `Tab` / `Shift-Tab` cycle them. The cyan border identifies the active region.
Vertical input stays local: `Up` / `Down` (or `k` / `j`) changes the selected Target or Version in a
list, and scrolls Plan, Version detail, Progress, or Live output when that region is active. Clicking
or scrolling a panel activates it. Review, Summary, and Versions loading/error/empty states remain
single-region screens, so navigation never lands on a hidden panel.

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

The active region and scroll position are session state, not preferences. Each phase starts at its
primary region; only the three divider ratios are saved.

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

Relative `working_dir`, `source_roots`, and `env_file` values are resolved from the directory
containing the selected configuration file. `working_dir` is always the primary source root. A
Target can list additional Git worktrees in `source_roots`; Kit includes their commits, tracked
changes, untracked paths, and untracked contents in `KIT_DEPLOY_SOURCE_SHA256`, and marks the
aggregate dirty when any declared root is dirty. Kit never guesses source dependencies or crawls
parent directories. With no `source_roots`, the source identity is unchanged.

Kit does not ship deployment defaults and never guesses a command. If no configuration is found,
the error lists the searched paths and points to the example. Invalid TOML and invalid plans fail
before the terminal enters interactive mode.

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
action = { type = "command", program = "<your-pages-deploy-command>", args = ["<output-directory>", "--project-name=<pages-project>", "--branch={{branch}}"] }
```

`account_id`, `project`, and `token_env` must be non-empty. `token_env` names an `op://` reference in
this Target's `env_file`; the token value never belongs in deploy TOML, the dotenv file, or the
process environment:

```dotenv
CLOUDFLARE_API_TOKEN=op://<vault>/<item>/<field>
```

Kit resolves only that named reference with a bounded, stderr-suppressed `op read --no-newline` in
the spawned Backend task. The literal Backend `account_id` comes from `deploy.toml`; Kit does not
resolve an account-ID environment variable or every other reference in the file. A missing or
literal token entry produces an actionable error and the Versions view supports `r` to retry. The
token needs Cloudflare Pages read access to browse deployments and write access to roll back,
delete, and publish preview deployments.

For this Backend, `v` calls Cloudflare's deployments API and shows newest-first deployment ID,
commit SHA when present, branch, creation time, production/preview environment, latest-stage status,
and URL. The current production deployment (Cloudflare's canonical deployment) is marked `● LIVE`,
and the panel title shows the project's `production_branch`. Rollback calls Cloudflare's rollback
API and appears in the normal Run and Summary views. Cloudflare permits rollback only to successful
production deployments, which Kit enforces before Review.

`o` opens the selected deployment's URL in the browser (`xdg-open` on Linux, `open` on macOS), and
after a Cloudflare publish the Summary shows that immutable URL and `o` opens it there too. `d` deletes the
selected deployment through Cloudflare's delete API after a `y`/`n` confirmation, then refreshes the
list; the live deployment is refused with an actionable notice. `e` and `n`
manage local annotations (see below). Enter from Browse reviews an explicit production deploy: Kit
reads the project's current production branch, substitutes it into `{{branch}}`, and labels the Run
as production through Summary. Cloudflare then advances the canonical deployment served by the
project's production domains. `p` publishes a preview: Kit substitutes the entered branch into
`{{branch}}` and sets `KIT_DEPLOY_BRANCH`, so the same publish Step targets a branch alias instead.
The production branch is rejected for previews. Deploy itself still runs the configured Steps; Kit
does not upload Pages artifacts.

Do not add `[targets.rollback]` to a Cloudflare Pages Target. Platform-backed and local rollback are
mutually exclusive so there is one authoritative owner. Cloudflare Pages Runs are not copied into
Kit's local Journal; reopening Versions reads the platform's current history.

## Target environment file

Set `env_file` on any Target to load ordinary deployment configuration and 1Password references
without exporting plaintext secrets before every Run:

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

Values beginning with `op://` are parsed as validated 1Password references. Other values are
ordinary, non-secret environment configuration. An explicitly exported process value overrides an
ordinary file value; a configured reference is authoritative and is not replaced by an inherited
plaintext value.

Every deploy and local rollback Step executes as:

```text
op run --env-file=<mode-0600-scoped-refs-file> -- <program> <arguments...>
```

The scoped file contains only the selected Target's `NAME=op://...` references and is deleted when
the Step completes. `OP_RUN_NO_MASKING` is removed and cannot be reintroduced by the Target file.
The child gets ordinary environment values plus `KIT_DEPLOY_VERSION`, `KIT_DEPLOY_REF`, the
inspected `KIT_DEPLOY_SOURCE_COMMIT`, `KIT_DEPLOY_SOURCE_DIRTY`, and
`KIT_DEPLOY_SOURCE_SHA256`, and, when applicable, `KIT_DEPLOY_BRANCH`. A Target that declares a
typed artifact also receives `KIT_DEPLOY_ARTIFACT_PATH`. 1Password resolves the references and
masks exact resolved values on the child's piped output before Kit receives it. Kit does not
implement a second literal masker.

`op run` does not mask arbitrary inherited or literal plaintext, transformed/encoded secrets, or a
malicious child's intentional exfiltration. Therefore only `op://` entries are secret inputs;
literal dotenv and process values must not contain secrets. Keep reference files out of source
control when their item names are sensitive. Kit never writes resolved values to the Journal,
output view, or debug formatting.

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

## Deployment annotations

Cloudflare Pages has no place to record "this deployment was bad", so Kit keeps annotations locally
at `$XDG_STATE_HOME/kit/deploy-annotations.json` (normally
`~/.local/state/kit/deploy-annotations.json`). The versioned, typed JSON document maps a Cloudflare
deployment ID to an `error` flag and an optional `note`. `e` toggles the flag and `n` edits the note
from the Cloudflare Versions view; both persist immediately with the same lock-and-atomic-replace
write the journal uses. An entry with neither an error flag nor a note is pruned, so cleared
annotations leave no residue. Annotations are keyed by deployment ID and hold no commit, path, or
secret; deleting a deployment on the platform simply leaves its annotation unreferenced.

## Schema

```toml
version = 1

[[targets]]
id = "staging"
name = "Staging"
description = "Build and publish the staging service"
working_dir = "../service"
source_roots = ["../shared-library"]
artifact = { type = "container-image" }

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
then receive missing ordinary values and resolved references from the Target's `env_file`, plus
`KIT_DEPLOY_VERSION`, `KIT_DEPLOY_REF`, and the three `KIT_DEPLOY_SOURCE_*` values (plus
`KIT_DEPLOY_BRANCH` when the Run resolved a branch). Standard input is closed; stdout and stderr
are captured through `op run` and streamed into the Run view. Put only `op://` references—not
plaintext secret values—in the dotenv file.

For a command-backed container deployment, `artifact = { type = "container-image" }` makes the
artifact identity part of successful completion. The child must write exactly this bounded,
non-secret document to `KIT_DEPLOY_ARTIFACT_PATH` after production is healthy:

```json
{"schemaVersion":1,"sourceCommit":"<git-sha>","digest":"sha256:<64-hex>"}
```

Kit rejects a missing, oversized, malformed, or unknown result and deletes the mode-0600 result
file after the Run. Headless JSON reports both `source` (the exact inspected working-tree
fingerprint) and `artifact` (the deployed source commit and immutable image digest). This avoids
pretending that a dirty command repository commit identifies an image built from another source
repository.

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
