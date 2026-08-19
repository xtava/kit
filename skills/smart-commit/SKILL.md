---
name: smart-commit
description: Commits only the relevant work produced in the current conversation by default, while preserving all pre-existing staged and unstaged changes exactly as they were. Use when the user asks to commit, smart commit, selectively commit the work just completed, or organize conversation-owned changes without disturbing a dirty worktree.
---

# Smart Commit

Create atomic commits from the work performed in the current conversation. The
default boundary is **provenance**, not topical similarity: a pre-existing change
does not become eligible merely because it touches the same feature, file, or
goal.

## Core Contract

Unless the user explicitly widens the scope, commit only changes that were made
as part of the current conversation.

- Preserve every pre-existing staged change as staged, with identical content.
- Preserve every pre-existing unstaged change as unstaged, with identical
  content.
- Preserve untracked files that were not created in the current conversation.
- Exclude pre-existing work even when it is closely related to the conversation.
- Never guess that an ambiguous hunk belongs to the conversation.
- If exact isolation cannot be proven without disturbing existing work, stop and
  explain the collision instead of committing a contaminated slice.

An explicit request such as "commit everything", "include the existing changes
in X", or "organize the whole worktree" may widen the boundary. A plain request
to "commit this", "commit our work", or use smart-commit does not.

State the interpreted boundary before mutating Git. In particular:

- "commit all email-related work" widens the eligible topic to every proven
  email hunk; it does **not** mean commit every staged file;
- "commit everything staged" includes the complete current index;
- a selective commit is not a request to make the Source Control staged list
  empty.

When the repository already contains protected staged work, say up front that a
successful selective commit will leave those files visibly staged.

## Step 1: Build the Conversation Change Ledger

Start from the actions and edits performed in the current conversation. Record
the exact files and, when necessary, the exact hunks that were created or changed
here.

Then inspect repository state without changing it:

```bash
git status --porcelain=v1
git diff --name-status
git diff --cached --name-status
```

Read the full staged and unstaged diffs for every conversation-touched or
ambiguous file:

```bash
git diff -- <paths>
git diff --cached -- <paths>
```

Use broader diffs only when needed to establish ownership. A huge dirty tree is
not permission to include everything, and dumping an enormous repository-wide
diff is not a substitute for tracing provenance.

Classify every relevant file or hunk as exactly one of:

1. **conversation-only** - created or changed entirely by the current
   conversation;
2. **pre-existing** - present before the current conversation;
3. **mixed** - contains both conversation and pre-existing changes;
4. **unknown** - provenance cannot be established from the conversation and Git
   evidence.

Only conversation-only changes and individually proven conversation-owned hunks
are eligible. Treat unknown changes as pre-existing.

## Step 2: Freeze the Protected State

Before staging or committing, identify the protected state:

- all pre-existing staged paths and hunks;
- all pre-existing unstaged paths and hunks;
- all pre-existing untracked files;
- any mixed files where index and worktree content differ.

The post-commit invariant is not merely "the files still exist." Their state must
remain equivalent: staged content stays staged, unstaged content stays unstaged,
and protected untracked content remains untracked.

Do not use operations that temporarily hide or rewrite protected work and hope to
restore it later.

## Step 3: Choose the Safe Selective Strategy

Choose the narrowest strategy that preserves the protected state.

### A. Whole files owned by the conversation

When every change in the selected files belongs to the conversation, but other
work is already staged, preview the payload with an isolated temporary index and
commit with explicit path-only mode:

```bash
task_index="$(mktemp)"
GIT_INDEX_FILE="$task_index" git read-tree HEAD
GIT_INDEX_FILE="$task_index" git add -- <exact-paths>
GIT_INDEX_FILE="$task_index" git diff --cached --check
GIT_INDEX_FILE="$task_index" git diff --cached -- <exact-paths>
rm -- "$task_index"

git commit --only -m "<message>" -- <exact-paths>
```

`git commit --only` is appropriate only when the complete working-tree change in
every named path is conversation-owned. It must not be used on a mixed file.

### B. Conversation hunks in mixed files with a clean index

If there were no pre-existing staged changes, use interactive staging to select
only proven conversation-owned hunks:

```bash
git add -p -- <exact-paths>
git diff --cached --check
git diff --cached
git commit -m "<message>"
```

