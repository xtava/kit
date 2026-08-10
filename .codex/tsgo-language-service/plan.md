# Tsgo Language Service Operating Plan

Goal: [`goal.md`](./goal.md)

## TLDR

- **Approved outcome:** Implement the narrow Kit-managed warm `tsgo` service and verify it live on Modular without tests.
- **Authoritative architecture:** `docs/ideation/tsgo-language-service/proposal.html`, revision 2.
- **Current phase:** Phase 6, completion audit and handoff.
- **Current status:** completed.
- **Last completed proof:** Three accepted installed-command measurements followed an exact 11-edge Modular call hierarchy and quantified restart, lazy, and already-hot service modes.
- **Next action:** Hand off the call-chain performance baseline and its scope.
- **Active blocker or stop gate:** None.
- **Remaining approval:** Only an architecture-changing deviation or action outside the approved source/build/live-proof scope.
- **Primary completion verifier:** Seven-step real Kit command path proving warm reuse, live edits, correlation, teardown, replacement, and stale-state recovery.

## Current State

- **Active phase:** Phase 6, completed.
- **Last completed phase:** Phase 6; every runtime, source, scope, artifact, and cleanup check passed.
- **Working-tree boundary:** Preserve the extensive pre-existing Kit dirty tree. Own only the new `src/tools/tsgo/**` files plus exact registration hunks in `src/tools/mod.rs` and `src/main.rs`; preserve all existing Modular content except one uniquely named temporary untracked verifier fixture.
- **Source baselines:** Kit current dirty checkout; native preview `7.0.0-dev.20260418.1`, package git revision `1de8d68230f8759af6fb71d9cf0f9c37d2c65507`.
- **Known runtime or environment state:** Runtime registry/socket/process state is empty. The temporary performance fixture and disposable measurement harness are removed. Installed stop JSON explicitly projects graceful/reaped evidence.
- **Open finding:** The call-chain baseline proves a median 12.2× wall-time and 21.6× combined-CPU improvement for an already-hot 11-edge traversal versus restarting at every hop. The implementation uses one daemon-owned request actor to serialize native LSP traffic while accepting concurrent Kit socket clients; correlation remains explicit through monotonic LSP and client request IDs.
- **Plan drift:** Revision 2 removes test work and moves acceptance from a disposable repository to a temporary fixture inside Modular; architecture and public scope remain unchanged.

## Restart Protocol

Start with no assumed conversation memory:

1. Read `goal.md`, this document, Kit `AGENTS.md`, `CONTRIBUTING.md`, and the applicable Kit contributor references.
2. Read the proposal and the three files under `.codex/tsgo-language-service/research/`.
3. Inspect `git status --short --branch`; treat every unrelated change as protected.
4. Confirm the three reports remain non-empty and contain `## Decision capsule` before relying on synthesis.
5. Do not repeat the completed research fleet, identity probe, proposal coverage audit, or Modular sample-file probe.
6. Do not add or run tests, formatting, linting, or unrelated refactors.
7. Execute only the Next Action unless new evidence triggers the structural stop gate or re-planning gate.
8. Update Current State, phase Evidence, and the append-only worklog after any material finding, failed verifier, steering, or phase transition.

## Next Action

Report the operation boundary and measured full-chain result. No runtime or temporary artifact remains.

## Approval Ledger

| Gate | Exact scope | State | Evidence supplied | User response or date |
| --- | --- | --- | --- | --- |
| research | Three Luna medium research lanes | approved | Three agent-owned reports | User request, 2026-08-10 |
| architecture | Proposal revision 2: same owner/public surface; no tests; live Modular proof | approved | Complete HTML plus user steering | User: “properly use $kit-contributor… verify it works on the modular repo”, 2026-08-10 |
| implementation | New tool-local files and exact registration hunks | approved | Reuse-first owner map and revision 2 | Same user steering, 2026-08-10 |
| architecture-changing deviation | Owner, public surface, trust, scope, or verifier change | pending if triggered | New evidence and revised proposal | — |
| Cargo runnable artifact | One `cargo check -j 2` then one bounded root install | approved | Required for installed command proof | Same user steering, 2026-08-10 |
| goal activation | Exact objective in `goal.md` | approved | Active goal `019fed9f-27b0-7bf2-8c3e-ab28d049e89f` | Activated 2026-08-10 |

Never infer one gate from another.

## Evidence Rules

- Use `pending`, `in progress`, `blocked`, or `completed` as exact phase status values and keep at most one phase in progress.
- Agent responses are receipts only. Reports must exist, be non-empty, contain direct evidence, and pass read-back.
- Separate verified fact, observed runtime behavior, inference, proposal, and unresolved question.
- Mark a phase completed only after every exit criterion has reproducible evidence.
- Record exact source paths, lines, commands, outputs, artifacts, or runtime observations.
- Preserve all pre-existing Modular content; only one unique temporary untracked fixture may be created and removed for live-edit proof.
- Do not add or run tests or a RED phase. Do not format or lint. Before Cargo work, check for active Cargo/rustc; run one `cargo check -j 2`, then one bounded install.
- Run negative checks for rejected owners, aliases, fallbacks, PID authority, raw LSP access, and repository-local state after implementation.
- Re-enter planning before any unplanned owner, public command, fallback, dependency, compatibility lane, platform promise, or scope expansion.
- Trigger the structural simplification stop gate before building on a duplicate owner, wrapper, compatibility path, or misplaced lifecycle boundary.

