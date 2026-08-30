# One-way Linux-to-macOS source verification

This runbook copies an explicitly reviewed slice of a dirty Kit checkout to one macOS peer for
native verification. It is not a repository mirror and it is not `kit sync`: Linux remains the
authoritative source, Git is never run on the peer, and only named files and deletions cross the
boundary.

Remote writes, installation, and service activation require separate approval. Stop before the
“Apply” section until the exact peer, root, file manifest, deletion manifest, and commands have been
approved.

## TLDR

Freeze a literal file and deletion manifest, resolve exactly one online peer by stable Tailscale
node ID, transfer into a private remote stage over Kit's deny-by-default SSH policy, verify every
file digest, publish only the manifest, run one native check, and install once when authorized.

OpenSSH constructs one remote shell command from everything after the destination. It does not
preserve local argument boundaries. Pass one complete remote command string or one stdin script;
do not rely on `ssh host sh -c '...' arg` to preserve `$1`.

Run the local steps in one Bash session so arrays, variables, failure propagation, and stage cleanup
remain active:

```bash
set -euo pipefail
cd "$HOME/src/kit"
```

## Contract

- Select the peer by stable Tailscale node identity and re-resolve its current Tailscale IP.
- Use Tailscale SSH authentication only. OpenSSH reads no user or system configuration and offers
  no key, agent, password, keyboard-interactive, GSSAPI, or host-based credential.
- Copy only `sync-files.txt`; delete only `sync-deletions.txt`.
- Reject absolute paths, `..`, `.git`, `target`, dependency trees, caches, and newline-containing
  names.
- Stage files privately, verify their manifest, and only then publish them under the reviewed
  remote root.
- Install at `$HOME/.local/bin/kit`. This Git-free peer workflow cannot use `kit update`, which
  requires the canonical Git checkout registered by `./install.sh`; continue to deploy only the
  explicitly reviewed manifest.

## 1. Freeze the reviewed manifest

From the authoritative Kit checkout:

```bash
git status --short
git diff --stat
${EDITOR:-vi} sync-files.txt
${EDITOR:-vi} sync-deletions.txt
```

Each manifest contains one repository-relative path per line. `sync-deletions.txt` may be empty.
Reject unsafe or implicit scope before continuing:

```bash
for manifest in sync-files.txt sync-deletions.txt; do
  test -f "$manifest"
  awk '
    !length || /^\// || /^-/ || /(^|\/)\.\.(\/|$)/ ||
    /(^|\/)(\.git|target|node_modules|vendor\/bundle)(\/|$)/ { bad=1 }
    END { exit bad }
  ' "$manifest"
done
test -s sync-files.txt
while IFS= read -r path; do test -f "$path"; done < sync-files.txt
sort -u sync-files.txt | cmp -s - sync-files.txt
sort -u sync-deletions.txt | cmp -s - sync-deletions.txt
test -z "$(comm -12 sync-files.txt sync-deletions.txt)"
sha256sum sync-files.txt sync-deletions.txt
while IFS= read -r path; do sha256sum "$path"; done < sync-files.txt > sync-before.sha256
```

Review `sync-before.sha256` and retain it with the verification evidence. Do not add either
manifest or the evidence files to the transfer manifest.

## 2. Resolve one Tailscale identity

Set the reviewed values. `KIT_REMOTE_NODE_ID` is the stable `ID` from `tailscale status --json`,
not a hostname:

