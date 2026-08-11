# Console

`kit console` connects to persistent terminal sessions on this machine or another machine in the
same tailnet. Tailscale identity and authentication are the remote boundary; native per-user
services own agent lifetime.

## TLDR

- Use `kit console <machine>` to connect.
- Use `kit --json console status <machine>` as the first diagnostic.
- Use `kit --json console setup <machine>` to install or repair the service.
- Use `kit --json console restart <machine>` to replace the agent only when sessions can be
  preserved.
- Use `--force` only when the operator explicitly accepts terminating live sessions.
- Personal SSH keys and `~/.ssh/config` are not part of Console authentication.

## Identity and authentication

A machine selector is resolved through Tailscale, then pinned to the peer's stable node ID for the
operation. DNS names and Tailscale IPs are routing observations, not durable identity.

All non-relay Console probes and commands use the shared process specification in
`src/tailscale/ssh.rs`. That owner disables user and system OpenSSH configuration, agent and key
authentication, password and keyboard-interactive authentication, GSSAPI, host-based
authentication, forwarding, proxy commands, and connection sharing. It stores accepted host keys
under Kit state and keys host identity by stable Tailscale node ID.

If Tailscale SSH check mode returns a login URL, authenticate through that URL and retry. Do not add
a personal SSH key as a workaround.

## Status and recovery

`ConsoleStatus` in `src/tools/console/service/model.rs` is the wire-level source of runtime facts:
service state, socket state, sessions, build identity, reachability, and typed remote failures. It
does not serialize UI actions.

`ConsoleStatus::recovery()` derives the single recovery policy used by text output and the Control
Center. A new status must therefore be added once at the service model, not independently mapped in
each presentation.

Start diagnosis with:

```bash
kit --json console status <machine>
```

The main recovery operations are:

| Observed state | Operation |
| --- | --- |
| Tailscale login required | Authenticate at the reported Tailscale URL, then retry status |
| Unix user missing | Configure the correct remote user, then retry |
| Kit or Console absent | `kit --json console setup <machine>` |
| Service stopped or definition damaged | `kit --json console setup <machine>` |
| Remote Kit out of date and no sessions block replacement | `kit update` on that machine, then setup or restart |
| Peer offline or transport timeout | Restore peer reachability, then retry status |
| Sessions prevent replacement | Close them normally, or explicitly choose `--force` |

`console start` is intentionally absent. `setup` owns installation, repair, and starting an absent
or stopped service.

## Restart semantics

`src/tools/console/service/mod.rs` owns the canonical restart operation. The CLI and Control Center
call that owner; platform modules implement only native service mechanics.

The default restart refuses to destroy live sessions. Forced replacement is explicit:

```bash
kit --json console restart <machine> --force
kit --json console status <machine>
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

Published installations use the verified GitHub Release path owned by `src/tools/update.rs`.
Interactive “Update now” and `kit update` invoke that same managed installer. Neither path updates
from a source checkout.

Contributor deployment from a dirty Linux checkout is a separate, explicitly approved workflow:
use [One-way source deployment](./source-sync-runbook.md), perform one native check, install once,
then decide whether service activation may replace the current agent.

## Ownership

- `src/tailscale/ssh.rs` owns shared supervised Tailscale SSH command construction.
- `src/tools/console/service/model.rs` owns status facts and recovery policy.
- `src/tools/console/service/mod.rs` owns setup, stop, and restart lifecycle.
- `src/tools/console/service/linux.rs` and `macos.rs` own native service mechanics.
- `src/tools/console/remote.rs` owns remote orchestration, not native lifecycle policy.
- `src/tools/console/control_center/` projects status and recovery into operator actions.
- `src/tools/update.rs` owns managed release installation.

Do not add another restart path, presentation-specific recovery table, source-checkout updater, or
tool-local copy of the shared SSH process policy.
