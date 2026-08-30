# Console

`kit console` connects to persistent terminal sessions on this machine or another machine in the
same tailnet. Tailscale identity and authentication are the remote boundary; native per-user
services own agent lifetime.

## TLDR

- Use `kit console <machine>` to connect.
- Run `kit console setup` once, locally on every target account that should own Console sessions.
- Run `kit --json console status` locally on a target as the first service diagnostic.
- Use `kit --json console restart` locally to replace that target's agent only when sessions can be
  preserved.
- Use `--force` only when the operator explicitly accepts terminating live sessions.
- Console does not use SSH, personal keys, `~/.ssh/config`, a remote shell, or a target Unix-user
  selector.

## Identity and authentication

A machine selector is resolved through Tailscale, then pinned to the peer's stable node ID for the
operation. DNS names and Tailscale IPs are routing observations, not durable identity.

The sole remote path is:

```text
Console client
  -> private local relay socket
  -> tailscale nc <current-tailnet-address> 57483
  -> Console gateway bound only to the target's Tailscale addresses
  -> private per-user agent.sock
  -> unchanged mux protocol
```

The gateway authenticates the actual TCP source with `tailscale whois`. It admits a source only
when WhoIs returns a non-empty stable node ID and either the source and target nodes have the same
Tailscale user ID or the source has the exact
`github.com/xtava/kit/cap/console` app capability. A missing target user identity denies closed.
If either node is tagged, the capability is required because Tailscale tags replace user identity;
the tag creator's user ID is never treated as authorization.
The gateway also rejects its own stable node ID, so another local OS account cannot route through
the node's tailnet address into the owner's private socket. The gateway never binds a wildcard,
loopback, LAN, or non-Tailscale address.

One Tailscale node supports one trusted Console owner account and one private agent socket. Failure
to claim an advertised Tailscale address is reported once per conflict in the native service log
and retried without taking down local Console sessions. Do not enable Console for mutually
untrusted OS accounts on the same node: the host process/account boundary is trusted, while
Tailscale identity is the network boundary. Target Unix accounts are not part of remote Console
identity or routing. This keeps the macOS GUI Tailscale runtime compatible: the target accepts an
ordinary tailnet TCP connection and does not need to run Tailscale SSH.
Transient status-probe failures and address churn retain established streams; only an authoritative
logout or authenticated node/user/tag identity change drains the active gateway generation.
On macOS, the shared Tailscale client prefers the GUI app's bundled CLI and enables its documented
CLI mode, so the native Console LaunchAgent does not depend on an interactive shell `PATH`.

## Status and recovery

`ConsoleStatus` in `src/tools/console/service/model.rs` is the source of runtime facts: local
service state, socket state, sessions, build identity, Tailscale readiness, peer reachability, and
typed gateway failures. It does not serialize UI actions.

Codec compatibility is the connection admission boundary. The agent build identity is reported as
diagnostic and update evidence; a different source revision or dirty-build bit does not reject an
otherwise compatible connection. A codec mismatch remains a hard failure. See
[Compatible Console builds were rejected by exact build identity](./pitfalls/console-compatible-build-drift-rejected-by-identity-equality.md).

`ConsoleStatus::recovery()` derives the single recovery policy used by text output and the Control
Center. A new status must therefore be added once at the service model, not independently mapped in
each presentation.

Start diagnosis with:

```bash
kit --json console status
```

That command is intentionally local-only. An absent remote gateway cannot install or restart
itself through the same endpoint. Run setup or lifecycle recovery in a terminal on the target; do
not add SSH as a bootstrap fallback.

The main recovery operations are:

| Observed state | Operation |
| --- | --- |
| Source Tailscale login required | Authenticate at the reported Tailscale URL, then reconnect |
| Target endpoint unavailable | Run `kit console setup` locally on that target |
| Tailnet access denied | Use the target owner's Tailscale account or grant the Console app capability |
| Target protocol incompatible | Update Kit and run `kit console setup` locally on the target |
| Local service stopped or definition damaged | `kit --json console setup` on that machine |
| Peer offline or transport timeout | Restore peer reachability, then reconnect |
| Sessions prevent replacement | Close them normally, or explicitly choose `--force` |

