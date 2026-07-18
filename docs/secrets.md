# Secrets

`kit secrets` is a local terminal UI over the official 1Password CLI. It is not a vault and does
not implement cryptography, synchronization, authentication, or secret persistence. The 1Password
desktop app and `op` remain the security and storage owners.

## Requirements

1. Install the current 1Password desktop app and official `op` CLI.
2. Sign in to and unlock the desktop app.
3. Enable **Settings > Developer > Integrate with 1Password CLI**.
4. Enable Touch ID on macOS or **Unlock using system authentication** on Linux if desired.

Launch the client:

```bash
kit secrets
```

With multiple accounts, choose one in the opening screen or pass its ID, sign-in address, or
shorthand:

```bash
kit secrets --account my.1password.com
```

## Keys

| Key | Action |
| --- | --- |
| `/` | Search the in-memory metadata index. `Enter` or `Esc` returns to browsing. |
| `j` / `k`, arrows | Select the next or previous item. |
| `Enter` | Open the selected item's already-loaded metadata. This performs no field read. |
| `u` | Fetch only the standard Login username field and copy it through OSC 52. |
| `y` | Fetch only the standard Login password field and copy it through OSC 52. |
| `n` | Create a Login item. An empty password asks 1Password to generate one. |
| `g` | Confirm and replace the password with a generated 32-character password. |
| `d` | Confirm and move the item to 1Password Archive. |
| `R` | Refresh metadata from 1Password. |
| `q`, `Ctrl-C` | Exit and drop Kit-owned buffers. |

In the create form, use `Tab` and `Shift-Tab` to move, `Ctrl-S` to submit, and `Esc` to cancel.
Manual passwords are limited to 4096 UTF-8 bytes.

## Security design

### Authentication and authorization

Kit never asks for or caches the 1Password account password, Secret Key, session token, or service
account token. Desktop app integration authenticates each `op` child process. According to
1Password's app-integration security documentation, authorization for an account in one terminal
session normally expires after 10 minutes of inactivity, renews with use up to a 12-hour limit, and
is revoked when the desktop app locks.

Kit has the same account and vault access as the signed-in `op` CLI. It does not create a second
permission layer. Use 1Password vault permissions and an appropriately scoped account when the
metadata visible to the TUI needs to be restricted.

### Metadata and search

- Startup fetches `op item list --format=json` and readable vault metadata for the chosen account.
- Search indexes only list-provided title, category, vault name, tags, URLs, and additional
  information.
- The index, selection, and query are process-memory only. Kit creates no secrets config, database,
  recency history, or cache.
- Opening an item is local and does not run `op item get`.
- Metadata is sensitive even when it is not a password. It is intentionally visible in this TUI and
  remains in memory until the process exits.

### Secret reads and memory

- Username and password copy use `op read op://<vault-id>/<item-id>/<field>` with stable IDs and a
  fixed built-in field name.
- Kit executes `op` directly with `tokio::process::Command`; it never constructs a shell command.
- Secret values are absent from argv, environment variables, config, logs, error messages,
  temporary files, JSON decoders, search indexes, and terminal rendering.
- Secret stdout is capped at 4096 bytes and read into one explicitly bounded, preallocated,
  non-reallocating, zeroizing buffer. Closing the item detail before a read completes revokes the
  pending clipboard delivery. Stderr from secret-returning operations is discarded rather than
  surfaced.
- The secret input type deliberately has no `Clone`, `Debug`, `Display`, `Serialize`, or
  `Deserialize` implementation. Deletion and clear operations overwrite removed bytes.
- Kit does not offer on-screen reveal. Rendering plaintext would copy it into Ratatui and terminal
  emulator buffers that Kit cannot reliably erase.

Zeroization is best-effort process-memory hygiene. It cannot erase copies owned by `op`, Tokio or OS
pipes, the kernel, allocator internals, swap, crash infrastructure, the terminal, or 1Password. It
also does not defend a compromised host, terminal emulator, `op` executable, or same-user debugger.

### Create and mutation safety

- Login create data is measured, serialized into an exactly sized zeroizing JSON buffer, and sent
  to `op item create -` over stdin. Titles, usernames, URLs, and manual passwords never enter argv.
- Generated create and rotation use 1Password's `--generate-password` flag. Create stdout and
  stderr are discarded, so generated values do not enter Kit memory during those operations.
- Existing arbitrary password replacement and whole-item JSON editing are not exposed. 1Password
  warns that JSON item edits can overwrite passkeys.
- Metadata editing is not exposed because documented flag-based edits place titles, URLs, and tags
  in process-visible argv.
- Permanent deletion is not exposed. `d` uses the documented archive operation and requires
  confirmation.
- Every successful write refreshes the authoritative metadata projection from 1Password. Kit does
  not maintain an optimistic shadow copy.

### Clipboard boundary

Kit sends copied values through the terminal's OSC 52 mechanism. Its temporary base64 escape is
zeroized immediately after the terminal write, but the terminal and OS clipboard then own the
plaintext. Kit deliberately does not clear the clipboard on a timer: OSC 52 cannot prove that the
clipboard still contains Kit's value, so blind clearing could erase something copied later.

Terminals and multiplexers can block or log OSC 52. Kit will report a copy failure; it will not print
the field as a fallback.

## Documented 1Password practices applied

| 1Password guidance | Kit behavior |
| --- | --- |
| Keep the CLI current. | Uses the installed official `op`; operators should update it through the official package channel. |
| Use desktop app integration for interactive CLI authentication. | Delegates every authorization decision and cadence to the desktop app. |
| Prefer stable object IDs. | Every field read and mutation uses account, vault, and item IDs. |
| Use `op read` to retrieve one field. | `u` and `y` each request exactly one fixed Login field. |
| Avoid sensitive values in command arguments; use JSON templates for create. | Manual create values use bounded JSON stdin; no field value enters argv. |
| Apply least privilege. | Kit cannot narrow the signed-in account; vault permissions remain the enforcement boundary. |
| Treat whole-item JSON edit carefully because it can overwrite passkeys. | Whole-item editing is absent. |

Primary references: [CLI best practices](https://www.1password.dev/cli/best-practices),
[secret references](https://www.1password.dev/cli/secret-references),
[`op read`](https://www.1password.dev/cli/reference/commands/read),
[item commands](https://www.1password.dev/cli/reference/management-commands/item), and
[app-integration security](https://www.1password.dev/cli/app-integration-security).

## Platform status

- Linux: implemented and contract-tested with the installed official CLI and desktop integration.
- macOS: the process design and official desktop integration are supported, but a native macOS TUI
  smoke test is still required before claiming runtime verification.
- Windows: not runtime-verified; OSC 52 and child-process integration need a native smoke test.

## Troubleshooting

### No accounts are available

Unlock the desktop app and enable **Settings > Developer > Integrate with 1Password CLI**.

### Connection reset on Linux

Confirm that a manually installed system `op` executable has the official group and setgid
permissions:

```bash
stat -c '%U %G %a %n' "$(command -v op)"
```

The official manual installation normally reports `root onepassword-cli 2755`.

### Copy does not reach the clipboard

Enable OSC 52 clipboard writes and passthrough in every terminal or multiplexer layer. Kit does not
fall back to displaying or printing a secret.
