# Source updates and package verification

Kit installs and updates from one registered canonical source checkout. GitHub Releases are not the
runtime update channel, and the repository's GitHub Actions workflow does not publish a release.

## Install and register the source

Clone the canonical repository and run its installer:

```bash
git clone https://github.com/xtava/kit.git
cd kit
./install.sh
```

`./install.sh` builds the checkout, replaces `$HOME/.local/bin/kit`, and asks the newly installed
binary to register that checkout, its current branch, and its configured upstream. Registration
accepts only a canonical Kit Git worktree with the expected `xtava/kit` remote. It does not persist
remote credentials.

An installation made before source registration was introduced cannot guess which checkout should
own updates. Run `./install.sh` once from the canonical checkout to register it. A missing or invalid
registration makes `kit update` fail with recovery guidance; there is no release-download fallback.

This update model requires Git, Cargo, and the Rust toolchain used by `install.sh`.

## Update contract

Run:

```bash
kit update
```

The updater performs one supervised transaction:

1. Load and revalidate the registered checkout, branch, upstream remote, and upstream ref.
2. Refuse before replacement when the local Console agent reports active sessions.
3. Fetch the exact registered upstream ref without tags, submodules, or interactive credential
   prompts, then resolve the fetched commit to an immutable revision.
4. Classify the checkout as current, behind, locally ahead, or diverged.
5. When behind, reject any incoming path that overlaps staged, unstaged, or untracked local work,
   then fast-forward to the exact fetched revision.
6. Run the registered checkout's absolute `install.sh`, which replaces
   `$HOME/.local/bin/kit`.
7. Verify that the managed executable reports the selected source revision.
8. With zero Console sessions, invoke the canonical non-forced Console restart so the running agent
   uses the replacement binary.

Current and locally ahead checkouts are installed from their existing revision after the upstream
fetch. Diverged history is refused. The updater never stashes, resets, rebases, force-switches a
branch, discards local commits, or guesses another checkout. A fast-forward must preserve the
checkout's exact staged, unstaged, and untracked status.

If installation succeeds but post-install identity verification or Console restart fails, the
error identifies that the managed binary has already been replaced. Console restart remains
non-forced: a session that appears after preflight causes restart to refuse instead of terminating
it.

## Cross-target package verification

[`.github/workflows/release.yml`](../.github/workflows/release.yml) is a manual
`workflow_dispatch` check. It builds and packages one `kit` executable for each supported target:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

Each job uploads `kit-VERSION-TARGET.tar.gz` as a workflow artifact. The Cargo package version is
used only to name the verification archive. The workflow does not create a tag, validate a tag,
publish a GitHub Release, mark anything as latest, or supply binaries to `kit update`.

Existing GitHub Releases are historical artifacts. Runtime update code does not query them, and a
green cross-target workflow run is not a published release.

## Verification

Before dispatching the cross-target workflow, run the applicable local checks sequentially and
confirm no competing Cargo build is active:

```bash
cargo fmt --check
RUSTFLAGS='-D warnings' cargo test --locked -j 2
RUSTFLAGS='-D warnings' cargo clippy --locked --all-targets -j 2
actionlint .github/workflows/*.yml
```

For every uploaded archive, verify that it contains exactly one executable named `kit`, extraction
succeeds in an empty temporary directory, and the extracted executable reports the expected
version. Native runners are required for native linking and runtime proof; Linux inspection of an
Apple target's metadata is not a macOS runtime check.

Updater verification is separate from archive verification. On an authorized machine, prove that
`./install.sh` registers the checkout, `kit update` fetches the registered upstream, the installed
binary identifies the selected revision, dirty state survives unchanged, and Console replacement
refuses when sessions are active.

## Ownership

- `install.sh` owns the canonical build, managed-path replacement, and source-registration handoff.
- `src/update.rs` owns registration validation and the source-update transaction.
- `src/tools/update.rs` exposes that owner through the CLI.
- `src/tools/console/service/mod.rs` owns the non-forced Console restart used after installation.
- `.github/workflows/release.yml` owns manual cross-target package verification only.

Do not add a GitHub-release downloader, startup update prompt, second installation path, automatic
stash/reset/rebase policy, or updater-specific Console lifecycle implementation.
