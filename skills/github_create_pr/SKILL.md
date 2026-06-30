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
    description: "Base branch. If omitted, use upstream tracking branch; fallback to main."
    required: false
  - name: LABELS
    description: "Comma-separated labels."
    required: false
  - name: REVIEWERS
    description: "Comma-separated reviewer usernames."
    required: false
allowed_tools:
  - bash
  - git
  - git_log
  - github
---

# GitHub Create PR

Create a pull request. No native create-PR tool exists, so this skill uses `gh` CLI.

## Task

$ARGUMENTS

---

## Phase 1: Pre-flight Checks

### 1.1 Verify `gh` CLI is available and authenticated

```bash
gh auth status --hostname github.com 2>&1 || true
```

**Interpretation**:

- Exit 0 with "Logged in to github.com" → authenticated, continue.
- Exit 1 but output still contains "Logged in to" → partial success (multi-account, one token expired). Use the account listed as "Logged in"; this is non-blocking.
- "not authenticated" or "not found" → stop and tell the user:
  - Not installed → `sudo apt install gh` or see https://cli.github.com
  - Not authenticated → `gh auth login`
  - Authenticated but wrong account → show current account, ask user to switch

### 1.2 Check current branch

```bash
git branch --show-current
```

If on `main` or the base branch, stop — nothing to create a PR from.

### 1.2.5 Auto-detect base branch from upstream tracking

If no `BASE` argument is provided, detect the upstream tracking branch instead of assuming `main`:

```bash
upstream=$(git rev-parse --abbrev-ref @{upstream} 2>/dev/null || true)
if [ -n "$upstream" ]; then
  base="${upstream#*/}"
else
  base="main"
fi
printf '%s\n' "$base"
```

Use the printed value as `BASE` throughout. Only fall back to `main` if upstream tracking is not configured.

### 1.3 Check for uncommitted changes

Use `git {action: "status"}`. If dirty, ask if user wants to commit first.

### 1.4 Verify the branch actually has commits to PR (empty-diff guard)

Before calling `gh pr create`, confirm the branch diverges from the base.
Use the `BASE` argument if the user supplied one; otherwise use the
upstream-detected base from step 1.2.5:

```bash
base="${BASE:-$(git rev-parse --abbrev-ref @{upstream} 2>/dev/null | sed 's|^[^/]*/||')}"
base="${base:-main}"
base_ref=$(git rev-parse "origin/${base}" 2>/dev/null || git rev-parse "${base}" 2>/dev/null || echo "")
git rev-list --count "${base_ref}..HEAD" 2>/dev/null || echo 0
```

If the count is `0` (or the command errors because the base ref is
unresolved), **STOP**:

- Tell the user the branch has no commits ahead of `${base}` — there is
  nothing to PR.
- Do not invoke `gh pr create` on an empty diff. GitHub will reject it with
  "No commits between ${base} and {branch}", and the failure is wasted tokens.
- Suggest running `git {action: "status"}` / `write_file` / `str_replace` first, or use
  a different `BASE` (e.g. `develop`, `master`) if the user intended a
  different target branch.

### 1.5 Check for existing PR from this branch

Use `github {action: "list_prs"}` with `state: "open"` to check. If a PR already exists from this branch, show it and ask whether to update or create a new one.

## Phase 2: Generate PR Content

### Title

If not provided, generate from recent commit messages:

If the user did NOT supply a `BASE` argument, run:

```bash
detected_base=$(git rev-parse --abbrev-ref @{upstream} 2>/dev/null | sed 's|^[^/]*/||')
detected_base="${detected_base:-main}"
git log "${detected_base}"..HEAD --oneline
```

Otherwise use the supplied `BASE`:

```bash
git log "${BASE}"..HEAD --oneline
```

Use conventional commit prefix if applicable: `feat:`, `fix:`, `refactor:`, `docs:`, `test:`, `chore:`.

### Body

If not provided, generate from diff summary using `git {action: "diff"}` with `stat_only: true`:

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
