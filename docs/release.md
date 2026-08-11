# Releases and updates

Kit ships as one self-updating binary through GitHub Releases.

## Contract

- `Cargo.toml` is the version source of truth.
- Stable release tags use `vMAJOR.MINOR.PATCH` and must equal the Cargo package version.
- A tag creates one GitHub Release with generated release notes and one archive per supported target.
- Every archive contains exactly one `kit` executable.
- GitHub's release-asset SHA-256 digest is verified before an executable is installed.
- `kit update` downloads the newest compatible release and atomically replaces the running binary.
- The updater never requires a source checkout, Git, Cargo, or a Rust toolchain.

The supported release targets are:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

Archives are named `kit-VERSION-TARGET.tar.gz`. The target triple is part of the filename so the
updater can select exactly one compatible asset. Unsupported targets fail explicitly instead of
installing a guessed binary.

## Install without a source checkout

First installation comes from the latest stable
[GitHub Release](https://github.com/xtava/kit/releases/latest), not from a repository clone. Choose
the archive matching the machine:

| Machine | Target |
| --- | --- |
| Linux x86-64 | `x86_64-unknown-linux-gnu` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` |
| macOS Intel | `x86_64-apple-darwin` |
| macOS Apple Silicon | `aarch64-apple-darwin` |

Download `kit-VERSION-TARGET.tar.gz`, compare its SHA-256 checksum with the digest shown for that
asset on the release page, and extract the single `kit` executable into a directory on `PATH`:

```bash
mkdir -p "$HOME/.local/bin"
tar -xzf kit-VERSION-TARGET.tar.gz -C "$HOME/.local/bin"
"$HOME/.local/bin/kit" --version
```

Use `sha256sum` on Linux or `shasum -a 256` on macOS. After the first installation, the normal
upgrade path is always:

```bash
kit update
```

That command resolves the compatible asset from the latest stable release, verifies GitHub's
published SHA-256 digest, and atomically replaces the current executable. It does not use a local
checkout, Git, Cargo, or a Rust toolchain.

## Published baseline

[`v0.1.0`](https://github.com/xtava/kit/releases/tag/v0.1.0) established the stable release channel
on 2026-07-19 from commit `c5d2766`. Its tag-triggered workflow published all four supported
archives, and a fresh Linux x86-64 download was checksum-verified, extracted, executed, and used to
query `kit update` successfully without a source checkout. The next release must advance the Cargo
version and publish a new immutable tag; `v0.1.0` must never be moved or reused.

## Update notification

Interactive Kit launches read cached release metadata without waiting for the network. When the
cache is older than 20 hours, Kit refreshes it in the background. A newer cached release opens a
startup prompt with three choices:

- **Update now** — run the same verified replacement used by `kit update`.
- **Later** — continue and show the release again on a future launch.
- **Skip this version** — suppress this exact version; a newer release appears normally.

Update prompts never run for `--json`, piped input/output, help/version requests, or `kit update`.
Network and cache failures never block the requested Kit command.

## Publishing

1. Update the version in `Cargo.toml` and refresh `Cargo.lock`.
2. Merge the release commit to `master` after normal CI succeeds.
3. Create and push an annotated tag:

   ```bash
   git tag -a v0.2.0 -m "Release 0.2.0"
   git push origin v0.2.0
   ```

4. The release workflow validates the tag/version pair, builds every target, publishes the GitHub
   Release, and marks the stable release as latest.
5. Verify the release assets and run `kit update` from the previous version on at least one Linux
   host and one macOS host.

`workflow_dispatch` is a verification mode. It runs validation, builds, packages, and artifact
uploads, but intentionally skips the `publish` job. Only a valid version-tag push creates the public
GitHub Release. Do not treat a successful manual run as a published release.

## Post-publish verification

A release is complete only when all of the following are true:

1. The tag-triggered Release workflow succeeded, including its `publish` job.
2. The public release is neither a draft nor a prerelease and is selected by GitHub as `latest`.
3. Exactly one archive exists for each supported target, with no Windows asset or guessed alias.
4. Each archive contains exactly one executable named `kit`.
5. A public asset can be downloaded into an empty temporary directory, its SHA-256 digest matches
   GitHub's published digest, and the extracted binary reports the tagged version.
6. The extracted binary can query the public release channel with `kit update` without a checkout.
7. Beginning with the second stable release, an installed previous version successfully updates on
   at least one Linux host and one macOS host.

Keep verification and publication separate: use a manual run to prove the matrix before tagging,
then use exactly one tag-triggered run to publish. If the tag-triggered run fails, fix the source or
workflow, bump the version, and publish a new tag; never move or reuse a published version tag.

Do not create releases manually around a failed workflow. Fix the workflow or release commit and
publish a new version so a tag, Cargo version, release, and asset set always describe one build.

## Ownership

- `src/tools/update.rs` owns release lookup, cached metadata, notification policy, and installation.
- `src/main.rs` invokes the update startup check before normal command dispatch.
- `.github/workflows/release.yml` owns tag validation, target builds, archives, and publication.
- GitHub Releases is the sole remote release source.

The former `CARGO_MANIFEST_DIR` → `git pull` → `cargo install` updater is intentionally deleted. A
local development build may still be installed with the repository's `install.sh`, but once running,
`kit update` follows the same published-binary path as every other installation.