## Phase 1: Evidence-backed research fleet

### Status

completed

### Context

Establish Kit owner truth, native `tsgo` protocol truth, and independent product/failure pressure before selecting an architecture or command vocabulary.

### Implementation

- [x] Kit process/daemon architecture report written by its assigned Luna medium agent.
- [x] Native `tsgo` LSP lifecycle/protocol report written by its assigned Luna medium agent.
- [x] Adversarial product/failure/scope report written by its assigned Luna medium agent.
- [x] Primary agent compared all three architectures and directly checked the cited Kit owners.

### Verification

- [x] Every exact report path exists, is non-empty, contains direct evidence, and was read back.
- [x] Every report contains `## Decision capsule`.
- [x] No chat summary was accepted as proof and no invalid report lane was silently skipped.
- [x] The protocol probe left its Modular sample file unchanged.

### Exit criteria

- [x] All three reports passed the artifact barrier.
- [x] Architecture decision inputs, vocabulary evidence, risks, and open questions support one proposal.

### Evidence

- `research/kit-architecture.md`: 12,743 bytes with current-source citations and architecture comparison.
- `research/tsgo-protocol.md`: 12,530 bytes with live capture and official Microsoft sources.
- `research/adversarial-review.md`: 14,760 bytes with fifteen structured failure findings.
- Direct source reads confirmed `RepositoryLocator`, `Output`, detached receipt inspection/stop, CDP concurrent Unix clients, CDP non-atomic ordinary record writes, and Console private path validation.

## Phase 2: Complete proposal and approval

### Status

completed

### Context

Convert research into one fully visible design. Planning and implementation remain separate authorizations; this phase must stop at the approval gate.

### Implementation

- [x] Write `docs/ideation/tsgo-language-service/proposal.html`.
- [x] Cover ownership, all three architectures, exact commands, resolution, identity, concurrency, protocols, lifecycle, sync, failure, security, JSON, proof, non-goals, and avoidance/deletion decisions.
- [x] Keep the proposal static, visual, source-grounded, and fully visible without collapsed or script-dependent sections.
- [x] Disposition adversarial findings in the target contracts and failure matrix.
- [x] Present the complete proposal and wait for explicit user approval.

### Verification

- [x] Fixed-string mechanical coverage passed every required design and verifier term.
- [x] HTML section structure balanced at 18 opening and 18 closing sections.
- [x] All three report artifacts still passed the evidence barrier after synthesis.
- [x] Strict operating-plan validation passed after the recorded structural reshape.

### Exit criteria

- [x] User explicitly authorizes implementation with the revision 2 constraints.
- [x] Approved owner, public vocabulary, protocol, lifecycle, trust, platform, and verification contracts are recorded in the Approval Ledger.

### Evidence

- Proposal revision 1 was 573 lines and 37,169 bytes before two explicit label fixes.
- `.gitignore:6` ignores `/docs/*` except canonical docs, so the requested ideation file is intentionally a local approval artifact and does not appear in ordinary Git status.
- No production file or Cargo/lint/format command was touched during proposal work.

## Phase 3: Canonical implementation

### Status

completed

### Context

After approval only, implement the selected one-daemon/one-child workspace owner in the primary agent while preserving Kit dependency and dirty-tree boundaries.

### Implementation

- [x] Re-read revision 2 and map exact production file ownership against the live dirty tree.
- [x] Implement only in the primary agent; research subagents do not write production code.
- [x] Reuse `ProcessSupervisor`, detached receipts, `AtomicFileWriter`, `RepositoryLocator`, output, and verified Unix-socket invariants at their proper owners. Do not add a watcher: each call re-reads and synchronizes the queried document before hierarchy requests.
- [x] Implement exact workspace/server identity, registry transaction, authenticated client protocol, daemon lifecycle, LSP pump, document sync, call hierarchy, management, recovery, and idle timeout.
- [x] Keep all single-consumer language-service policy under `src/tools/tsgo`; do not import another tool or create a speculative peer framework.
- [x] Avoid or delete every rejected alias, fallback, raw LSP path, PID authority, repository-local state, and duplicate owner.

### Verification

- [x] Focused non-Cargo source checks prove one owner and the absence of forbidden paths.
- [x] No tests or RED artifacts are added.
- [x] Scoped status/diff proves registration edits are additive and the tool owns only its directory.

### Exit criteria

- [x] Approved behavior is implemented with one canonical path and no unapproved compatibility lane.
- [x] Production diff is isolated and every structural stop-gate finding is resolved or approval-gated.

