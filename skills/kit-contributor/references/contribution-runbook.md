# Kit contribution runbook

## 1. Establish a safe baseline

1. Resolve the repository root and read all applicable `AGENTS.md` files plus `CONTRIBUTING.md`.
2. Inspect `git status --short`, staged diff, unstaged diff, and relevant untracked files. Treat
   pre-existing changes as user-owned.
3. Identify the source owner, callers, tests, docs, and workflow path filters for the requested
   surface.
4. Inspect the process table before a heavy Cargo operation. Wait when Cargo or rustc is already
   building for the user or another agent.

The baseline is ready when the task-owned slice is distinguishable from unrelated work and no
competing build is running.

## 2. Design the smallest canonical change

Write down the behavioral invariant, the owner, the consumer list, and the cheapest proof. Prefer:

- extending a current typed API over adding parallel state or wrappers;
- moving a clear owner over adding a cache, override map, or compatibility lane;
- pure transformations plus thin I/O shells;
- one clean cutover when an owner or API changes;
- a local implementation for one caller until a shared invariant is real.

For TUI work, inventory action, keyboard, mouse, focus, navigation history, resize, context-menu,
settings, empty/loading/error, and non-interactive behavior before editing.

The design is ready when every affected path has one owner and the planned test observes that owner.

## 3. Implement

- Use `rg` and `rg --files` for discovery.
- Read each file before editing and preserve its surrounding style.
- Use the standard library and existing dependencies first. Add a crate only after checking
  maintenance, stability, transitive cost, and Linux/macOS behavior.
- Keep public APIs documented where their types or invariants carry meaning.
- Add focused tests alongside pure engine code and cross-boundary tests under `tests/` when needed.
- Remove superseded code, tests, imports, configuration, and dependencies in the same change.

Implementation is ready for verification when residue searches find no superseded owner and the
scoped diff contains only intentional work.

## 4. Verify sequentially

Run only the commands justified by the change. Never overlap heavy Cargo work.

1. Format proof:

   ```bash
   cargo fmt --check
   ```

2. Cheapest compile proof:

   ```bash
   RUSTFLAGS='-D warnings' cargo check --locked -j 2
   ```

3. Focused behavioral proof:

   ```bash
   RUSTFLAGS='-D warnings' cargo test --locked -j 2 <test-or-module-filter>
   ```

4. Lint after focused checks pass:

   ```bash
   RUSTFLAGS='-D warnings' cargo clippy --locked --all-targets -j 2
   ```

5. Run the full suite once near handoff only when the surface warrants it:

   ```bash
   RUSTFLAGS='-D warnings' cargo test --locked -j 2
   ```

Use `docs/canonical/stats-headless-verification.md` for Stats. Interactive Stats, PTY, terminal
window, ignored benchmark, release build, and sampling gates are opt-in. For CDP daemon changes,
exercise a live command when relevant and detach stale daemons with `kit cdp detach --all` first.

If a heavy command is interrupted, confirm its Cargo/rustc process tree exited before starting the
next one. Install once, after checks pass, only when requested or necessary. Use the root installer
so its bounded-job and lockfile policy stays canonical:

```bash
./install.sh
```

Verification is complete when the cheapest owner-level proof and every applicable boundary proof
are green with warnings denied.

## 5. Reproduce CI before pushing

Read the workflow file and its path filters; do not guess the matrix. At minimum:

- `.github/workflows/macos.yml` runs warning-fatal Stats tests on macOS.
- `.github/workflows/connectivity.yml` runs process fixtures, shared Tailscale, Tail, Console,
  external-opener, and live Console tests on Linux and macOS.
- `.github/workflows/release.yml` builds the four supported release targets.

Validate workflow syntax with `actionlint` when workflows change. For CI-sensitive work, prove the
commit from a clean detached worktree so ignored build artifacts and unrelated local changes cannot
mask a failure. A Linux cross-check of Apple target metadata is useful but does not prove native
linking, SDK integration, terminal behavior, or runtime behavior; keep real macOS CI as the native
boundary.

Push only after local warning-fatal checks and the clean-checkout proof pass. Use one foreground
`gh run watch <id> --exit-status`. If it returns a running session, perform one long empty wait.
When that wait expires, report that the workflow is still running and check again only when the user
asks. Avoid repeated dispatches or fix-push cycles based on partial logs.

CI reproduction is complete when the exact applicable commands pass from committed source and each
unavailable platform proof is named.

## 6. Hand off or publish

- Review path-scoped status and diffs.
- Report exact commands, results, and skipped proofs.
- Commit only the task-owned slice when requested; preserve pre-existing staged and unstaged work.
- Use `<scope>: <imperative summary>` subjects.
- PRs explain the owner boundary, shared capability, deleted duplicate path, verification, and TUI
  evidence where applicable.

For release publication, follow `release-runbook.md` only after contribution verification is green.
