---
name: pr-review-address
description: Fetch review comments on a PR (inline threads and top-level), analyze each one, recommend accept/reject for the user to decide, apply the accepted fixes, and reply to every comment signed as Claude answering on the account owner's behalf. Use when review comments exist on a PR and the user wants them triaged or addressed.
---

# Addressing PR review comments

## 1. Identify the PR

From the current branch: `gh pr view --json number,url,headRefName`.
If the user names a PR number, use that instead.

## 2. Fetch everything

```
gh api repos/{owner}/{repo}/pulls/{n}/comments     # inline review comments
gh api repos/{owner}/{repo}/pulls/{n}/reviews      # review bodies + verdicts
gh api repos/{owner}/{repo}/issues/{n}/comments    # top-level conversation
```

When thread state matters (resolved / outdated), use GraphQL:

```
gh api graphql -f query='
query($owner:String!,$repo:String!,$pr:Int!){
  repository(owner:$owner,name:$repo){
    pullRequest(number:$pr){
      reviewThreads(first:100){ nodes{
        isResolved isOutdated path line
        comments(first:50){ nodes{ databaseId author{login} body } }
      } }
    }
  }
}' -F owner={owner} -F repo={repo} -F pr={n}
```

Skip threads that are already resolved, and threads whose latest reply
carries the Claude signature tag (already handled).

## 3. Analyze and recommend

For each open comment, classify it: change request, question, or opinion.
Check the claim against the actual code before judging it. Produce a
recommendation per comment: **accept** (with the planned fix), **reject**
(with a concrete rationale), or **answer** (question — no code change).

Present the list to the user to pick per comment (AskUserQuestion when
choices are genuinely debatable). Exception: unambiguous directives from
the repo owner may be applied directly — say so in the summary.

## 4. Apply

Make the accepted changes, run the quality gates (see CLAUDE.md Commands),
commit with the project's commit trailers, and push. Never rewrite history
or force-push unless a comment explicitly asks for it; then use
`git push --force-with-lease` on the PR branch only.

## 5. Reply on every thread

Reply per inline comment (never resolve threads — the reviewer resolves):

```
gh api repos/{owner}/{repo}/pulls/{n}/comments/{comment_id}/replies \
  -f body='<what was done, or why rejected / the answer>

— Claude (on behalf of @{account})'
```

Top-level comments get one summary reply via `gh pr comment`.

Every reply MUST end with the signature tag `— Claude (on behalf of
@{account})` so it is always clear an LLM wrote it from the account
owner's session.
