# Compatible Console builds were rejected by exact build identity

## TLDR

A healthy Console agent and SSH relay could be rejected because the client and agent came from
different source revisions or had different dirty-build bits. Exact build identity is not wire
compatibility. The codec version is the hard admission gate; the agent build remains visible as
diagnostic evidence.

## Signal

- `kit console <machine>` ends with `Error: the initial Console relay preflight failed`.
- Typed status reports an expected and actual build identity instead of a codec mismatch.
- An older Kit executable can connect successfully to the same agent.
- The remote service is ready and owns active sessions, making replacement unnecessary or unsafe.

## Discovery

The decisive comparison used the same target and transport with two local executables. The older
client matching the agent build connected, while the newer client reached the same healthy agent
and failed only at exact build-identity comparison. Source inspection then separated
`GetCodecVersion` from `GetBuildIdentity` in
`vendor/wezterm/wezterm-client/src/client.rs`.

That evidence ruled out Tailscale reachability, SSH authentication, service state, socket state,
and session corruption.

## Root Cause

The vendored client bootstrap compared the full server build identity with a caller-supplied local
identity before registration. Console remote status repeated that equality policy. Source revision
and dirty state were therefore mistaken for protocol capability.

The compatibility owner is the client bootstrap codec check. Remote status and the Control Center
must present compatibility facts; they must not invent a stricter build-equality gate.

## Fix

Bootstrap now rejects only a codec version mismatch, captures the server build identity, completes
registration, and exposes the current identity to Console status. The obsolete build-incompatible
status and update-before-connect recovery path were removed. Initial relay failure also preserves
the typed preflight status rather than replacing it with a generic message.

## Verify

Connect a newly built client to a codec-compatible agent from a different source build:

```bash
kit --json console status <machine>
kit console <machine>
kit --json console status <machine>
```

Status must remain `ready`, report the agent's actual build identity, and preserve existing
sessions. A real codec mismatch must still return `codec-incompatible`.

See [Console](../console.md) and
[ADR 001](../../../sessions/console-connectivity-20260811-01/adr/001-console-compatibility-boundary.md).