### Evidence

- Frozen owned files: `src/tools/tsgo/{mod.rs,protocol.rs,service.rs}` plus exact registration hunks in `src/tools/mod.rs` and `src/main.rs`.
- Existing framework owners remain canonical: repository resolution, structured output, attached child control, detached receipt authority, and atomic state replacement.
- Existing CDP code supplied transaction/socket patterns only; `tsgo` imports no sibling tool.
- Concurrency boundary: Unix handlers accept concurrent clients; one daemon actor owns the native stream and correlates monotonic IDs, preventing cross-client reply theft without a general LSP framework.
- Sync boundary: the queried file is re-read on every operation and sent as `didOpen` or full-text `didChange`; no broad filesystem watcher enters scope.
- Source-only checks pass: no sibling-tool imports, direct process spawning, watcher, raw request surface, repository-local state, alias command, or PID authority exists under `src/tools/tsgo`.

## Phase 4: Bounded compile proof

### Status

completed

### Context

Prove the approved Rust source compiles without competing with user builds or silently expanding verification authority.

### Implementation

- [x] Check for active Cargo/rustc processes and wait rather than compete.
- [x] Run `cargo check -j 2` under current authorization, correcting only the recorded task-local derive/import diagnostics from the first attempt.
- [x] After checks pass, use the root installer only for the live artifact and the two recorded task-owned corrective refreshes.
- [x] Do not run formatting, linting, tests, benchmarks, or unrelated Cargo work.

### Verification

- [x] Record the exact process inspection command/output.
- [x] Record each bounded `cargo check -j 2` and install command/output; classify every failure and corrective refresh.

### Exit criteria

- [x] Authorized compile proof passes, or failure is recorded and routed back to Phase 3.
- [x] No Cargo/rustc process remains after an interruption.

### Evidence

- No Cargo/rustc process was active before check/install attempts, and no heavy operations overlapped.
- First check failed only on the recorded root Clap derive and unused import.
- Corrected check passed in 15.35 seconds with no warnings.
- The root installer completed initially in 4m54s. Two task-owned live findings required recorded corrective checks and artifact refreshes (4m51s and 4m46s); no competing Cargo work ran.

## Phase 5: Real command-path acceptance

### Status

completed

### Context

Exercise the installed Kit surface against Modular and its native server. Use one uniquely named temporary untracked fixture for controlled edits; static checks, mocks, and PID equality cannot substitute.

### Implementation

- [x] Install the settled binary after compile proof, with only recorded corrective refreshes after live verifier defects/evidence omissions.
- [x] Snapshot Modular status, create one unique untracked fixture inside Modular, and remove it after proof.
- [x] Execute the seven-step primary verifier entirely through public Kit operations after fixture setup.
- [x] Capture instance ID, start time, public request count, child run ID/generation/start, canonical launcher, and exact version.

### Verification

- [x] First query starts the service.
- [x] Second query reuses the same service and child while increasing request count.
- [x] A TypeScript edit changes hierarchy output without child restart.
- [x] Two concurrent clients receive their own correctly correlated sentinel results.
- [x] Stop completes LSP shutdown/exit, terminates, and reaps child and daemon ownership.
- [x] A later query starts a clean replacement instance and child.
- [x] Owned stale registry/socket state recovers without PID guessing.
- [x] Prepare, incoming, and outgoing all work through the public command path.

### Exit criteria

- [x] Every real-surface proof passes with structured outputs recorded in the worklog.
- [x] No PID-only, mock-only, or direct-daemon claim substitutes for the public command path.

### Evidence

- Warm identity stayed at instance `1beef046-47e2-46bb-aba0-c00234547626`, child `2187d7a5-f5aa-4518-8187-2ef8b94c410f`, and fixed starts while request count advanced 1→6.
- The live edit removed `entry` from `alpha` incoming results and changed `entry` outgoing to `gamma` without identity change.
- Concurrent clients returned their own `beta` and `gamma` items with shared identity and distinct counts 6/5.
- A post-stop query created new instance `74115378-331b-4ec8-bfaa-2419163958fd` and child `5b325b2b-ccb1-4a4d-bd5c-9790918508a8` at request 1.
- A seeded owned stale receipt/socket was reported stale, then automatically reconciled by a normal query into instance `2f76a019-a858-4ef0-831b-f105e489e027`; no PID field or signal was used.
- Final installed stop reports `graceful:true`, `reaped:true`, `completion:Natural`, and a complete stopped service; inspect/prune are empty afterward.

## Phase 6: Completion audit and handoff

### Status

completed

### Context

Audit requirements, ownership, avoidance/deletion, risks, verification, and dirty-tree preservation one-for-one before goal completion.

### Implementation

