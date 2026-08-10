# Tsgo Language Service Goal

## Outcome

Design, obtain approval for, implement, and prove a concise Kit-managed service that keeps the
TypeScript 7 native `tsgo --lsp --stdio` process warm per canonical workspace and exact server
version. Users must be able to query and fully manage the service only through Kit; target
repositories remain unmodified.

## Baseline

- Known native server: `/home/tvx/Desktop/projects/modular/node_modules/.bin/tsgo`.
- Known invocation: `tsgo --lsp --stdio`.
- Native call-hierarchy requests work, but starting a fresh server for every query causes a large
  CPU spike.
- `/home/tvx/Desktop/projects/modular` is the required live verification workspace. Preserve all
  existing tracked, staged, unstaged, and untracked content; only a uniquely named temporary
  untracked verification fixture may be created and removed by the verifier.
- Kit's worktree was already extensively dirty before this goal. Every unrelated staged,
  unstaged, deleted, and untracked path is protected.

## Stable constraints

- Research precedes design; three Codex 5.6 Luna agents at medium effort own the three requested
  research reports and may not implement production code.
- The primary agent verifies the report files and evidence, retries failed lanes, synthesizes the
  proposal, and owns all implementation after approval.
- Do not add or run tests and do not create a RED phase. Use compile proof plus the real installed
  Kit command path against Modular.
- Keep scope narrow: reuse existing Kit owners and dependencies; do not create a generic LSP peer,
  refactor sibling tools, or expand beyond the management and call-hierarchy surface required here.
- Compare: one global `tsgo`; one Kit daemon plus `tsgo` per workspace; one broker managing
  workspace-scoped `tsgo` children.
- Reuse canonical Kit mechanics where they truly own the policy: `ProcessSupervisor`, detached
  process receipts, Unix sockets, repository/workspace location, and structured output.
- Preserve `tools/* -> framework | tui | cdp`; no tool imports another tool.
- Complete `docs/ideation/tsgo-language-service/proposal.html` and obtain explicit approval before
  editing production code.
- Leave one canonical path. Do not add aliases, shims, adapters, fallback owners, legacy command
  surfaces, or speculative shared abstractions.
- Do not run formatting or linting. Before Cargo work, check for active Cargo/rustc processes; use
  no more than one `cargo check -j 2` operation at a time.

## Non-goals

- Modifying Modular or any target TypeScript repository.
- A general-purpose editor, general LSP proxy framework, or support for unrelated language
  servers unless an existing Kit owner already supplies the exact reusable mechanism.
- Treating PID equality, unit tests, or a successful build as proof of warm reuse.
- Compatibility with an unapproved or superseded command vocabulary.

## Primary verifier

Exercise the real installed Kit command path against the Modular workspace and its native `tsgo`,
using a uniquely named temporary untracked fixture for the controlled live edit.
Capture structured service evidence for each step: daemon-owned instance identifier, daemon start
time, cumulative request count, and child identity. The verifier must demonstrate all of the
following without altering any pre-existing Modular content:

1. The first query starts the service.
2. A second query reuses the same service instance and `tsgo` child while increasing request count.
3. Editing a TypeScript file updates the result without restarting the child.
4. Two concurrent clients receive correctly correlated results.
5. Stop gracefully terminates and reaps the owned child.
6. A later query starts a clean replacement instance.
7. Stale registry/socket state recovers through ownership validation, without PID guessing.

## Supporting proof

- Direct live commands cover LSP framing/correlation, initialization, document synchronization,
  file changes, management, recovery, teardown, and prepare/incoming/outgoing call hierarchy.
- Run only bounded `cargo check -j 2` and root-install operations needed for the live command proof;
  any corrective refresh must be caused by and recorded against a task-owned live-verifier failure.
- Do not run unit/integration tests, RED tests, formatting, linting, or unrelated verification.
- Scoped diff/status inspection proves unrelated dirty work remains untouched.

## Anti-cheating rules

- Do not weaken or replace the real command-path verifier with mocks, PID-only assertions, or direct
  daemon/socket calls.
- Do not restart `tsgo` between the reuse, live-edit, or concurrent-client steps.
- Do not select a different executable/version silently; identity and output must expose the exact
  resolved executable and version.
- Do not leave stale state cleanup dependent on signaling an unverified PID.
- Do not claim a lifecycle state that was inferred only from registry files; query the owned
  service protocol where possible.

## Approval gates

1. Research artifacts may be created immediately.
2. The complete visual HTML proposal must be explicitly approved before any production-code edit.
3. Any architecture change after approval that alters ownership, public commands, scope, trust
   boundary, or completion proof requires renewed approval.

## Structural simplification stop gate

If research or implementation finds a duplicate owner, needless wrapper, compatibility lane, or a
better existing Kit primitive on the direct ownership path, pause before layering over it. Record
the evidence and comparison in `plan.md`; continue only when the cleanup is local and already
authorized, otherwise request approval.

## Blocker standard

Difficulty, a failed check, or an uncertain design is not a blocker. Record the failure, update the
route, and retry or investigate. Report a blocked goal only after the same external condition has
prevented meaningful progress for three consecutive goal turns and no safe in-scope action remains.

## Completion proof

The goal completes only when:

- every phase in [`plan.md`](./plan.md) is complete;
- the proposal was approved before production edits;
- the real installed Kit command path on Modular produces saved evidence satisfying every
  primary-verifier step;
- management, recovery, teardown, warm reuse, live updates, and concurrent correlation are proven;
- exact command outputs and scoped status/diff evidence are recorded in the plan; and
- no required work or unresolved high-confidence blocker remains.
