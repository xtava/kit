# Synced Projects

`kit sync` keeps the source portion of a project aligned between two Tailscale machines. Each
machine retains its own Git metadata, dependencies, build output, caches, tools, credentials, and
running processes.

## Requirements

- Linux or macOS on both endpoints.
- Tailscale online on both endpoints.
- Tailscale SSH enabled for the remote machine. Personal SSH keys and `~/.ssh/config` are neither
  required nor consulted.
- Mutagen `0.18.1` and its adjacent `mutagen-agents.tar.gz` bundle on the initiating machine.
- Existing absolute local and remote directories.

Run `kit sync doctor` for global setup diagnostics. Authentication failures include the trusted
Tailscale login URL when one is available.

Kit binds Mutagen to private `ssh` and `scp` launchers through `MUTAGEN_SSH_PATH`. Those launchers
disable agent, key, password, keyboard-interactive, GSSAPI, and host-based authentication, then use
Tailscale SSH authentication with a Kit-owned known-host store keyed by stable Tailscale node ID.
Kit also assigns Mutagen a private data directory. Mutagen client and daemon discovery therefore
remain inside one Kit-owned runtime instead of colliding with the operator's default Mutagen
daemon. “Hermetic” means this behavior is determined by Kit-owned state rather than ambient
personal SSH configuration or credentials.

### Existing-daemon handoff

Mutagen's daemon retains the environment from the process that originally launched it. Installing
a corrected Kit binary cannot retrofit `MUTAGEN_SSH_PATH` into an older daemon that already owns
Kit's data directory.

Before using the current private transport for the first time, `MutagenClient` takes an atomic
generation lock, stops the pre-hermetic daemon, and publishes the transport generation marker only
after that stop succeeds. The next command starts Mutagen with both Kit-owned environment
variables. This handoff happens once per transport generation; it does not become a restart on
every Sync command.

Do not repair this boundary by adding a personal SSH key, enabling `~/.ssh/config`, or manually
starting Mutagen. See
[Mutagen daemon retained a non-hermetic SSH environment](./pitfalls/sync-mutagen-daemon-retains-old-ssh-environment.md)
for recognition and live verification.

## Create and operate a project

Open the dashboard:

```sh
kit sync
```

Or create a project directly:

```sh
kit sync add kit-sync tvxm /Users/tvx/Desktop/projects/kit-sync \
  --user tvx \
  --local-root /home/tvx/Desktop/projects/kit-sync
```

The machine selector must resolve to exactly one online Tailscale peer. The project name or UUID
then selects it for every lifecycle command:

```sh
kit sync status kit-sync
kit sync flush kit-sync
kit sync pause kit-sync
kit sync resume kit-sync
kit sync doctor kit-sync
kit sync remove kit-sync
```

Removal terminates only the engine session owned by that Synced Project. It preserves files on
both endpoints.

Use `--json` before the subcommand for automation:

```sh
kit --json sync status
```

## Dashboard

The dashboard refreshes all projects through one bounded background operation. A refresh never
blocks navigation, settings, the command palette, or Quit; lifecycle actions are serialized.
Disconnected sessions are shown as `offline` and catch up automatically after connectivity
returns.

Default controls:

| Action | Key |
| --- | --- |
| Command palette | `Ctrl+P` |
| Add project | `n` |
| Pause or resume | `Space` |
| Sync now | `f` |
| Diagnose | `d` |
| Remove | `x` |
| Refresh | `r` |
| Settings | `,` |
| Previous / next project | `Up` / `Down` |
| Selection history | `Left` / `Right` |
| Change panel focus | `Tab` / `Shift+BackTab` |
| Quit | `q` |

Every visible action is mouse-accessible. Right-click a project for its context menu, click either
panel to focus it, or drag the divider to persist its width. The add form accepts terminal paste,
including paths inserted by terminal drag-and-drop.

Settings and keybindings live in `~/.config/kit/sync.toml` and are also editable inside the
dashboard.

## Source boundary

Synced Projects use Mutagen's `two-way-safe` mode and a stable source-only policy.
Linux endpoints use a one-second polling fallback for cold paths while retaining Mutagen's
low-latency native-watch fast path. macOS endpoints retain the native watcher defaults.
Kit versions this complete runtime contract with a session profile label. A session created by an
older profile is reported as `stale` instead of silently retaining outdated synchronization
behavior; remove and recreate that Synced Project to apply the current profile without deleting
endpoint files.

- `.git/` never crosses endpoints.
- Common dependencies, build output, caches, virtual environments, and local environment files
  remain machine-local.
- `.gitignore` files synchronize as source, but changing one does not silently change a running
  project's synchronization boundary.
- `--exclude` adds an engine ignore pattern.
- `--include` re-includes a path excluded by an earlier pattern.
- Concurrent edits to one path surface as a conflict; Kit never silently chooses a winner.

Resolve a conflict by choosing the intended contents on one endpoint and removing or replacing the
conflicting copy on the other, then run `kit sync flush <project>`.

## Ownership

`SyncController` is the single lifecycle and monitoring owner used by both CLI and TUI.
`MutagenClient` owns the pinned engine protocol, private data-directory environment, and daemon
transport generation. `src/tailscale/mutagen_ssh.rs` owns the permanent adapter from Mutagen's
external `ssh`/`scp` protocol to Kit's private Tailscale SSH policy. Each synchronization session is
bound to both the project UUID and stable Tailscale node identity. `Config` owns durable project
intent and dashboard preferences. TUI actions contribute once through Kit's shared action registry
and project into keyboard, mouse, menus, and the command palette.

Kit does not implement its own filesystem watcher, reconciliation algorithm, transfer protocol, or
per-project polling loop.
