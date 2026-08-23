# Kit release runbook

Read the live `docs/release.md` and `.github/workflows/release.yml` before acting. They override this
runbook if the release contract changes.

## Contract

- `Cargo.toml` is the version source of truth.
- Stable tags are immutable `vMAJOR.MINOR.PATCH` tags matching the Cargo version.
- Supported targets are Linux x86-64, Linux ARM64, macOS Intel, and macOS Apple Silicon.
- Each `kit-VERSION-TARGET.tar.gz` contains exactly one executable named `kit`.
- GitHub Releases is the sole remote release source; release assets contain no Windows binary.
- `workflow_dispatch` verifies and packages but intentionally skips publication. Only a valid tag
  push publishes.

## 1. Preflight locally

Confirm no competing Cargo build is running. Then execute sequentially:

```bash
cargo fmt --check
RUSTFLAGS='-D warnings' cargo test --locked -j 2
RUSTFLAGS='-D warnings' cargo clippy --locked --all-targets -j 2
actionlint .github/workflows/*.yml
cargo build --locked --release --target "$(rustc -vV | sed -n 's/^host: //p')" -j 2
```

The release build is an intentional heavy check. Inspect its archive staging logic and run a local
package smoke test: the archive contains only `kit`, extraction succeeds in an empty temporary
directory, and `kit --version` reports the Cargo version.

Repeat the source checks in a clean detached worktree at the exact commit that will be pushed.
Linux checks of Apple target metadata do not prove native linking or runtime behavior; the macOS
release runners remain required.

Preflight is complete when warning-fatal tests and lint, workflow syntax, the host release build,
package smoke test, and clean-commit proof all pass.

## 2. Verify the remote matrix once

Push the release commit to `master` only after normal applicable CI passes. Dispatch the Release
workflow once in manual mode. Watch that one run to completion and confirm all four build jobs
produce their archives. The absent `publish` job is expected for `workflow_dispatch`.

Use one foreground `gh run watch <id> --exit-status`. If the terminal yields a running session,
perform one long empty wait; after a timeout, report the still-running state and wait for the user to
request another check. Diagnose a failed run completely and reproduce it locally before another
push or dispatch.

Remote verification is complete when validation and all four target builds are green in one manual
run.

## 3. Publish once

1. Advance `Cargo.toml` and refresh `Cargo.lock`.
2. Commit and push the release commit; verify its normal CI.
3. Confirm the manual release matrix is green for that commit.
4. Create one annotated tag matching the Cargo version:

   ```bash
   git tag -a vX.Y.Z -m "Release X.Y.Z"
   git push origin vX.Y.Z
   ```

5. Watch the single tag-triggered workflow through its `publish` job.

If publication fails, fix source or workflow, increment the version, and publish a new tag. Keep
the failed or published tag immutable. Avoid manually constructing a GitHub Release around a failed
workflow.

Publication is complete when the tag-triggered workflow, including `publish`, succeeds.

## 4. Verify the public channel

Use `gh release view vX.Y.Z` and the public release endpoint to prove:

- the release is latest, public, non-draft, and non-prerelease;
- exactly four correctly named target archives exist and no Windows asset exists;
- every archive contains exactly one `kit` executable;
- a public download into an empty temporary directory matches GitHub's published SHA-256 digest;
- the extracted binary reports `kit X.Y.Z`;
- the extracted binary can run `kit update` without a repository, Git, Cargo, or a Rust toolchain.

Beginning with the second stable release, prove an installed previous version updates successfully
on at least one Linux host and one macOS host.

The release is complete only after these public, repo-free checks pass.