- [x] Compare every user requirement against implementation and saved proof.
- [x] Run residue searches for rejected verbs/aliases, broker/global owner, raw LSP, PATH fallback, PID authority, repository-local state, and tool-to-tool imports.
- [x] Inspect scoped Git status/diff and confirm the task owns only its new files/additive registration hunks; no Modular fixture remains.
- [x] Update the proposal to the approved narrow actor/full-text-sync implementation actually delivered.

### Verification

- [x] Every phase exit has reproducible evidence.
- [x] Every high-confidence adversarial finding is incorporated or rebutted with evidence.
- [x] Final handoff names exact commands/results and all intentionally unrun checks.

### Exit criteria

- [x] The observable finish line and all Completion Proof items are satisfied.
- [x] No required work or unresolved high-confidence blocker remains.
- [x] The plan is ready for the durable goal to be marked complete.

### Evidence

- `git diff --check` passes for all owned source/registration paths; no dependency file changed and no task test file exists.
- Residue searches find no sibling-tool import, direct process spawn, watcher owner, raw-LSP public command, PATH fallback, broker/global service, PID authority, or rejected alias.
- The three report artifacts remain 12,743 / 12,530 / 14,760 bytes and retain `## Decision capsule`.
- Proposal revision 2 is fully visible with 18 balanced sections, no `<details>`, and no superseded watcher/pending-map/repeated-install claim.
- Final installed `inspect --all` reports zero services; no registry/socket or tsgo/daemon process remains. The private 0600 per-key lock remains as inert synchronization infrastructure.
- Modular's exact temporary path is absent and clean. Its global status hash changed from `21bf…273` to `b409…f02` while the shared dirty repo continued receiving unrelated work; no pre-existing Modular path was written by this verifier.
- Strict operating-plan validation passes.
- Intentionally not run: unit/integration/RED tests, lint, formatting, clippy, benchmarks, or unrelated verification, per user constraint.

## Baseline and Worklog

Append entries. Never erase prior evidence, user steering, failed approaches, or phase transitions.

### 2026-08-10 — Grounding and activation

- **Phase/status:** Phase 1, in progress.
- **Action:** Read Kit `AGENTS.md`, `CONTRIBUTING.md`, required skills/references, repository identity, dirty tree, and relevant memory registry; wrote `goal.md` and the first operating plan; activated goal `019fed9f-27b0-7bf2-8c3e-ab28d049e89f`.
- **Evidence:** Repository root `/home/tvx/Desktop/projects/kit`, origin `xtava/kit`, extensive pre-existing dirty status.
- **Finding:** The global `clean-cutover` skill referenced by repository rules was unavailable in the active skill catalog; its one-owner/no-shim/no-fallback contract was preserved directly. No relevant prior memory entry existed.
- **Plan effect:** Froze protected dirty-tree and approval boundaries.
- **Approval effect:** Research authorized; production implementation pending.
- **Next action:** Run three report-owning research lanes.

### 2026-08-10 — Research fleet launch retry

- **Phase/status:** Phase 1, in progress.
- **Action:** First Kit-lane spawn supplied an incompatible agent-type/full-history combination; runtime rejected it. Retried immediately with identical model, medium effort, role, and output contract without the incompatible field.
- **Evidence:** Successful agent receipt after retry; owned report passed read-back.
- **Finding:** Infrastructure argument failure only; no research evidence lost.
- **Plan effect:** Recorded bounded retry; no architecture change.
- **Approval effect:** None.
- **Next action:** Enforce report artifact barrier.

### 2026-08-10 — Known native server identity

- **Phase/status:** Phase 1, in progress.
- **Action:** Inspected shim, package, help, version, and upstream revision without starting a primary-agent LSP process.
- **Evidence:** Shim resolves to `@typescript/native-preview/bin/tsgo.js`; CLI/package version `7.0.0-dev.20260418.1`; package git revision `1de8d68230f8759af6fb71d9cf0f9c37d2c65507`.
- **Finding:** Ordinary help does not advertise working `--lsp --stdio`; identity must bind canonical workspace, launcher, and exact version rather than symlink spelling or PID.
- **Plan effect:** Strengthened resolution/identity proposal requirements.
- **Approval effect:** None.
- **Next action:** Read back reports and synthesize only after all pass.

### 2026-08-10 — Research barrier complete

- **Phase/status:** Phase 1, completed; Phase 2 started.
- **Action:** Read back all three reports and directly checked cited Kit owners.
- **Evidence:** Report sizes 12,743 / 12,530 / 14,760 bytes; required markers present; Modular probe file diff clean.
- **Finding:** All lanes select a daemon + child per workspace/exact server identity. Native capture observed incremental sync, dynamic server requests, call hierarchy, and shutdown-without-params followed by exit code 1 plus `context canceled`.
- **Plan effect:** Added atomic publication, token handshake, bidirectional pump, generation-safe idle stop, receipt reconciliation, stable JSON evidence, and no broad watcher/broker/general LSP framework.
- **Approval effect:** Proposal synthesis authorized; implementation still pending.
- **Next action:** Write the complete HTML proposal.

### 2026-08-10 — Proposal coverage attempts

