---
name: github-create-pr
description: "Create a pull request with proper title, description, labels, and reviewers. Uses gh CLI (no native create-PR tool exists)."
user_invocable: true
when_to_use: "When the user wants to create/open a PR, says 'create PR', 'open PR', 'make PR'"
arguments:
  - name: TITLE
    description: "PR title. If omitted, generated from commit messages."
    required: false
  - name: BODY
    description: "PR body/description. If omitted, generated from diff."
    required: false
  - name: BASE
    description: "Base branch (default: main)."
    required: false
  - name: LABELS
    description: "Comma-separated labels."
    required: false
  - name: REVIEWERS
    description: "Comma-separated reviewer usernames."
    required: false
allowed_tools:
  - bash
  - git_diff
  - git_status
  - git_log
  - github_list_prs
  - github_get_pr
---
# GitHub Create PR

Create a pull request. No native create-PR tool exists, so this skill uses `gh` CLI.

## Task

$ARGUMENTS

---

## Phase 1: Pre-flight Checks

### 1.1 Verify `gh` CLI is available and authenticated
```bash
gh auth status 2>&1
```
If not authenticated or not installed, stop and tell the user:
- Not installed → `sudo apt install gh` or see https://cli.github.com
- Not authenticated → `gh auth login`
- Authenticated but wrong account → show current account, ask user to switch

### 1.2 Check current branch
```bash
git branch --show-current
```
If on `main` or the base branch, stop — nothing to create a PR from.

### 1.3 Check for uncommitted changes
Use `git_status`. If dirty, ask if user wants to commit first.

### 1.4 Check for existing PR from this branch
Use `github_list_prs` with `state: "open"` to check. If a PR already exists from this branch, show it and ask whether to update or create a new one.

## Phase 2: Generate PR Content

### Title
If not provided, generate from recent commit messages:
```bash
git log main..HEAD --oneline
```
Use conventional commit prefix if applicable: `feat:`, `fix:`, `refactor:`, `docs:`, `test:`, `chore:`.

### Body
If not provided, generate from diff summary using `git_diff` with `stat_only: true`:

```markdown
## Summary
{what changed and why}

## Changes
- {key change 1}
- {key change 2}

## Testing
{how it was tested}
```

## Phase 3: Create the PR

```bash
gh pr create \
  --title "{title}" \
  --body "{body}" \
  --base "{base_branch}" \
  --label "{labels}" \
  --reviewer "{reviewers}" 2>&1
```

Omit `--label` / `--reviewer` flags if not provided (empty flags cause errors).

**If creation fails:**
- "not found" or 403 → user may not have push access to the remote
- "already exists" → show existing PR URL
- Network error → suggest retry

## Phase 4: Post-Creation

After successful creation:
1. Show the PR URL
2. Suggest running pre-PR checks if not already done (reference `github-pre-pr` skill)
3. Suggest checking CI status after push (reference `github-ci-check` skill)
