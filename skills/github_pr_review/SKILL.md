---
name: github-pr-review
description: "Review GitHub PR comments, address feedback, and suggest code changes to resolve review comments. Handles multi-comment PR discussions."
user_invocable: true
when_to_use: "When the user wants to review PR comments, address review feedback, says 'check PR comments', 'review PR', 'address feedback', or 'pr review'"
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
  - github_get_issue
  - git_diff
  - read_file
  - str_replace
  - grep
---
# GitHub PR Review

Review PR comments from GitHub, address feedback, and suggest code changes to resolve review discussions.

## Task

$ARGUMENTS

---

## Phase 1: Identify the PR

If `PR_NUMBER` is not provided:
1. Use `github_list_prs` with `state: "open"` and `detail: "normal"` to list open PRs
2. Show the list and ask the user which PR to review
3. If only one open PR exists, use it automatically

## Phase 2: Fetch PR Details and Comments

Use `github_get_issue` with `detail: "detailed"` to get:
- PR title, description, and labels
- Review comments and discussions
- Commit history (if available)

```json
{"repo": "owner/repo", "issue_number": N, "detail": "detailed"}
```

## Phase 3: Categorize Comments

Group comments by type:

| Category | Description | Action |
|----------|-------------|--------|
| 🔴 Must Fix | Bugs, incorrect logic, security issues | Apply fix |
| 🟡 Should Fix | Missing tests, edge cases, error handling | Apply fix or discuss |
| 💡 Suggestion | Style, naming, alternative approach | Consider or discuss |
| ✅ Approved | Positive feedback | Acknowledge |
| ❓ Question | Clarification needed | Answer |

## Phase 4: Address Each Comment

For each comment that needs action:

1. **Locate the code** — use `read_file` to see the current state of the referenced file/line
2. **Understand the feedback** — what is the reviewer asking for?
3. **Propose a fix** — use `str_replace` to apply the change, or explain why the current approach is correct
4. **Verify** — after making changes, run relevant checks (build, test)

### Addressing Common Review Patterns

**"Can you add a test for this?"**
- Find the test file for the changed module
- Add a test that covers the scenario
- Run the test to verify

**"This looks like it could fail when..."**
- Analyze the edge case
- Add handling if needed, or explain why it's safe
- Add a test if appropriate

**"Consider using X instead of Y"**
- Research if X is indeed better
- If yes, apply the change
- If no, explain why Y is preferred

**"Missing error handling"**
- Add appropriate error handling
- Update the function signature if needed
- Add a test for the error path

## Phase 5: Report

```markdown
## PR Review: #{pr_number} — {title}

### Review Comments Summary

| Status | Count | Details |
|--------|-------|---------|
| ✅ Addressed | {n} | {summary} |
| 💬 Discussed | {n} | {summary} |
| 🔄 Pending | {n} | {summary} |

### Changes Made

{list of file changes with brief explanation}

### Remaining Discussions

{comments that need user input or are awaiting reviewer response}
```

## Phase 6: Suggest Reply Text

For comments that don't need code changes but need a response, suggest reply text that:
- Acknowledges the reviewer's point
- Explains the reasoning (if keeping current approach)
- Asks for clarification (if needed)
