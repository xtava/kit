# Operational pitfalls

These entries record confirmed failures whose visible symptom points away from the real owner.

## Quick search

| Search phrase or signal | Pitfall |
| --- | --- |
| `console restart` reports success but agent remains stopped; `launchctl bootout`; bootstrap race | [macOS Console restart races launchd removal](./console-macos-restart-launchd-bootout-race.md) |
| Tailscale SSH works but Mutagen is offline; upgrade did not fix Sync; missing `MUTAGEN_SSH_PATH` | [Mutagen daemon retained a non-hermetic SSH environment](./sync-mutagen-daemon-retains-old-ssh-environment.md) |
