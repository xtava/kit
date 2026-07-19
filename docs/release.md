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
