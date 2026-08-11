# Canonical documentation

These documents define Kit's settled subsystem ownership, operating invariants, and verification
paths. Plans, audits, and exploratory notes are not canonical.

## Subsystems

- [Action contributions](./action-contributions.md) — one typed action projected through every
  interaction surface.
- [Console](./console.md) — remote access, service lifecycle, recovery, update, and forced restart.
- [Source sync](./sync.md) — persistent source projects backed by Mutagen over Tailscale SSH.
- [One-way source deployment](./source-sync-runbook.md) — reviewed Linux-to-macOS source transfer
  for native verification and installation.
- [Stats headless verification](./stats-headless-verification.md) — canonical non-interactive Stats
  acceptance path.

## Operational pitfalls

See the [pitfall index](./pitfalls/README.md) for confirmed failures whose symptoms point at the
wrong owner.
