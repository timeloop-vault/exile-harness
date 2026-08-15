---
name: git-workflow
description: Create branches and commits following this repo's conventions. Use when creating branches for features, fixes, chores, refactors, or docs, and when committing changes with properly formatted messages. Handles branch naming (feat/, fix/, chore/, refactor/, docs/) and commit messages (imperative mood, ≤80 char subject, subject=WHAT, body=WHY only and usually omitted, never narrate HOW or list changes, required trailers).
---

# Git Workflow

Create branches and commits following exile-harness conventions.

## When to Use This Skill

- User wants to create a branch for their work
- User wants to commit changes
- User says "create a branch", "commit this", "commit my changes", or similar
- When you see uncommitted changes and need to help organize them

## Agent Instructions

**When the user asks to create a branch or commit changes:**

1. **Check git status first:**
   ```bash
   git status --short
   ```

2. **Analyze the changes:**
   - Look at which files are modified/added/deleted
   - Infer the type of work based on file patterns:
     - `docs/`, `*.md`, `README` changes → likely `docs/`
     - Test files only → likely `chore/` or `test/`
     - New features/capabilities → likely `feat/`
     - Bug fixes → likely `fix/`
     - Dependencies, configs, tooling → likely `chore/`
     - Structure-only changes with no behavior change → likely `refactor/`

3. **Propose the branch/commit:**
   - Show the user what you plan to create, with your reasoning based on
     the file changes. Ask a clarifying question only when the work type
     is genuinely ambiguous — never prompt for everything.
   - Example: "I see changes to documentation files. I'll create a
     `docs/update-readme` branch and commit with `docs: <subject>`"

4. **Execute the git commands** (don't just suggest them)

## Branch Creation

| Work Type     | Branch Format               | Example                    |
| ------------- | --------------------------- | -------------------------- |
| Feature       | `feat/descriptive-name`     | `feat/league-tool`         |
| Bug/Fix       | `fix/descriptive-name`      | `fix/repl-bom-handling`    |
| Chore         | `chore/descriptive-name`    | `chore/update-dependencies`|
| Refactor      | `refactor/descriptive-name` | `refactor/extract-toolkit` |
| Documentation | `docs/descriptive-name`     | `docs/add-workflow-skills` |

**When to use which:**
- `feat/` — New capability
- `fix/` — Bug fix or correction
- `chore/` — Maintenance, tooling, dependencies
- `refactor/` — Restructuring with no behavior change
- `docs/` — Documentation-only changes

```bash
git checkout -b <branch-name>
```

`main` is protected — all work lands via PR (see the `pr-workflow` skill).
Never commit directly to `main`.

## Commit Messages

### Format

```
<prefix>: <subject>

<body (optional)>

<trailers (required when Claude authors the commit)>
```

### The one rule that matters

**The subject says WHAT changed. The body — if there is one — says WHY.**
Never narrate HOW. Never re-list the changes. The diff already shows the
what and the how; the commit message exists to capture the *why* that the
diff cannot.

Most commits need **no body at all**. A clear subject is the whole message.

### Rules

1. **Prefix**: Match the branch type — `feat:`, `fix:`, `chore:`,
   `refactor:`, `docs:`

2. **Subject line** (≤80 characters total including prefix):
   - Imperative mood ("add" not "added", "fix" not "fixes")
   - Lowercase
   - State WHAT changed, concisely. Not HOW.
   - No period at the end

3. **Body — default to none.** Add one ONLY when the subject leaves a
   genuine *why* unanswered. Valid reasons for a body:
   - The root cause of a bug that isn't obvious from the fix
   - A trade-off or constraint that forced this approach
   - A non-obvious consequence, follow-up, or rollback note

   When you do write a body:
   - Explain WHY, not what or how. If a sentence describes the code
     change, delete it — the diff already says that.
   - **No bullet lists that enumerate the changes** (`- bump X`,
     `- add Y`). That is the diff rewritten as prose; cut it.
   - Keep it tight. A sentence or two is almost always enough.

4. **If a commit does so much that you feel the need to list its changes,
   that is a signal to split it into smaller commits** — not to write a
   longer message. One logical change per commit.

5. **Trailers**: when Claude authors the commit, end the message with the
   `Co-Authored-By: Claude ...` and `Claude-Session: ...` lines used
   throughout this repo's history.

6. **Public repo**: never put PII, secrets, machine-specific paths, or
   private infrastructure details (LAN IPs, hostnames) in a commit
   message or in committed content.

### Validation

Before committing, verify the first line is ≤80 characters. If over 80,
shorten the subject — don't spill the overflow into a body. The subject
should be tighter, not longer elsewhere.

### Examples

Most changes need only a subject:

```
feat: add league resolver tool
```

```
fix: strip UTF-8 BOM from piped REPL input
```

```
chore: update ureq to latest
```

```
docs: add REPL commands to README
```

A body is justified only when it adds a *why* the diff can't:

```
fix: strip UTF-8 BOM from piped REPL input

PowerShell pipes prefix the first line with a BOM, so the first command
never matched and fell through to chat. Interactive input is unaffected.
```

```
refactor: extract shared tool runtime into exile-toolkit

Keeps exile-tool-api dependency-free (core depends on it); every future
tool would otherwise copy the HTTP/UA/timestamp plumbing and drift.
```

Note what these bodies do NOT do: they don't say "changed the parser and
the dispatch loop" or list the files touched. That's the diff's job.

### Anti-pattern: narrating the diff

A large change tempts a change-log body. Resist it — a big diff does not
earn a big message.

❌ **Bloated (restates the diff, enumerates changes):**
```
feat: add wiki retrieval tool

Add the new wiki retrieval tool. The tool fetches articles from the
community wiki and caches them locally so the model can ground answers
in real mechanics text.

- Add exile-wiki crate (dependency + lockfile)
- Register the tool in the REPL registry
- Update CLAUDE.md architecture tree
```

✅ **Tight (subject carries the what; body carries the one why):**
```
feat: add wiki retrieval tool

Articles carry vintage stamps so the model can judge freshness; the
local cache keeps repeat lookups off the wiki's rate limits.
```

If the bullet list felt necessary, the real fix is often to split: one
commit for the crate, one for the registry wiring.

## Safe Git Commands

**CRITICAL**: Avoid interactive commands that open VIM/nano.

### Squash Commits (non-interactive)

```bash
git reset --soft main
git commit -m "feat: description"
git push --force-with-lease origin <branch-name>
```

Force-pushing is allowed on non-`main` branches only, and never after
review has started (see `pr-workflow`).

### Check History

```bash
git log --oneline -10
git log main..HEAD --oneline  # Commits only in current branch
```

### Amend Without Editor

```bash
git commit --amend -m "new message"  # Always use -m flag
```
