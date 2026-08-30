# Kit cross-target package verification runbook

Read the live `docs/release.md`, `install.sh`, and `.github/workflows/release.yml` before acting.
They override this runbook if the update or verification contract changes.

## Contract

- `./install.sh` is the canonical installer and registers its canonical Git checkout for updates.
- `kit update` fetches that checkout's exact configured upstream and installs source; it does not
  download a GitHub Release.
- Upstream movement is fast-forward-only. Never stash, reset, rebase, force-switch, or discard
  local work to make an update proceed.
- Active Console sessions refuse replacement. A successful install with zero sessions uses the
  canonical non-forced Console restart.
- `.github/workflows/release.yml` is a manual four-target package-verification workflow. It does not
  publish, tag, or update a GitHub Release.

## 1. Preflight locally

Confirm no competing Cargo or rustc build is running. Then execute the applicable checks
sequentially:

```bash
cargo fmt --check
RUSTFLAGS='-D warnings' cargo test --locked -j 2
RUSTFLAGS='-D warnings' cargo clippy --locked --all-targets -j 2
actionlint .github/workflows/*.yml
cargo build --locked --release --target "$(rustc -vV | sed -n 's/^host: //p')" -j 2
```

The release-mode build is an intentional heavy check. Run it only after the cheaper checks pass.
Stage a local archive with the same shape as the workflow and prove that it contains exactly one
`kit` executable, extracts into an empty temporary directory, and reports the Cargo version.

Repeat source checks in a clean detached worktree at the exact commit being verified. Linux checks
of Apple target metadata do not prove native linking or runtime behavior; the macOS runners remain
required.

## 2. Verify the remote matrix once

Push the verified commit only after the repository's normal applicable CI passes. Dispatch the
`Cross-target package verification` workflow once in manual mode and watch that run to completion.
Confirm all four jobs build and upload their target archive:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

Use one foreground `gh run watch <id> --exit-status`. If it remains running after the available
wait, report that state rather than dispatching a duplicate run. Diagnose a failed job completely
and reproduce it locally before another push or dispatch.

A green run proves only cross-target build and packaging. There is no publication job, and its
artifacts are not an update channel.

## 3. Inspect the artifacts

Download each workflow artifact into an empty temporary directory and prove:

- exactly four correctly named target archives exist and no Windows archive exists;
- every archive contains exactly one executable named `kit`;
- archive extraction succeeds without path traversal or extra files;
- the executable reports the expected Cargo version on a compatible native runner.

Do not create a GitHub Release around the workflow artifacts or describe the run as a release.

## 4. Verify source updating separately

On an authorized machine with the canonical checkout and no active Console sessions:

1. Capture the checkout revision, raw Git status, and installed executable identity.
2. Run `./install.sh` once and confirm it registers that checkout.
3. Run `kit update` against a controlled current or fast-forward upstream state.
4. Confirm the fetched revision, final checkout revision, installed source identity, and managed
   `$HOME/.local/bin/kit` path agree.
5. Confirm staged, unstaged, and untracked state is byte-for-byte unchanged.
6. Confirm the Console agent uses the replacement binary after the non-forced restart.

Also prove the refusal boundaries with isolated fixtures: missing registration, noncanonical
remote, divergence, dirty/incoming path overlap, failed fetch, and active Console sessions must not
invoke installation. A session race during restart must refuse without force and report that the
binary was already replaced.

An older installation without source registration is repaired only by rerunning `./install.sh`
from the canonical checkout. Do not add a release-download fallback or guess a checkout from the
caller's current directory.