- **Phase/status:** Phase 2, in progress.
- **Action:** Wrote proposal revision 1 and ran mechanical requirement checks.
- **Evidence:** Initial artifact 573 lines / 37,169 bytes; first detector found two explicit label misses and they were corrected.
- **Finding:** Second detector falsely treated `+` as a regex quantifier and expected former capitalization. Fixed-string rerun passed; 18 section pairs balanced; reports remained valid. `.gitignore:6` intentionally ignores the local ideation path.
- **Plan effect:** Clarified selected architecture/non-goal labels and corrected the detector without weakening coverage.
- **Approval effect:** None; production gate remains.
- **Next action:** Structurally validate the operating plan.

### 2026-08-10 — Strict plan validation failure and reshape

- **Phase/status:** Phase 2, in progress.
- **Action:** Ran `validate_plan.py plan.md --kind operating --strict`.
- **Evidence:** Failed with 39 structural findings: missing TLDR/Baseline and Worklog/Completion Proof plus six required subsections per phase.
- **Finding:** The initial plan followed Ultragoal shorthand but not the stricter iterative-planning template.
- **Plan effect:** Reshaped this document to the required durable operating structure while preserving decisions, evidence, failures, status, and gates.
- **Approval effect:** Approval request waits for the strict rerun.
- **Next action:** Rerun strict operating-plan validation.

### 2026-08-10 — Strict plan validation passed

- **Phase/status:** Phase 2, in progress at the approval gate.
- **Action:** Reran `validate_plan.py plan.md --kind operating --strict` after reshaping.
- **Evidence:** Exit 0: `.codex/tsgo-language-service/plan.md: valid operating plan`.
- **Finding:** Durable restart, phase, evidence, approval, worklog, and completion structure is valid.
- **Plan effect:** Proposal revision 1 is ready to present; no architecture change.
- **Approval effect:** Complete proposal approval is now the only immediate gate.
- **Next action:** Present the proposal and wait without production edits.

### 2026-08-10 — Proposal presented; approval still pending

- **Phase/status:** Phase 2, in progress at the approval gate.
- **Action:** Presented the complete proposal and resumed from durable state on the next goal turn.
- **Evidence:** Proposal remains non-empty at the requested path; strict operating-plan validation still exits 0; no explicit approval response is present in the conversation or Approval Ledger.
- **Finding:** Production implementation requires a consequential user decision and cannot begin by inference.
- **Plan effect:** Marked proposal presentation complete; Phase 2 remains in progress until approval.
- **Approval effect:** Architecture and implementation gates remain pending.
- **Next action:** Wait for explicit approval or requested revisions; make no production edits.

### 2026-08-10 — Approval gate reached blocked threshold

- **Phase/status:** Phase 2, blocked at the external approval gate.
- **Action:** Re-audited the proposal, Approval Ledger, durable plan, and allowed work after the original presentation turn and two automatic goal continuations.
- **Evidence:** Proposal exists; strict plan validation previously passed; architecture and implementation ledger rows remain pending; no explicit user approval or requested revision is present across three consecutive goal turns.
- **Finding:** The same external approval condition now satisfies the goal's three-turn blocker standard. Production edits would violate the user-defined gate, and research/proposal verification is already exhausted.
- **Plan effect:** Phase 2 moved from `in progress` to `blocked`; no finish line, architecture, or verifier changed.
- **Approval effect:** Architecture and implementation remain pending. A user approval or concrete revision request is the exact resume event.
- **Next action:** On resume, record the user decision, set Phase 2 back to `in progress` or complete as appropriate, and continue from the approved scope.

### 2026-08-10 — Narrow implementation boundary frozen

- **Phase/status:** Phase 3, in progress.
- **Action:** Applied the user's no-test/no-scope-expansion steering, read the complete Kit contributor runbook and relevant framework owners, and inspected both staged and unstaged overlap in the registration files.
- **Evidence:** `ProcessSupervisor`, detached receipt control, `AtomicFileWriter`, `RepositoryLocator`, and `Output` provide the required shared mechanics; `src/main.rs` and `src/tools/mod.rs` contain protected user changes that can be preserved with exact additive hunks.
- **Finding:** The smallest coherent owner is three tool-local files. A daemon-owned actor can accept concurrent socket clients while serializing and explicitly correlating the native LSP stream. Re-reading the queried file before every call provides live document synchronization without a watcher subsystem.
- **Plan effect:** Removed the previously listed watcher reuse, froze `src/tools/tsgo/{mod.rs,protocol.rs,service.rs}`, and retained only two registration hunks outside that directory.
- **Approval effect:** No architecture/public-surface expansion; implementation remains authorized under proposal revision 2 and the latest user constraint.
- **Next action:** Implement the frozen slice, then run source-only boundary checks.

### 2026-08-10 — Canonical source slice implemented

