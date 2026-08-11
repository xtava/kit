# Mutagen daemon retained a non-hermetic SSH environment

## TLDR

Setting `MUTAGEN_SSH_PATH` on new Mutagen client processes is insufficient when an older daemon for
Kit's data directory is already running. The daemon retains the environment from its original
launch. Kit performs a one-time locked daemon handoff before marking the private SSH transport
generation active.

## Signal

- Kit's direct Tailscale SSH probe succeeds, but a Synced Project remains offline or reports SSH
  authentication failure.
- Adding or removing personal SSH keys changes behavior, even though Kit should use Tailscale SSH.
- Installing the corrected Kit binary does not repair Sync until the existing Mutagen daemon is
  replaced.
- On Linux, the daemon environment contains `MUTAGEN_DATA_DIRECTORY` but no
  `MUTAGEN_SSH_PATH`.

## Discovery

The decisive evidence compared the environment of the already-running Mutagen daemon before and
after an upgraded `kit sync status`:

```bash
pid="$(pgrep -x mutagen | head -n 1)"
tr '\0' '\n' < "/proc/$pid/environ" |
  grep '^MUTAGEN_\(DATA_DIRECTORY\|SSH_PATH\)='
```

In the confirmed run, the original daemon had only Kit's data directory. After the handoff, the
replacement daemon also had Kit's private `MUTAGEN_SSH_PATH`. The project status command then
executed through the intended transport; endpoint offline state remained a separate reachability
fact.

## Root Cause

Mutagen owns a long-lived daemon. Its client can discover an existing daemon through the data
directory, but changing the client's environment does not mutate that daemon's process
environment. The direct SSH probe and Mutagen therefore used different authentication boundaries.

The permanent external-protocol adapter belongs in `src/tailscale/mutagen_ssh.rs`; Mutagen process
and daemon-generation ownership belongs in `src/tools/sync/engine.rs`. Do not patch this by enabling
personal keys, reading `~/.ssh/config`, or adding a second Sync-specific SSH policy.

## Fix

- Kit generates private `ssh` and `scp` launchers plus a deny-by-default OpenSSH configuration.
- Every Mutagen process receives Kit's private `MUTAGEN_DATA_DIRECTORY` and `MUTAGEN_SSH_PATH`.
- Before the private transport generation marker exists, one process takes the atomic transport
  lock, stops the pre-hermetic daemon, and atomically publishes the marker.
- The next Mutagen command starts the daemon with the private transport. Later clients observe the
  marker and do not repeatedly disrupt it.

The handoff preserves Mutagen's Kit-owned data directory and synchronization sessions.

## Verify

Run:

```bash
kit --json sync status
```

On Linux, inspect the resulting daemon environment using the discovery command above. It must
contain both `MUTAGEN_DATA_DIRECTORY` and `MUTAGEN_SSH_PATH`; the latter should point under Kit's
private Tailscale SSH state directory.

Then use:

```bash
kit sync doctor
kit --json sync status
```

Treat an `offline` endpoint after transport verification as peer reachability, not proof that SSH
keys should be re-enabled.

See [Synced Projects](../sync.md) for the full transport and lifecycle contract.
