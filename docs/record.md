# kit record

`kit record` is the operator shell for Modular's Playwright recorder. Kit owns
the command-line and interactive terminal experience; Modular owns the recorder
implementation, snapshots, Electron launch, replay, and artifact layout.

## TLDR

- Start the interactive lane with `kit record -i`.
- `start` shells into Modular's `pnpm record`.
- `stop` shells into Modular's `pnpm record-stop`.
- `cancel` shells into Modular's `pnpm record-cancel` and closes the recorder
  window without finalizing replay artifacts.
- `replay [DIR]` shells into `pnpm record -- --replay [DIR]`.
- After a recording stops, the feed prints the current recording directory.
- `rename NAME` moves the current recording into the saved recordings directory.
- Tab completion covers commands, replay directories, scenarios, and a suggested
  rename value.
- Install or refresh the managed binary from this repo with `./install.sh`.

## Ownership

Kit source:

- `src/tools/record/mod.rs` owns the `kit record` CLI surface.
- `src/tools/record/tui.rs` owns the `kit record -i` interactive lane.
- `src/main.rs` registers the tool.

Modular remains the recorder owner. The Kit tool calls these scripts in the
Modular repo selected by `--repo`:

```bash
pnpm record -- --scenario <scenario>
pnpm record-stop -- --scenario <scenario>
pnpm record-cancel -- --scenario <scenario>
pnpm record -- --scenario <scenario> --replay [dir]
```

The repo has no built-in default — it is machine-specific. Set it once as `repo`
in `record.toml` (the `kit` config dir), or pass `--repo` / use the interactive
`repo PATH` command to point at your Modular checkout.

## Interactive Use

```bash
kit record -i
```

The screen follows the same Kit TUI spine as `kit cdp -i`: a feed on top and a
command line on the bottom. Child process output is captured into the feed so the
alternate-screen terminal stays coherent.

Commands:

```text
start [--out DIR]    start recording
stop                 stop and flush the active recording
cancel               cancel the active recording and close its window
replay [DIR]         replay the current or provided recording
status               show run-state and current artifact directory
events               summarize physical-events.jsonl
artifacts            list files in the current recording directory
rename NAME          save the current recording under a stable name
repo [PATH]          show or change the Modular checkout
scenario [ID]        show or change the scenario id
help                 show help
quit                 exit
```

Keys:

- `Tab` selects from suggestions.
- `Enter` accepts an engaged suggestion or submits the line.
- `Right` accepts the inline ghost when one is available.
- `Esc` hides suggestions or re-pins the feed to live.
- `PgUp` and `PgDn` scroll the feed.
- `Ctrl-P` and `Ctrl-N` walk prompt history.
- `Ctrl-L` clears the feed.
- `Ctrl-C` on an empty prompt cancels an active recording, closes an active
  replay window, and exits.
- `Ctrl-D` exits.

## Artifact Layout

The current recording directory lives outside Playwright's `test-results`
scratch directory:

```text
<repo>/artifacts/e2e-recordings/current/instance-<id>/<scenario>
```

Saved recordings created by `rename NAME` are stored under:

```text
<repo>/artifacts/e2e-recordings/saved/instance-<id>/<NAME>
```

The instance id is resolved in this order:

1. `INSTANCE_ID` from the process environment.
2. `INSTANCE_ID=` in `<repo>/.worktree-env`.
3. `0`.

Typical files in a recording:

```text
metadata.json
physical-events.jsonl
generated.spec.ts
run-state.json
final-state.json
trace.zip
video/
```

## Rename Flow

After `stop` and after the recorder process exits, the TUI prints:

```text
recording dir: <repo>/artifacts/e2e-recordings/current/instance-0/<scenario>
rename with: rename <suggested-name>
```

Run:

```text
rename drag-file-explorer-to-editor
```

That moves the current recording to:

```text
<repo>/artifacts/e2e-recordings/saved/instance-0/drag-file-explorer-to-editor
```

`rename` rejects empty names, `.` and `..`, path separators, and existing saved
recording names. It moves the directory; it does not copy it.

The same operation is also available outside the TUI:

```bash
kit record rename drag-file-explorer-to-editor
```

## Completion

The interactive prompt offers:

- command names at the first word;
- `--out` after `start` or `record` when typing a flag;
- the current recording directory and saved recording directories after
  `replay`;
- known current scenario directories after `scenario`;
- a timestamped suggested name after `rename`;
- the active Modular repo after `repo`.

Completion is local to Kit. It does not query Playwright or launch Electron.

## Replay Notes

`replay` can open an Electron replay window. The replay script waits for a manual
finish prompt after it has driven the recording. In `kit record -i`, press Enter
on an empty prompt to send that newline to the replay process and close the
replay window.

If an older replay was launched outside the current TUI and remains stuck, look
for a process tree rooted at:

```text
pnpm record -- --replay
```

Terminate that replay tree only. Do not kill unrelated Modular dev or canary
windows just because their command lines also contain `electron`.

## Install Or Update

From the kit repo root:

```bash
cargo fmt
cargo check
cargo test
./install.sh
```

The installed binary path is usually:

```text
~/.local/bin/kit
```

After installing, check:

```bash
"$HOME/.local/bin/kit" record --help
"$HOME/.local/bin/kit" record status
```
