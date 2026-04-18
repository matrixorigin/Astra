---
name: github-pr-review
description: "Address GitHub PR review comments and feedback. Help resolve reviewer requests by making code changes. NOT for code review — use review-changes for that."
user_invocable: true
when_to_use: "When the user wants to address PR review comments/feedback, says 'check PR comments', 'address feedback', 'address review', or 'fix review comments'. NOT for code review of a PR — that is the review-changes skill."
arguments:
  - name: REPO
    description: "Repository as 'owner/repo' or bare project name. Defaults to current repo."
    required: false
  - name: PR_NUMBER
    description: "PR number to review. If omitted, lists open PRs first."
    required: false
  - name: ACTION
    description: "Action: 'list' (show PRs), 'comments' (show comments for a PR), 'address' (help address feedback). Default: 'comments'."
    required: false
allowed_tools:
  - github_list_prs
  - github_get_pr
  - github_get_issue
  - git_diff
  - read_file
  - str_replace
  - grep
  - bash
---
# GitHub PR Review

Review PR comments from GitHub, address feedback, and suggest code changes.

## Task

$ARGUMENTS

---

## Phase 1: Resolve Repository

1. **If the user provided a full GitHub URL** (e.g., `https://github.com/owner/repo/pull/123`), parse `owner/repo` and PR number directly from the URL. This takes absolute priority — do NOT fall back to git remote detection.
2. If `REPO` is provided as `owner/repo`, use it directly
3. Otherwise, detect from git remote:
   ```bash
   git remote get-url origin 2>/dev/null
   ```
   Parse `owner/repo` from the URL. If detection fails, ask the user.

## Phase 2: Identify the PR

If `PR_NUMBER` was already extracted from a GitHub URL in Phase 1, skip this phase.

If `PR_NUMBER` is not provided:
1. Use `github_list_prs` with `state: "open"` and `detail: "normal"`
2. If only one open PR exists, use it automatically
3. Otherwise, show the list and ask the user

**If native tool fails** ("requires a configured GitHub client"):
```bash
gh pr list --repo owner/repo --state open --json number,title,author,headRefName 2>&1
```

## Phase 3: Fetch PR Details and Comments

Use `github_get_pr` with `detail: "full"` to get:
- PR title, description, labels, changed files
- Review comments and discussions

```json
{"repo": "owner/repo", "pr_number": N, "detail": "full"}
```

**If `github_get_pr` doesn't return review comments**, supplement with `github_get_issue`:
```json
{"repo": "owner/repo", "issue_number": N, "detail": "full"}
```

**If native tools fail**, fall back to `gh`:
```bash
gh pr view N --repo owner/repo --json title,body,reviews,comments,reviewRequests,files --jq '.' 2>&1
```

## Phase 4: Categorize Comments

| Category | Description | Action |
|----------|-------------|--------|
| 🔴 Must Fix | Bugs, incorrect logic, security issues | Apply fix |
| 🟡 Should Fix | Missing tests, edge cases, error handling | Apply fix or discuss |
| 💡 Suggestion | Style, naming, alternative approach | Consider |
| ✅ Approved | Positive feedback | Acknowledge |
| ❓ Question | Clarification needed | Answer |

## Phase 5: Address Each Comment

For each comment that needs action:

1. **Locate the code** — use `read_file` to see the current state
2. **Understand the feedback** — what is the reviewer asking for?
3. **Propose a fix** — use `str_replace` to apply, or explain why current approach is correct

### Common Patterns

- "Add a test for this" → find test file, add test, run it
- "Could fail when..." → add error handling or explain why it's safe
- "Consider X instead of Y" → evaluate and apply or explain
- "Missing error handling" → add handling, update signature if needed

## Phase 6: Report

```markdown
## PR Review: #{pr_number} — {title}

| Status | Count | Details |
|--------|-------|---------|
| ✅ Addressed | {n} | {summary} |
| 💬 Discussed | {n} | {summary} |
| 🔄 Pending | {n} | {summary} |

### Changes Made
{list of file changes}

### Remaining Discussions
{comments needing user input}
```
