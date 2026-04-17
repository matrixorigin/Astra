---
name: github-create-pr
description: "Create a pull request using `gh` CLI with proper title, description, labels, reviewers, and base branch. Automates PR creation from local changes."
user_invocable: true
when_to_use: "When the user wants to create/open a PR, says 'create PR', 'open PR', 'make PR', or 'gh pr create'"
arguments:
  - name: TITLE
    description: "PR title. If omitted, generated from commit message."
    required: false
  - name: BODY
    description: "PR body/description. If omitted, generated from commit diff summary."
    required: false
  - name: BASE
    description: "Base branch (default: main)."
    required: false
  - name: LABELS
    description: "Comma-separated labels to add."
    required: false
  - name: REVIEWERS
    description: "Comma-separated reviewer usernames."
    required: false
allowed_tools:
  - bash
  - git_diff
  - git_status
  - github_list_prs
---
# GitHub Create PR

Create a pull request using the `gh` CLI with proper title, description, labels, and reviewers.

## Task

$ARGUMENTS

---

## Phase 1: Pre-flight Checks

1. **Check current branch** — ensure not on `main` or the base branch
2. **Check for uncommitted changes** — use `git_status`
   - If dirty with uncommitted changes, ask if user wants to commit first
3. **Check for existing PR** — use `github_list_prs` to see if a PR already exists from this branch
   - If yes, show it and ask if user wants to create another or update the existing one

## Phase 2: Generate PR Content

### Title

Generate from the most recent commit message or current changes:
- Use conventional commit prefix if applicable: `feat:`, `fix:`, `refactor:`, `docs:`, `test:`, `chore:`
- Keep it concise (under 72 chars)

### Body

Generate from the diff summary:
- **What changed**: brief description of the changes
- **Why**: rationale (from commit message or inferred from changes)
- **Testing**: how it was tested
- **Breaking changes**: if any

Format:
```markdown
## Summary

{what changed and why}

## Changes

- {key change 1}
- {key change 2}

## Testing

{how it was tested}

## Notes

{any additional context, breaking changes, migration notes}
```

## Phase 3: Create the PR

Use `gh pr create` with the generated content:

```bash
gh pr create \
  --title "{title}" \
  --body "{body}" \
  --base "{base_branch}" \
  --label "{labels}" \
  --reviewer "{reviewers}"
```

If `gh` is not available or not authenticated:
```bash
# Guide the user through setup
gh auth login
gh pr create ...
```

## Phase 4: Post-Creation

After successful PR creation:
1. Show the PR URL
2. Suggest running pre-PR checks if not already done (reference `github-pre-pr` skill)
3. Suggest checking CI status after creation (reference `github-ci-check` skill)

### If PR Creation Fails

Common issues:
- **Not authenticated**: `gh auth login`
- **No changes**: nothing to push
- **Already exists**: show existing PR URL
- **Base branch not found**: ask user for correct base branch

## Arguments Handling

| Argument | Source |
|----------|--------|
| `TITLE` | User input → commit message → auto-generate |
| `BODY` | User input → commit body → auto-generate from diff |
| `BASE` | User input → `main` (default) |
| `LABELS` | User input → inferred from commit prefix |
| `REVIEWERS` | User input → from CODEOWNERS or ask |
