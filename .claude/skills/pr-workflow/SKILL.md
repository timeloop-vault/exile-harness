---
name: pr-workflow
description: Create pull requests following this repo's conventions. Use when pushing changes and opening a PR, preparing code for review, or when user says "create PR", "open pull request", "push and create PR". Handles pushing, PR creation with description template, and review.
---

# PR Workflow

Create pull requests following exile-harness conventions.

## Prerequisites

- Changes committed (use the `git-workflow` skill for branch/commit
  conventions)
- On a branch that isn't `main` (`main` is protected — everything lands
  via PR)

## Workflow

### 1. Verify State

```bash
git status                    # Should be clean (nothing to commit)
git branch                    # Verify on feature branch, not main
git log main..HEAD --oneline  # Review commits to be included
```

The pre-commit hook has already enforced fmt/clippy/tests on every
commit; for a final check run `cargo test --workspace`.

### 2. Squash (if needed)

If you have multiple commits and want a single clean commit:

```bash
git reset --soft main
git commit -m "feat: description"
```

If you already pushed before squashing, you'll need `--force-with-lease`
in the push step below.

### 3. Push Branch

```bash
git push -u origin $(git branch --show-current)
```

If you squashed after a previous push, use `--force-with-lease`.

> **Never force-push after review has started.** It can orphan review
> comments and make them harder to interpret. The one exception: the
> reviewer explicitly asks for a history rewrite (e.g. purging
> machine-specific or sensitive content from public history) — then use
> `--force-with-lease`, never plain `--force`.

### 4. Create PR

Use the `gh` CLI.

**Title:** Either a `feat:`/`fix:`/`chore:`/`refactor:`/`docs:` prefixed
subject, or a short descriptive phrase (this repo has used both, e.g.
"League resolver: multi-source live current + vendored past leagues").

**Description template:**

```markdown
## Why

[1-2 sentences explaining the reason/problem being solved]

## How

- [Implementation decision or review guidance]
- [Keep to a few bullet points]
```

The "What" is covered by the PR title — no need to repeat it.

When Claude authors the PR, end the body with the repo's standard footer
(the "Generated with Claude Code" line plus the session link).

**Public repo:** never include PII, secrets, machine-specific paths, or
private infrastructure details in PR titles, bodies, or comments.

> **CRITICAL — No escape sequences in PR body:**
> When passing the description to the `gh` CLI, use **real newlines** —
> never literal `\n` escape sequences. Literal `\n` will appear as
> visible backslashes on GitHub instead of line breaks. Write the body
> as plain multi-line markdown text (e.g. a PowerShell here-string), not
> as a single-line string with escape characters.

### 5. Reviews

Every PR is reviewed by the repo owner before merge. When review
comments arrive, use the `pr-review-address` skill: triage each comment,
apply the accepted fixes, and reply on every thread signed as Claude on
behalf of the account owner. Never resolve threads — the reviewer does.