- **Phase/status:** Phase 3, in progress pending compile proof.
- **Action:** Added the three frozen tool-local files and only the two registration hunks. Implemented lazy service launch, exact version identity, atomic receipt-backed registry, token-authenticated Unix protocol, daemon actor, native LSP lifecycle/framing, document synchronization, call hierarchy, inspect/stop/prune, idle teardown, and stale recovery.
- **Evidence:** `git diff --check` passed for the owned paths; scoped searches found no sibling-tool import, direct process spawn, watcher, raw-LSP public path, or extra registration. No test file or RED artifact was created.
- **Finding:** The feature fits entirely on existing owners. The public protocol returns daemon instance/start/request count plus child run/generation/start, so live reuse proof does not depend on PID equality.
- **Plan effect:** Phase 3 implementation and source-only checks are satisfied; compile proof is the next bounded gate.
- **Approval effect:** No scope or public-vocabulary change.
- **Next action:** Check for active Cargo/rustc, then run exactly one `cargo check -j 2`.

### 2026-08-10 — First bounded compile verifier failed

- **Phase/status:** Phase 3, in progress.
- **Action:** After confirming no active Cargo/rustc process, ran `cargo check -j 2`.
- **Evidence:** Exit 101 after 15.1 seconds: `TsgoArgs::command()` was unavailable because the new root CLI type derived `Args` instead of Kit's established `Parser`; one `serde_json::json` import was unused.
- **Finding:** Both diagnostics are isolated to `src/tools/tsgo/mod.rs`; no pre-existing or cross-owner failure appeared.
- **Plan effect:** Correct only the derive and unused import, then rerun the same bounded compile verifier. No test, lint, format, or broader Cargo action is added.
- **Approval effect:** None; diagnostic correction remains inside the approved owned file.
- **Next action:** Apply the two source corrections and rerun `cargo check -j 2`.

### 2026-08-10 — Bounded compile verifier passed

- **Phase/status:** Phase 3 completed; Phase 4 in progress.
- **Action:** Confirmed Cargo/rustc was idle and reran `cargo check -j 2` after the diagnostic-only correction.
- **Evidence:** Exit 0 in 15.35 seconds: `Finished dev profile`; no warnings.
- **Finding:** The approved source slice compiles against the current dirty Kit checkout without cross-owner changes.
- **Plan effect:** Phase 3 exit criteria are satisfied. Phase 4 now has only the authorized one-time install remaining.
- **Approval effect:** None.
- **Next action:** Read the installer, recheck resource availability, then install once.

### 2026-08-10 — Phase-state validation correction

- **Phase/status:** Phase 4, in progress.
- **Action:** Inspected `install.sh` and reran strict operating-plan validation before installation.
- **Evidence:** Installer is the repository's bounded `cargo install --locked --force -j 2 --root "$HOME/.local" --path ...`. Validation found Phase 3 still textually marked `in progress` after the transition to Phase 4.
- **Finding:** Durable-state bookkeeping only; source and architecture were unaffected.
- **Plan effect:** Marked Phase 3 `completed` and will rerun validation before installation.
- **Approval effect:** None.
- **Next action:** Validate the corrected plan, recheck Cargo/rustc, then run `./install.sh` once.

### 2026-08-10 — One bounded install completed

- **Phase/status:** Phase 4 completed; Phase 5 in progress.
- **Action:** Validated the corrected plan, confirmed Cargo/rustc was idle, and ran the repository root installer once.
- **Evidence:** `./install.sh` exited 0 after 4m54s and replaced `/home/tvx/.local/bin/kit`; the installer used `cargo install --locked --force -j 2`. The long interval was an active release/LTO rustc process, not a stall.
- **Finding:** The installed artifact is ready for the required public-command proof. No tests, lint, formatting, repeated install, or parallel Cargo task ran.
- **Plan effect:** Phase 4 exit criteria are satisfied; Phase 5 is active.
- **Approval effect:** None.
- **Next action:** Snapshot Modular state, create the unique untracked verifier fixture, and issue the first installed command.

### 2026-08-10 — Modular preflight and verifier cwd correction

- **Phase/status:** Phase 5, in progress.
- **Action:** Confirmed the installed `kit tsgo --help` surface, verified the unique fixture path is absent, and hashed Modular's complete porcelain status before mutation.
- **Evidence:** Public verbs are exactly `call`, `inspect`, `stop`, and `prune`; baseline status hash is `21bf0e7c8e35f4eb60a2ad50d891a091dbb25b44a43c986244e93213a6aed273`.
- **Finding:** A strict plan-validation command was launched from Modular with a Kit-relative plan path and correctly failed `file does not exist`; this is a verifier cwd mistake, not source or runtime failure.
- **Plan effect:** Rerun validation with the absolute Kit path before fixture creation; live verifier remains unchanged.
- **Approval effect:** None.
- **Next action:** Validate the absolute plan path, then create the one untracked TypeScript fixture.

### 2026-08-10 — Warm path passed; graceful-stop reply failed

