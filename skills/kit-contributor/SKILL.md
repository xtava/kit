---
name: kit-contributor
description: Contribute to the xtava/kit Rust repository using its canonical framework, TUI, process, testing, CI, and release rules. Use when implementing, refactoring, reviewing, testing, documenting, or publishing Kit; when adding a Kit tool or shared capability; or when changing Kit framework, TUI, CDP, process, platform, or GitHub workflow code.
---

# Kit Contributor

Use a **reuse-first** workflow: find the canonical owner, preserve its boundary, and leave one
coherent path behind.

## 1. Orient

Resolve the repository with `git rev-parse --show-toplevel`. Confirm it is the `kit` Cargo package
from `xtava/kit`; when the current directory is elsewhere, use the user-provided checkout rather
than assuming one machine-specific path.

Read every applicable `AGENTS.md` plus the repository's `CONTRIBUTING.md` before non-trivial work.
Treat those live files as authoritative when they differ from this skill.

Load [references/system.md](references/system.md) for every implementation, architecture, or review
task. Load [references/rust-practices.md](references/rust-practices.md) whenever Rust is read or
changed. Load [references/contribution-runbook.md](references/contribution-runbook.md) before
editing, testing, installing, committing, pushing, or diagnosing CI. For versioning, release
workflows, update behavior, tags, or published binaries, also load
[references/release-runbook.md](references/release-runbook.md).

Orientation is complete when the repository rules, current dirty state, affected owner, and
relevant verification lane are known.

## 2. Map the owner

Inspect the current implementation and its callers before proposing a shape. Separate:

- tool-owned domain policy and presentation;
- reusable mechanics owned by `framework`, `tui`, `cdp`, or a justified peer module;
- effect boundaries from pure parsing, ranking, grouping, and state transitions.

State the invariant and the positive capture boundary: what the change handles, what remains
outside it, and which proof will fail if the invariant breaks. Search shared modules and sibling
tools before adding helpers, state, dependencies, or abstractions.

The map is complete when there is one named behavior owner, every affected consumer is known, and
the verification proves the owner rather than only one UI path.

## 3. Implement the canonical path

Keep the dependency direction `tools/* -> framework | tui | cdp`. Tools remain independent of one
another; shared modules remain independent of tools. Extend an existing shared primitive when the
mechanism is genuinely shared. Keep a single caller's policy local until a real shared invariant
exists.

When replacing behavior, migrate all current producers and consumers, delete the displaced path,
and remove its unused tests, imports, state, and dependencies in the same change. Keep `main.rs` as
registry wiring and put reusable logic in the library.

For interactive work, project one typed action model through keyboard, mouse, menus, and visible
help. Reuse Kit's navigation, history, context-menu, split, settings, clipboard, search, session,
and theme primitives instead of rebuilding them inside a tool.

Implementation is complete when every current caller uses the canonical owner, the old lane has no
remaining references, and focused tests cover the owner plus each policy adapter.

## 4. Verify and hand off

Follow the resource-conservative sequence in the contribution runbook. Run one Cargo operation at
a time with at most two jobs, starting with the cheapest relevant proof. Keep warning-fatal CI
behavior local before pushing. Install only when requested or required for a bounded live proof.

Report the behavior delivered, relevant files, exact verification commands and outcomes, and any
skipped platform or live proof. Preserve unrelated dirty work. Commit, push, publish, delete, or
rewrite history only when the user authorizes that operation.

Handoff is complete when the scoped diff is intentional, validation is proportional and green, and
remaining uncertainty is explicit.
