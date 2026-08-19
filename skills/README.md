# skills/ — canonical agent skills

One source of truth per skill, written to the open [Agent Skills
spec](https://agentskills.io/specification) (`SKILL.md` with `name` +
`description` frontmatter, supporting files loaded on demand). The same files
work in Claude Code, Codex, and any other spec-conformant tool — only the
*discovery* path differs per platform, and that is handled by projections:

```
skills/<name>/             ← canonical content (edit here, and only here)
.claude/skills/<name>      ← symlink: Claude Code project discovery
.agents/skills/<name>      ← symlink: Codex (and ecosystem) repo discovery
.claude-plugin/plugin.json ← the repo doubles as a Claude Code plugin
```

## Working in this repo

Nothing to do. Claude Code picks the skill up from `.claude/skills/`; Codex
picks it up from `.agents/skills/` (it scans every directory from cwd to the
repo root).

## Using the skill elsewhere

**Claude Code** — install the repo as a plugin:

```bash
/plugin marketplace add xtava/kit
/plugin install kit@kit
# or, for local development:
claude --plugin-dir /path/to/kit
```

**Codex** — symlink (or copy) the skill into your user skills directory:

```bash
mkdir -p ~/.agents/skills
ln -s /path/to/kit/skills/kit-cdp ~/.agents/skills/kit-cdp
ln -s /path/to/kit/skills/kit-dev-log ~/.agents/skills/kit-dev-log
ln -s /path/to/kit/skills/kit-tsgo ~/.agents/skills/kit-tsgo
```

**Anything else that speaks Agent Skills** — point it at the desired directory under `skills/`.

## Available skills

- `kit-cdp` — drive and verify live Electron or Chrome applications.
- `kit-dev-log` — create, update, and resume durable Kit engineering session ledgers.
- `kit-tsgo` — trace semantic TypeScript callers and callees through Kit's warm native server.
- `session-ledger` — build and maintain durable local context for long engineering efforts.
- `smart-commit` — commit only conversation-owned work while preserving pre-existing changes.

## Adding a skill

1. Create `skills/<name>/SKILL.md` (directory name must equal the frontmatter
   `name`; lowercase, hyphens). Put depth in `references/` — the body should
   stay lean, the description must carry the *when to use this* triggers.
2. Add the two symlinks: `.claude/skills/<name>` and `.agents/skills/<name>`,
   both pointing to `../../skills/<name>`.
3. `claude plugin validate .` to check the packaging.
