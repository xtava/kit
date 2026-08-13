# `kit diff`

`kit diff` is a terminal review surface for the current Git repository. It shows staged changes
(`HEAD` to index) and unstaged changes (`index` to worktree) as separate tree groups, so a path with
both kinds of changes appears once in each group with the correct comparison. The selected file can
be staged or unstaged without leaving the viewer.

```bash
kit diff
kit diff --mode inline
kit diff --mode split
kit diff --context 10
kit diff --context all
kit diff --theme terminal
kit settings
```

`--mode auto` is the default. Modified files use side-by-side mode when the content pane is at least
72 columns wide and inline mode below that. Added and untracked files always use one full-width new
file pane; deleted files always use one full-width old file pane. An explicit split request reports
its 50-column content minimum for genuinely two-sided comparisons instead of silently changing modes.

`--context` controls unchanged lines around each change. It defaults to `3`, accepts any
non-negative line count, and accepts `all` to show every unchanged line between changes and at the
start and end of the file.

## Controls

| Input | Action |
|---|---|
| `↑` / `↓`, `k` / `j` | Select a changed file or scroll the active code region |
| `n` / `N`, `]` / `[` | Select next or previous hunk |
| `PageUp` / `PageDown` | Scroll the selected comparison |
| `←` / `→` | Move between the Changes, old-file, and new-file regions |
| `h` / `l` | Pan the active code region horizontally |
| `v` | Toggle inline and side-by-side split views |
| `<` / `>` | Narrow or widen the Changes tree in a wide terminal |
| `F` | Fit the Changes tree to the visible paths and counts |
| `Ctrl-T` | Hide or restore the Changes tree for focused review |
| `=` | Reset the Changes tree to its default width |
| `s` | Stage the selected Changes file or unstage the selected Staged file |
| `o` | Open the selected file with the system default handler |
| `O` | Reveal the selected file in the system file manager |
| `p` | Preview the selected file when the platform provides a native preview |
| `r` | Refresh staged, unstaged, and untracked changes from Git |
| `Tab` / `Shift-Tab` | Cycle the visible interactive regions |
| `Home` / `End` | Return to the start and reset panning, or jump to the end |
| `q`, `Esc`, `Ctrl-C`, `Ctrl-D` | Quit and restore the terminal |

The cyan border identifies the active region. Click a file to select it, or click a group/directory
to expand or collapse it. Hover a file row to replace its change counts with a `+` button for
unstaged files or a `-` button for staged files; click that exact adornment to stage or unstage the
file. Each wheel event scrolls the surface under the pointer by one row;
Shift-wheel pans the active code region. In a wide terminal, drag the divider between the Changes
tree and review panel to resize the tree; the proportional width is saved for the next run. Press
Escape during a drag to restore the starting width. In split mode, the divider between the old and
new files can be dragged independently. Both dividers are clamped so their neighboring panes remain
usable. Use `--no-mouse` to keep terminal mouse reporting disabled; the keyboard resize controls
remain available.

Click `[-]` in the Changes title or press `Ctrl-T` to give the review panel the whole terminal;
click `changes [+]` or press `Ctrl-T` again to restore the tree at its saved width. `F` measures the
currently expanded tree—including indentation, paths, counts, hover actions, borders, and any
scrollbar—and chooses the smallest width that shows it without crowding the review pane.

File actions resolve the selected Git path from the canonical worktree root, so they keep working
when `kit diff` is launched from a nested directory. They use Kit's shared external-handoff and
process-supervision framework: `o` opens the file, while `O` selects it in Finder or Explorer and
opens its containing directory on Linux. `p` uses the native preview capability when one is
available (Quick Look on macOS); unsupported actions are reported in the footer instead of running
an ad hoc fallback command.

## Review presentation

The tree presents untracked files as additions (`A`) rather than exposing Git porcelain's `?`
marker. Addition counts are green, deletion counts are red, and zero-valued sides are omitted.
Uninterrupted single-child directory chains are compacted into one row while preserving the terminal
directory as the expansion identity; mixed and branching directories remain separate rows.

Code rows use narrow colored bars and muted row backgrounds instead of literal patch `+` / `-`
prefixes. The `line_numbers` choice in the Diff Settings section is `auto` by default: inline mode
omits line numbers to maximize code width, while split mode retains one number per pane for side
alignment. Choose `always` or `never` in `kit settings`; the value is persisted in the XDG-scoped
`diff.toml`. Omitted context is labeled as “N unmodified lines” rather than displaying
raw `@@` hunk headers. These are presentation choices only: every staged, unstaged, and untracked
path reported by Git remains visible in the viewer.

## What is compared

Git CLI output is the repository source of truth. The viewer parses porcelain-v2 status, reads raw
HEAD/index blobs for staged comparisons, and asks Git to render the index blob through worktree
attributes for unstaged comparisons. This preserves installed Git behavior for line-ending and
clean/smudge attributes instead of turning those transformations into false whole-file changes.

The two render modes project one canonical set of hunks and aligned rows. Switching modes preserves
the selected file, hunk, and logical row. Source rows retain change backgrounds and character-level
emphasis without delaying the first frame for whole-file syntax parsing.

The viewer explicitly labels conflicts, submodules, binary content, non-UTF-8 content, unavailable
files, and text inputs whose combined old/new size exceeds the 8 MiB safety limit. It also preserves
CRLF and missing-final-newline information. Paths that are not valid UTF-8 are byte-escaped on Unix.

## Scope and limitations

`kit diff` writes only the Git index when `s` is pressed. The open, reveal, and preview controls hand
paths to the operating system but do not edit them in Kit. Diff never discards worktree content,
creates commits, or changes branches. Press `r` to refresh manually; the current snapshot remains
visible if reloading fails. It has no automatic watch mode. It does not show ignored files, recurse
into submodules, compare arbitrary commits/ranges, read patch files or stdin, or perform semantic/AST
diffing.

The default code behavior is horizontal scrolling rather than wrapping, because wrapping would break
the shared row identity required by the side-by-side projection.
