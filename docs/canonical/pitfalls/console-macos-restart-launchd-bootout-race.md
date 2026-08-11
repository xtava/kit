# macOS Console restart races launchd removal

## TLDR

`launchctl bootout` returning successfully does not prove the per-user Console service has finished
unregistering. Bootstrapping immediately can attach to the draining registration and leave the Mac
agent stopped. The macOS service owner must wait for `launchctl print` to report the service absent
before bootstrap.

## Signal

- `kit --json console restart <mac>` completes, but the following status is stopped or unavailable.
- The launch-agent plist exists and is valid.
- Re-running setup later succeeds without changing the plist or binary.
- The failure is timing-sensitive and is more visible during forced replacement.

## Discovery

The decisive check was to query launchd immediately after `bootout`. The command had returned, but
`launchctl print gui/<uid>/<label>` could still see the old registration. An immediate bootstrap was
therefore not evidence that a new agent had started.

This distinguished a launchd lifecycle race from Tailscale authentication, plist contents, binary
installation, socket cleanup, and terminal-session recovery.

## Root Cause

The old restart path treated successful `bootout` process exit as completion of asynchronous
launchd state removal. Both stop and restart need the same native unregistration invariant.

The owner is `src/tools/console/service/macos.rs`. The remote client and Control Center must not add
their own sleep, retry, or second restart sequence.

## Fix

macOS stop and restart now share `bootout_if_registered`. After `bootout`, it checks
`launchctl print` at a short bounded interval until the service is absent, then restart proceeds to
bootstrap. Timeout remains an explicit service error.

A fixed sleep is not an acceptable replacement: it is either unnecessarily slow or still races
under load.

## Verify

From another tailnet machine:

```bash
kit --json console restart <mac> --force
kit --json console status <mac>
```

The confirmed live result was `state: ready`, `platform: macos-launch-agent`, and `sessions: 0`
immediately after forced restart.

See [Console](../console.md) for lifecycle ownership and force semantics.