- **Phase/status:** Phase 3 reopened; Phase 5 paused.
- **Action:** Through installed Kit on Modular, proved first-query start, second-query reuse, live `didChange`, prepare/incoming/outgoing, concurrent client correlation, and inspect; then invoked public stop.
- **Evidence:** Instance `1beef046-47e2-46bb-aba0-c00234547626`, child run `2187d7a5-f5aa-4518-8187-2ef8b94c410f`, and their start times stayed fixed while request count rose 1 through 6. The edit removed `entry` from `alpha` incoming calls and added `gamma` to `entry` outgoing calls. Concurrent `beta`/`gamma` clients returned their own items. Stop changed one record but returned `service: null` and detail `tsgo service request owner stopped`.
- **Finding:** `LspSession::finish` discarded the ready result from `timeout(&mut JoinHandle)` and then awaited the completed handle again, panicking the actor after child reap but before its structured stop reply reached the client.
- **Plan effect:** Correct only that owned wait-result branch, rerun bounded compile/install, and resume the verifier at stop. The warm-path evidence remains valid.
- **Approval effect:** Necessary defect correction inside the approved function; no scope, owner, or public command changes.
- **Next action:** Compile the teardown correction and refresh the installed artifact.

### 2026-08-10 — Teardown correction compiled

- **Phase/status:** Phase 3 completed again; Phase 4 in progress.
- **Action:** Confirmed Cargo/rustc was idle and ran `cargo check -j 2` for the single-function wait-result correction.
- **Evidence:** Exit 0 in 3.98 seconds with no warnings.
- **Finding:** The fix is task-local and compile-clean.
- **Plan effect:** Refresh the installed artifact once, then repeat stop and continue replacement/recovery proof.
- **Approval effect:** None.
- **Next action:** Run the necessary corrective install.

### 2026-08-10 — Corrected artifact installed

- **Phase/status:** Phase 4 completed; Phase 5 resumed.
- **Action:** Ran the necessary corrective root install with the same locked two-job boundary.
- **Evidence:** Exit 0 after 4m51s; `/home/tvx/.local/bin/kit` was replaced. No tests, lint, formatting, or competing Cargo operation ran.
- **Finding:** The real command path now contains the teardown correction.
- **Plan effect:** Resume at clean replacement identity and graceful stop, preserving all earlier passing warm-path evidence.
- **Approval effect:** None.
- **Next action:** Start the replacement service through a query and repeat public stop.

### 2026-08-10 — Runtime acceptance passed; teardown evidence projection added

- **Phase/status:** Phase 3 briefly reopened after Phase 5 runtime success.
- **Action:** Proved clean replacement, corrected stop, stale record/socket detection and automatic query recovery, live-safe prune, final stop, empty inspect/prune, and fixture removal. Completion audit then traced the stop response projection.
- **Evidence:** Corrected stop returned a full stopped service with no error; stale inspect reported the exact forgotten receipt and refused socket; a normal query started instance `2f76a019-a858-4ef0-831b-f105e489e027` / child `8db03bf3-11f1-48e0-8794-664bc7dedc67`; prune retained it; final stop left inspect/prune empty and no tsgo process. The daemon reply already contained graceful/reaped/process result, but `stop_services` discarded `reply.result`.
- **Finding:** Runtime lifecycle is correct; structured management output omitted existing teardown evidence required for the strongest public proof.
- **Plan effect:** Add only `InspectEntry.result` and project the already-returned daemon result. Recompile/reinstall, then repeat one bounded start/stop pair.
- **Approval effect:** No command, lifecycle, owner, or protocol expansion; output-only correction inside approved structured JSON.
- **Next action:** Compile the projection correction.

### 2026-08-10 — Teardown evidence projection compiled

- **Phase/status:** Phase 3 completed; Phase 4 in progress.
- **Action:** Confirmed no active Cargo/rustc process and ran `cargo check -j 2`.
- **Evidence:** Exit 0 in 4.60 seconds with no warnings.
- **Finding:** The output-only projection is compile-clean.
- **Plan effect:** Install this final artifact and repeat only the start/stop evidence pair.
- **Approval effect:** None.
- **Next action:** Refresh the installed artifact.

### 2026-08-10 — Final installed teardown proof passed

- **Phase/status:** Phase 5 completed; Phase 6 in progress.
- **Action:** Installed the output projection, recreated the unique fixture, issued one public prepare call, issued public stop, and removed the fixture.
- **Evidence:** Stop JSON for instance `73d47ac5-f0c0-4d30-b4e0-6fed97f87a2f` / child `a21dfbbf-68bc-4ae5-8265-6c8bc9efb9a4` contains `result: {completion:"Natural", graceful:true, leader_exit:"Observed(Code(0))", reaped:true}` and `detail:null`.
- **Finding:** Teardown is now directly proven through the public structured surface, not inferred.
- **Plan effect:** All Phase 5 checks are satisfied; proceed to the no-build completion audit.
- **Approval effect:** None.
- **Next action:** Audit source scope, rejected paths, durable artifacts, runtime cleanup, and dirty-tree preservation.

