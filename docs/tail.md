# Tail sharing

`kit tail` is an interactive client for sharing text and files between devices on the same
Tailscale network. Tailscale remains the transport, identity, encryption, and authorization owner;
Kit adds the local workflow, receive cache, and terminal UI.

While the TUI is open, Kit refreshes Tailscale readiness and devices every two seconds. Newly
activated, disconnected, renamed, or newly Taildrop-eligible machines update in place without
resetting the selected recipient, search, or unsent draft. The header shows `● live` while this
reconciliation is active.

## Start

Install and connect the official Tailscale CLI, then run:

```bash
kit tail
```

If this device is not authenticated, Kit starts `tailscale login`, shows the login URL, and waits
for Tailscale to report both `BackendState=Running` and a local Tailscale IP. Press `o` to open the
link or `c` to copy it. Kit does not store auth keys or credentials.

## Share

- Select a device once, then press `Enter`, `p`, or click the message box. The composer stays on
  screen before, during, and after every send.
- Type or paste text and press `Enter` to send immediately. The message enters a serial background
  queue, the composer clears, and you can type the next message without waiting.
- `Alt+Enter` or `Shift+Enter` inserts a newline. Arrow, Home, End, Backspace, Delete, `Ctrl+A`,
  `Ctrl+E`, `Ctrl+W`, and `Ctrl+U` edit the in-memory message normally.
- A nonempty message is locked to the device it started with. Selecting another device cannot
  silently retarget it; use **Move message here**, send it, or clear it.
- Press `f` from a device or `Ctrl+F` from the composer for the portable file browser. `Space`
  selects multiple files and `s` reviews them.
- Drag one or more files into the terminal. Terminal drag/drop arrives as a bracketed paste; Kit
  recognizes quoted paths, `file://` URLs, percent encoding, and multiple paths.
- Any paste that resolves to local files remains reversible: review the exact files and recipient,
  or insert the original paste as text. Mixed paths and prose also require an explicit choice.
- Outgoing text is streamed to `tailscale file cp --name=… - <target>:` and is never placed in the
  receive cache.

Each queued item captures its recipient immediately. Sends run one at a time so rapid submissions
stay ordered and do not create competing Tailscale processes. The header and composer show sending,
queued, failed, and latest-result state. **Cancel send** stops only the active item; later items stay
queued. **Retry failed** appends failed items behind work already waiting. Successful text payloads
are discarded from memory, and all remaining outgoing state disappears when the TUI exits.

## Receive

- Receiving starts automatically while the TUI is open and continues independently of navigation
  and sending. It holds `tailscale file get --wait` open, so arrivals wake it immediately rather
  than waiting for a polling interval. Interrupted receives retry with bounded backoff.
- Exactly one open Tail session owns receiving on each machine. Additional sessions show
  `receiver elsewhere`, keep their cached-item panels synchronized, and take over automatically
  when the owner closes.
- The header reports the real lifecycle: `waiting`, `receiving`, `receiver elsewhere`, or the live
  retry countdown. It does not label a failed or standby receiver as merely “watching.”
- `w`: pause or resume automatic receiving. `r` explicitly resumes it.
- `Tab`: move focus between devices, received items, and the persistent composer.
- `c`: copy a selected text item to this terminal's clipboard through OSC 52.
- `s`: choose a destination directory, then move the item out of the cache.
- Right-click a device or received item for its shared context menu.
- `o`: open the cached payload with the platform opener.
- `d`: confirm and delete the cached copy.
- `Enter`: preview received text or inspect file details; press `c` from the preview to copy it.
- `/`: filter devices and cached items.

Every visible action is clickable. Lists support click selection and wheel scrolling. On wide
terminals, drag the divider between Devices and Received; Kit saves the ratio for the next run.
Narrow terminals stack the same panels vertically. Clicking inside the composer focuses it and
places the cursor. **Quit** is always available in the header; Kit confirms before abandoning an
active, queued, or retryable send, or an unsent message draft.

## Settings

Tail contributes its preferences to the shared `kit settings` TUI:

- **Automatic receiving**: start the inbox watcher whenever `kit tail` is open. Default: on.
- **Mouse interaction**: enable clicks, scrolling, context menus, and split resizing. Default: on.

The split ratio is saved automatically after a divider drag. These values live in the shared Kit
configuration store under `tail.toml`.

## Cache and safety

Received items live under Kit's platform cache directory in `tail/received`. Each item has an
opaque UUID directory, a fixed `payload` path, and a minimal manifest containing only its display
name and receive time.

- Items expire after 30 days and are pruned when `kit tail` starts.
- Tailscale does not expose authoritative sender metadata for retrieved Taildrop files, so Kit
  presents a global received inbox and never guesses which peer sent an item.
- UTF-8 payloads up to 1 MiB are copyable text; everything else is treated as a file.
- Unix cache directories use mode `0700`; payloads and manifests use `0600`.
- Receive staging is cleaned after successful adoption and ordinary cancellation. After Tailscale
  exits successfully, Kit atomically marks that batch complete before importing it. A new receiver
  adopts only proven-complete batches; incomplete or failed batches are preserved and reported,
  never guessed complete or deleted as recovery fallout.
- Symlinks and non-regular files are never imported.
- Save removes the cached copy only after the destination move succeeds.
- Filename conflicts offer keep-both, safe replacement, or another destination.
- Cache deletion validates that the target is a direct, UUID-named child of the Tail cache root.

## Platform behavior

- Linux: `xdg-open`
- macOS: `open`

All Tailscale subprocesses run through Kit's process supervisor. Targets and paths are passed as
process arguments, never interpolated into a shell command.