`console start` is intentionally absent. `setup` owns installation, repair, and starting an absent
or stopped service.

## Collaborative terminal input

Every established current Console attachment may operate the sessions exposed by
`ConsoleAuthorizer`. There is no pane owner, controller lease, observer mode, or claim/release
step. `ConsoleAuthorizer` remains the sole product permission boundary; connection identity,
resume tokens, epochs, and disconnect grace fence attachment lifecycle rather than pane ownership.

Writes, keys, paste, mouse input, resize, erase-scrollback, palette changes, and pane close enter
one authoritative mutation lane per pane. Successful enqueue is the acceptance point, accepted
mutations apply in sequence, and pane close is terminal: earlier accepted work drains while later
enqueue attempts reject.

Each visible client announces its rendered terminal size on attach, resume, reveal, and actual
content-size changes. The last accepted viewport announcement becomes the PTY size. Unchanged
clients do not resend size on redraw, animation, focus, terminal output, or transient zero-sized
layout frames. Focus, panel selection, scroll, search, selection, copy, and overlays remain local to
each client.

## Restart semantics

`src/tools/console/service/mod.rs` owns the canonical restart operation. The CLI and Control Center
call that owner; platform modules implement only native service mechanics.

The default restart refuses to destroy live sessions. Forced replacement is explicit:

```bash
kit --json console restart --force
kit --json console status
```

On macOS, the service is a per-user launch agent. `launchctl bootout` can return before the old
registration disappears. The macOS service owner therefore waits until `launchctl print` reports
the service absent before bootstrapping the replacement. Do not replace that wait with a fixed
sleep or move retry policy into the remote client. See
[macOS Console restart races launchd removal](./pitfalls/console-macos-restart-launchd-bootout-race.md)
for the recognition and verification path.

On Linux, the corresponding native service owner uses the same typed restart contract behind its
platform boundary.

## Installation and updates

`kit update` uses the canonical source checkout registered by `./install.sh`. It fetches the exact
registered upstream ref, permits only a safe fast-forward when upstream is newer, and refuses
divergence or any incoming path that overlaps local work. It never stashes, resets, rebases, or
force-switches the checkout. The checkout's `install.sh` replaces `$HOME/.local/bin/kit`, and the
updater verifies that the installed binary reports the selected source revision.

Before changing the binary, the updater checks local Console status. Active sessions refuse the
update. With zero sessions, a successful install invokes the same non-forced Console restart owned
by `src/tools/console/service/mod.rs`, so the running agent is replaced without inventing another
lifecycle path. An existing installation without registered source state must run `./install.sh`
once from the canonical checkout.

Contributor deployment from a dirty Linux checkout is a separate, explicitly approved workflow:
use [One-way source deployment](./source-sync-runbook.md), perform one native check, install once,
then decide whether service activation may replace the current agent.

## Ownership

- `src/tailscale/client.rs` and `model.rs` own status, stable identity, WhoIs, and supervised
  `tailscale nc` command construction.
- `src/tailscale/ssh.rs` remains the shared SSH owner for Sync and Stream; Console does not consume
  it.
- `src/tools/console/transport/gateway.rs` owns target binding, WhoIs admission, and the private
  socket bridge.
- `src/tools/console/transport/tailnet.rs` owns the local relay, versioned gateway handshake, and
  supervised `tailscale nc` epochs.
- `src/tools/console/service/model.rs` owns status facts and recovery policy.
- `src/tools/console/service/mod.rs` owns setup, stop, and restart lifecycle.
- `src/tools/console/authorization.rs` owns the exhaustive product operation policy.
- `vendor/wezterm/mux/src/lib.rs` owns authoritative per-pane mutation sequencing.
- `src/tools/console/service/linux.rs` and `macos.rs` own native service mechanics.
- `src/tools/console/remote.rs` owns remote orchestration, not native lifecycle policy.
- `src/tools/console/control_center/` projects status and recovery into operator actions.
- `src/update.rs` owns source registration and the managed source-update transaction;
  `src/tools/update.rs` exposes it through the CLI.

Do not add an SSH fallback, another restart path, presentation-specific recovery table,
release-download updater, wildcard listener, or second remote transport.
