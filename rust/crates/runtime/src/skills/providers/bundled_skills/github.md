---
name: github
description: "Interact with GitHub — PRs, issues, CI status, reviews, and repository management"
version: "1.0.0"
triggers:
  - github
  - "create pr"
  - "open issue"
  - "ci status"
  - "pr status"
  - "merge pr"
  - "gh"
when_to_use: "When the user wants to interact with GitHub — create/review PRs, manage issues, check CI, or perform repository operations"
category: integration
arguments:
  - name: ACTION
    description: "GitHub action to perform (e.g., 'create pr', 'check ci', 'list issues', 'review #123')"
    required: false
tags:
  - github
  - git
  - workflow
  - integration
---
# GitHub Integration

Interact with GitHub using the `gh` CLI.

## Action

$ARGUMENTS

## Available Operations

### Pull Requests
- **Create PR**: `gh pr create --title "..." --body "..."`
- **List PRs**: `gh pr list`
- **View PR**: `gh pr view <number>`
- **Review PR**: `gh pr diff <number>` then provide review
- **Merge PR**: `gh pr merge <number>` (with appropriate strategy)
- **Check PR status**: `gh pr checks <number>`

### Issues
- **Create issue**: `gh issue create --title "..." --body "..."`
- **List issues**: `gh issue list`
- **View issue**: `gh issue view <number>`
- **Close issue**: `gh issue close <number>`

### CI/CD
- **Check CI status**: `gh run list --limit 5`
- **View run details**: `gh run view <run-id>`
- **View logs**: `gh run view <run-id> --log-failed`
- **Re-run failed**: `gh run rerun <run-id> --failed`

### Repository
- **View repo**: `gh repo view`
- **Clone**: `gh repo clone <owner/repo>`
- **Fork**: `gh repo fork`

## Process

### 1. Parse Intent

Determine what the user wants from `$ARGUMENTS`:
- If a specific action → execute it directly
- If ambiguous → ask for clarification
- If empty → show available operations

### 2. Verify Prerequisites

- Check `gh` is installed and authenticated: `gh auth status`
- Verify we're in a git repository: `git rev-parse --git-dir`
- For PR operations, check we're on the right branch

### 3. Execute

Run the appropriate `gh` commands. For complex operations (e.g., "create a PR for my changes"):
1. Check for uncommitted changes and offer to commit them
2. Push the current branch if needed
3. Create the PR with an appropriate title and body
4. Report the PR URL

### 4. Report

Show the result clearly:
- PR/issue URLs for created items
- Status summaries for list/view operations
- CI pass/fail with details for check operations

## Rules
- Always verify `gh auth status` before operations that need it
- Never force-push or delete branches without explicit user confirmation
- For PR creation, generate a meaningful title and body from the diff
- Respect branch protection rules — don't try to push directly to protected branches
- If `gh` is not installed, explain how to install it: `brew install gh` / `apt install gh`