Use `s` to split hunks and `e` only when the ownership of every edited patch line
is certain. Leave all pre-existing hunks unstaged.

### C. Mixed files while pre-existing work is staged

Do not use broad staging, a normal `git commit`, or `git commit --only` to force
this case. Those paths can respectively sweep in the protected index or commit
pre-existing work from the mixed file.

Proceed only if Git can isolate the exact conversation patch while preserving
both the existing index and worktree state. If that cannot be demonstrated
safely, stop and tell the user which file or hunk is entangled and why it was not
committed. Never solve this by resetting, stashing, restoring, or rewriting the
user's work.

### D. No provable conversation-owned diff

Do not create an empty or speculative commit. Report that no eligible changes
remain, for example when the work was already committed earlier in the
conversation.

## Step 4: Verify the Exact Payload

Before every commit, inspect the exact candidate payload. The verification must
show only conversation-owned changes.

For normal staged commits:

```bash
git diff --cached --check
git diff --cached
```

For path-only commits in a dirty index, use the isolated-index preview from
Strategy A. The real `git diff --cached` includes protected staged work and is
not the candidate payload for `git commit --only`.

Check explicitly that the payload contains no:

- pre-existing staged or unstaged hunks;
- unrelated formatting or generated artifacts;
- files merely related by topic;
- debugging residue not created for the finished result;
- credentials, tokens, or private material.

## Step 5: Commit Message

Follow repository-local commit rules first. Otherwise use:

- an imperative, specific summary under 50 characters;
- a body only when the reason or impact is not evident;
- no AI attribution or co-author trailer.

Example:

```text
[ai] Refine error banner styling
```

Never amend unless the user explicitly asks to amend and repository policy
allows it.

## Step 6: Prove Preservation After Commit

After committing, run:

```bash
git log --oneline -n <number-of-commits-created>
git status --porcelain=v1
git show --stat --oneline --summary HEAD
```

Also re-read the staged and unstaged diffs for protected paths that could have
been affected. Verify all of the following:

- each committed file or hunk belongs to the current conversation;
- pre-existing staged content is still staged and unchanged;
- pre-existing unstaged content is still unstaged and unchanged;
- unrelated untracked files remain untouched;
- no eligible conversation change was accidentally left behind, unless reported
  intentionally.

Do not claim preservation from path counts alone. Compare the relevant content
and index/worktree state.

### Prove the requested scope is gone

The repository may still show hundreds of staged files after a correct selective
commit. Prove the eligible scope independently from the overall dirty-tree count:

```bash
git diff --quiet HEAD -- <eligible-paths>          # worktree matches committed scope
git diff --cached --quiet -- <eligible-paths>      # no eligible staged diff remains
git diff --quiet -- <eligible-paths>               # no eligible unstaged diff remains

git diff --cached --name-only | wc -l              # protected staged files still visible
git diff --name-only | wc -l                       # protected unstaged files still visible
git ls-files --others --exclude-standard | wc -l  # protected untracked files still visible
```

If an eligible hunk was committed from a mixed file, that file may remain staged
because protected hunks still differ from `HEAD`. Re-read its staged diff and
report exactly what remains. Never infer commit failure from the overall staged
count, and never imply that the staging area is clear when it is not.

## Forbidden Operations

Unless the user explicitly requests the operation for an independent reason, do
not use:

- `git add .`, `git add -A`, or broad directory staging;
- `git commit -a`;
- `git reset`, `git restore`, or `git checkout --`;
- `git stash` as a way to move unrelated work out of the way;
- `git commit --amend`;
- temporary commits containing protected work;
- aliases, scripts, or patch flows whose effect on the real index has not been
  verified.

## Output

Report:

- the number of commits created;
- each commit hash and summary;
- the exact conversation-owned scope committed;
- whether the requested scope is now clean against `HEAD`, the index, and the
  unstaged worktree;
- the overall staged, unstaged, and untracked counts that remain visible;
- any mixed file that remains staged after its eligible hunk was committed,
  including what the remaining protected hunks concern;
- confirmation that pre-existing staged and unstaged work was preserved;
- any conversation-owned change left uncommitted because it was entangled or
  ambiguous.

Do not say "all work is committed" when protected changes remain. Say "the
requested scope is committed; N staged, M unstaged, and K untracked protected
paths remain" so the result matches what the user will see in Source Control.