```bash
KIT_REMOTE_NODE_ID='REVIEWED-STABLE-NODE-ID'
KIT_REMOTE_SELECTOR='REVIEWED-MACHINE-SELECTOR'
KIT_REMOTE_USER='REVIEWED-UNIX-USER'
KIT_REMOTE_ROOT='/REVIEWED/ABSOLUTE/PATH/kit'
KIT_KNOWN_HOSTS="${XDG_STATE_HOME:-$HOME/.local/state}/kit/tailscale-ssh/known_hosts"
case "$KIT_REMOTE_NODE_ID" in *[!A-Za-z0-9_-]*|'') exit 1 ;; esac
case "$KIT_REMOTE_SELECTOR" in *[!A-Za-z0-9._-]*|'') exit 1 ;; esac
case "$KIT_REMOTE_USER" in *[!A-Za-z0-9._-]*|'') exit 1 ;; esac
case "$KIT_REMOTE_ROOT" in *[!A-Za-z0-9._/-]*|'') exit 1 ;; esac
install -d -m 700 "$(dirname "$KIT_KNOWN_HOSTS")"
if test ! -e "$KIT_KNOWN_HOSTS"; then
  install -m 600 /dev/null "$KIT_KNOWN_HOSTS"
fi
KIT_REMOTE_IP="$(
  tailscale status --json |
    jq -er --arg id "$KIT_REMOTE_NODE_ID" '
      [.Peer[] | select(.ID == $id and .Online == true) | .TailscaleIPs[0]]
      | if length == 1 then .[0] else error("peer is absent, offline, or ambiguous") end
    '
)"
```

Use this exact OpenSSH policy for every probe and transfer:

```bash
KIT_SSH=(
  -F none -T
  -o RequestTTY=no
  -o ForwardAgent=no
  -o IdentityAgent=none
  -o IdentityFile=none
  -o IdentitiesOnly=yes
  -o PubkeyAuthentication=no
  -o PasswordAuthentication=no
  -o KbdInteractiveAuthentication=no
  -o GSSAPIAuthentication=no
  -o HostbasedAuthentication=no
  -o BatchMode=yes
  -o ClearAllForwardings=yes
  -o PermitLocalCommand=no
  -o ControlMaster=no
  -o ControlPath=none
  -o ProxyCommand=none
  -o ProxyJump=none
  -o GlobalKnownHostsFile=none
  -o "UserKnownHostsFile=$KIT_KNOWN_HOSTS"
  -o UpdateHostKeys=no
  -o VerifyHostKeyDNS=no
  -o StrictHostKeyChecking=accept-new
  -o "HostKeyAlias=kit-node-$KIT_REMOTE_NODE_ID"
  -o ConnectTimeout=10
)
ssh "${KIT_SSH[@]}" -- "$KIT_REMOTE_USER@$KIT_REMOTE_IP" \
  'printf "remote-home=%s\n" "$HOME"'
```

If Tailscale prints a check-mode login URL, authenticate there and rerun the probe. Do not enable a
personal SSH key as a workaround. The conservative character checks above make the reviewed values
safe to embed in the single remote command strings used below. Stop instead of weakening those
checks for an unexpected path.

## 3. Capture the remote baseline

```bash
ssh "${KIT_SSH[@]}" -- "$KIT_REMOTE_USER@$KIT_REMOTE_IP" \
  "set -eu
   test -d '$KIT_REMOTE_ROOT'
   printf 'root=%s\\n' '$KIT_REMOTE_ROOT'
   test ! -e '$KIT_REMOTE_ROOT/.git/index.lock'"
```

Also record `git status --short` locally. If the peer root contains unrelated local edits that
overlap either manifest, stop and reconcile ownership before applying.

## 4. Apply only the approved slice

Create a private remote stage, transfer only the file manifest, verify the received hashes, publish
the named files, then apply only the deletion manifest:

