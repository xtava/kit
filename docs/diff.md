# `kit diff`

`kit diff` is a terminal review surface for the current Git repository. It shows staged changes
(`HEAD` to index) and unstaged changes (`index` to worktree) as separate tree groups, so a path with
both kinds of changes appears once in each group with the correct comparison. The selected file can
be staged or unstaged without leaving the viewer.

```bash
kit diff
kit diff --mode unified
kit diff --mode split
kit diff --theme terminal
```

`--mode auto` is the default. Modified files use side-by-side mode when the content pane is at least
72 columns wide and unified mode below that. Added and untracked files always use one full-width new
file pane; deleted files always use one full-width old file pane. An explicit split request reports
its 50-column content minimum for genuinely two-sided comparisons instead of silently changing modes.

## Controls

| Input | Action |
|---|---|
| `↑` / `↓`, `k` / `j` | Select a changed file or scroll the active code region |
| `n` / `N`, `]` / `[` | Select next or previous hunk |
| `PageUp` / `PageDown` | Scroll the selected comparison |
| `←` / `→` | Move between the Changes, old-file, and new-file regions |
| `h` / `l` | Pan the active code region horizontally |
| `v` | Toggle unified and split projections |
| `s` | Stage the selected Changes file or unstage the selected Staged file |
| `r` | Refresh staged, unstaged, and untracked changes from Git |
| `Tab` / `Shift-Tab` | Cycle the visible interactive regions |
| `Home` / `End` | Return to the start and reset panning, or jump to the end |
| `q`, `Esc`, `Ctrl-C`, `Ctrl-D` | Quit and restore the terminal |

The cyan border identifies the active region. Click a file to select it, or click a group/directory
to expand or collapse it. Each wheel event scrolls the surface under the pointer by one row;
Shift-wheel pans the active code region. In split mode, click a region to activate it and drag the
center divider to resize it. The divider is clamped so neither side disappears. Use `--no-mouse` to
keep terminal mouse reporting disabled.

## Review presentation

The tree presents untracked files as additions (`A`) rather than exposing Git porcelain's `?`
marker. Addition counts are green, deletion counts are red, and zero-valued sides are omitted.
Uninterrupted single-child directory chains are compacted into one row while preserving the terminal
directory as the expansion identity; mixed and branching directories remain separate rows.

Code rows use narrow colored bars and muted row backgrounds instead of literal patch `+` / `-`
prefixes. Unified mode omits line numbers to maximize code width; split mode retains one number per
pane for side alignment. Omitted context is labeled as “N unmodified lines” rather than displaying
raw `@@` hunk headers. These are presentation choices only: every staged, unstaged, and untracked
path reported by Git remains visible in the viewer.

## What is compared

Git CLI output is the repository source of truth. The viewer parses porcelain-v2 status, reads raw
HEAD/index blobs for staged comparisons, and asks Git to render the index blob through worktree
attributes for unstaged comparisons. This preserves installed Git behavior for line-ending and
clean/smudge attributes instead of turning those transformations into false whole-file changes.

The two render modes project one canonical set of hunks and aligned rows. Switching modes preserves
the selected file, hunk, and logical row. Syntax highlighting runs over each complete file side in
source order before visible hunks are projected, so multiline syntax state remains correct.

The viewer explicitly labels conflicts, submodules, binary content, non-UTF-8 content, unavailable
files, and text inputs whose combined old/new size exceeds the 8 MiB safety limit. It also preserves
CRLF and missing-final-newline information. Paths that are not valid UTF-8 are byte-escaped on Unix.

## Scope and limitations

`kit diff` writes only the Git index when `s` is pressed. It never discards or edits worktree content,
creates commits, or changes branches. Press `r` to refresh manually; the current snapshot remains
visible if reloading fails. It has no automatic watch mode. It does not show ignored files, recurse
into submodules, compare arbitrary commits/ranges, read patch files or stdin, or perform semantic/AST
diffing.

The default code behavior is horizontal scrolling rather than wrapping, because wrapping would break
the shared row identity required by the side-by-side projection.