### 2026-08-10 — Final audit shell quoting retry

- **Phase/status:** Phase 6, in progress.
- **Action:** Launched a combined empty-runtime/proposal-residue audit.
- **Evidence:** Zsh rejected the command before execution with `unmatched \"` because a backtick-bearing search pattern was inside double quotes.
- **Finding:** Audit command construction only; no verifier ran and no state changed.
- **Plan effect:** Rerun the same read-only/public-management checks with single-quoted fixed patterns.
- **Approval effect:** None.
- **Next action:** Rerun the final runtime/proposal audit safely.

### 2026-08-10 — Proposal counter escape correction

- **Phase/status:** Phase 6, in progress.
- **Action:** Ran the final scoped audit bundle.
- **Evidence:** Source checks, plan validation, and empty process checks passed. The proposal counter falsely reported zero opening sections because its Python raw regex contained a doubled backslash (`<section\\b`); closing sections remained 18.
- **Finding:** Counter escaping only. Direct `rg` already showed 18 opening tags, including classed sections.
- **Plan effect:** Rerun the counter with regex `<section\b`; no source or proposal edit required.
- **Approval effect:** None.
- **Next action:** Complete the corrected proposal/artifact count and finish the audit ledger.

### 2026-08-10 — Completion audit passed

- **Phase/status:** Phase 6 completed.
- **Action:** Re-ran the corrected proposal counter, scoped diff/residue checks, research artifact barrier, plan validation, installed empty-management checks, runtime process/state checks, and Modular fixture check.
- **Evidence:** Proposal 18/18 sections, zero collapsed details, 36,994 bytes; all three reports retain their decision capsule and exact recorded sizes; scoped diff check and strict plan validation exit 0; `inspect --all` and `prune --all` match zero; no registry/socket or tsgo/daemon process remains; fixture path is absent.
- **Finding:** One inert mode-0600 key lock remains in Kit's mode-0700 state directory as synchronization infrastructure, not a live service record/socket/process. Modular's global dirty-status hash drifted during shared work, but the unique verifier path is absent and the verifier wrote no pre-existing path.
- **Plan effect:** Every phase, exit criterion, and completion-proof item is satisfied.
- **Approval effect:** No remaining approval gate.
- **Next action:** Mark durable goal complete and hand off.

### 2026-08-10 — Full call-chain performance baseline

- **Phase/status:** Completed goal, supplemental performance proof.
- **Action:** Used the installed public Kit command to follow a controlled 11-edge static call hierarchy in Modular. Measured restart-before-each-hop, lazy-start-and-reuse, and already-hot modes with `perf stat` plus recursive service-cgroup CPU accounting.
- **Evidence:** Three accepted passes resolved every expected `chain00` through `chain11` edge. Median restart/lazy/hot wall totals were 3,981.3 / 667.1 / 326.9 ms; median combined CPU totals were 41,672.9 / 5,677.9 / 1,925.5 ms. Restart created 11 instances and 11 children per trace; both retained modes kept exactly one instance and child while request count advanced across all 11 queries.
- **Finding:** An already-hot service made the complete chain 12.2× faster in wall time and used 21.6× less combined CPU than per-hop native startup. Including the first lazy startup still improved wall time 6.0× and combined CPU 7.3×. Two preliminary reporting passes were rejected after the disposable harness first passed an empty `task-clock:u` field to `jq`; matching the actual perf event label and reading recursive cgroup `cpu.stat` corrected the reporter without changing Kit.
- **Plan effect:** Added the accepted raw samples, measurement boundary, identity proof, and comparison to `performance-baseline.md`; no production source or public scope changed. A final read-only existing-source check followed `parseServerMode → findArgument → Array.indexOf` while keeping instance `e91b41ba-a4a6-4b18-93d7-a4b25b896285` and child `0ea789a4-5c02-4ab6-a996-1c29ea64c8d6` across request counts 1→2; public stop reported graceful natural reap.
- **Approval effect:** None; this was user-requested live performance verification only.
- **Next action:** Handoff. The unique Modular fixture and measurement harness are removed, and Kit reports no live service.

## Completion Proof

- [x] The approved observable finish line is demonstrated.
- [x] The seven-step real Kit command verifier passes with saved structured evidence.
- [x] Warm reuse uses identical daemon instance/start and child run/generation/start with increased public request count.
- [x] Live file updates and two-client correlation pass without child restart.
- [x] Graceful stop/reap, clean replacement, and stale-state recovery pass without PID guessing.
- [x] Prepare, incoming, and outgoing call hierarchy pass through the public surface.
- [x] Every supporting verifier required by the approved proposal passes or is explicitly approval-limited.
- [x] Every avoidance/deletion and negative check passes.
- [x] Every phase exit criterion has evidence.
- [x] No unresolved high-confidence adversarial finding remains.
- [x] The final report names verification not run and why.
- [x] The goal is marked complete only after all proof is present and no required work remains.