```bash
KIT_REMOTE_STAGE="$(
  ssh "${KIT_SSH[@]}" -- "$KIT_REMOTE_USER@$KIT_REMOTE_IP" \
    'umask 077; mktemp -d "${TMPDIR:-/tmp}/kit-source-sync.XXXXXX"'
)"
case "$KIT_REMOTE_STAGE" in
  /tmp/kit-source-sync.*|/private/tmp/kit-source-sync.*) ;;
  *) exit 1 ;;
esac
cleanup_remote_stage() {
  ssh "${KIT_SSH[@]}" -- "$KIT_REMOTE_USER@$KIT_REMOTE_IP" \
    "rm -rf -- '$KIT_REMOTE_STAGE'"
}
trap cleanup_remote_stage EXIT
tar -cf - -T sync-files.txt |
  ssh "${KIT_SSH[@]}" -- "$KIT_REMOTE_USER@$KIT_REMOTE_IP" \
    "set -eu; tar -xf - -C '$KIT_REMOTE_STAGE'"
while IFS= read -r path; do sha256sum "$path"; done < sync-files.txt |
  ssh "${KIT_SSH[@]}" -- "$KIT_REMOTE_USER@$KIT_REMOTE_IP" \
    "set -eu; cd '$KIT_REMOTE_STAGE'; shasum -a 256 -c -"
tar -cf - sync-files.txt sync-deletions.txt |
  ssh "${KIT_SSH[@]}" -- "$KIT_REMOTE_USER@$KIT_REMOTE_IP" \
    "set -eu; tar -xf - -C '$KIT_REMOTE_STAGE'"
ssh "${KIT_SSH[@]}" -- "$KIT_REMOTE_USER@$KIT_REMOTE_IP" \
  "KIT_REMOTE_STAGE='$KIT_REMOTE_STAGE' KIT_REMOTE_ROOT='$KIT_REMOTE_ROOT' sh -s" <<'REMOTE_APPLY'
set -eu
stage=$KIT_REMOTE_STAGE
root=$KIT_REMOTE_ROOT
while IFS= read -r path; do
  test -f "$stage/$path"
  install -d -m 755 "$root/$(dirname "$path")"
  cp -p "$stage/$path" "$root/$path"
done < "$stage/sync-files.txt"
while IFS= read -r path; do
  test -n "$path"
  rm -f -- "$root/$path"
done < "$stage/sync-deletions.txt"
rm -rf -- "$stage"
REMOTE_APPLY
trap - EXIT
```

The `rm` operation above is destructive and is authorized only for the exact reviewed deletion
manifest and the validated private stage. Never substitute a root glob, `rsync --delete`, or a
generated Git diff. If transfer, verification, or publication fails, the trap removes that exact
stage before returning control.

## 5. Native verification and managed installation

Run one warning-fatal native check first:

```bash
ssh "${KIT_SSH[@]}" -- "$KIT_REMOTE_USER@$KIT_REMOTE_IP" \
  "KIT_REMOTE_ROOT='$KIT_REMOTE_ROOT' sh -s" <<'REMOTE_CHECK'
set -eu
root=$KIT_REMOTE_ROOT
cd "$root"
if pgrep -x cargo >/dev/null || pgrep -x rustc >/dev/null; then
  printf '%s\n' 'remote Cargo or rustc build already running' >&2
  exit 1
fi
RUSTFLAGS="-D warnings" cargo check --locked -j 2
REMOTE_CHECK
```

Only with explicit install approval:

```bash
ssh "${KIT_SSH[@]}" -- "$KIT_REMOTE_USER@$KIT_REMOTE_IP" \
  "KIT_REMOTE_ROOT='$KIT_REMOTE_ROOT' sh -s" <<'REMOTE_INSTALL'
set -eu
root=$KIT_REMOTE_ROOT
cd "$root"
cargo install --locked -j 2 --root "$HOME/.local" --path .
"$HOME/.local/bin/kit" --json console status
REMOTE_INSTALL
```

Do not restart or force-stop Console merely because a new binary was installed. From the
authoritative machine, inspect the selected peer:

```bash
kit --json console status "$KIT_REMOTE_SELECTOR"
```

- For an absent, stopped, or repairable service, run
  `kit --json console setup "$KIT_REMOTE_SELECTOR"`.
- For an agent change with zero sessions, run
  `kit --json console restart "$KIT_REMOTE_SELECTOR"`.
- If activation is deferred by live sessions, preserve the existing PID, socket, service
  definition, and sessions until they close normally.
- Use `--force` only when the operator explicitly authorizes terminating those sessions.

See [Console](./console.md) for the canonical lifecycle and macOS restart boundary.

## 6. Evidence and stop conditions

Record:

- stable node ID and resolved IP;
- both manifest digests and `sync-before.sha256`;
- native `cargo check` output;
- local and remote Kit build identities;
- Console status before and after any approved setup;
- service PID, socket identity, definition path, and session count when activation defers;
- whether a normal or forced restart was authorized and its final typed status.

Stop immediately on an ambiguous/offline peer, host-key mismatch, authentication method other than
Tailscale check mode, overlapping unreviewed remote edits, hash mismatch, native warning/error, or
active-session replacement request. `kit sync doctor` diagnoses persistent Sync projects only; it
does not validate this one-way acceptance manifest.
