---
name: github-create-issue
description: "Create a GitHub issue with proper title, description, and labels. Uses native github_create_issue tool with gh CLI fallback."
user_invocable: true
when_to_use: "When the user wants to create a GitHub issue, file a bug, report a problem, says 'create issue', 'file issue', 'open issue'"
arguments:
  - name: REPO
    description: "Repository as 'owner/repo'. Defaults to current repo."
    required: false
  - name: TITLE
    description: "Issue title."
    required: true
  - name: BODY
    description: "Issue body/description. If omitted, generated from context."
    required: false
  - name: LABELS
    description: "Comma-separated labels (e.g., 'bug,high-priority')."
    required: false
allowed_tools:
  - github_create_issue
  - github_list_issues
  - git_diff
  - read_file
  - grep
  - bash
---
# GitHub Create Issue

Create a GitHub issue with proper title, description, and labels.

## Task

$ARGUMENTS

---

## Phase 1: Resolve Repository

1. **If the user provided a full GitHub URL** (e.g., `https://github.com/owner/repo/issues`), parse `owner/repo` directly from the URL. This takes absolute priority.
2. If `REPO` is provided (must be `owner/repo` form), use it directly
3. Otherwise, detect from git remote:
   ```bash
   git remote get-url origin 2>/dev/null
   ```
   Parse `owner/repo` from the URL. If detection fails, ask the user.

## Phase 2: Determine Issue Type and Generate Body

If `BODY` is not provided, generate from context and user input.

Determine issue type:
- **Bug**: something is broken → use bug template
- **Feature**: new functionality → use feature template
- **Improvement**: enhancement → use improvement template

### Bug Template
```markdown
## Description
{clear description}

## Steps to Reproduce
1. {step}

## Expected Behavior
{what should happen}

## Actual Behavior
{what actually happens}
```

### Feature Template
```markdown
## Problem Statement
{what problem does this solve?}

## Proposed Solution
{how should it work?}

## Alternatives Considered
{other approaches}
```

## Phase 3: Suggest Labels

| Type | Suggested Labels |
|------|-----------------|
| Bug | `bug` |
| Feature | `enhancement` |
| Improvement | `improvement` |

## Phase 4: Create the Issue

**Primary — native tool:**
```json
{"repo": "owner/repo", "title": "...", "body": "...", "labels": "..."}
```

**If native tool fails** ("requires a configured GitHub client" or auth error):

Fall back to `gh` CLI:
```bash
gh issue create --repo owner/repo --title "..." --body "..." --label "..." 2>&1
```

**If `gh` also fails**, report the error with guidance:
- Not authenticated → suggest `gh auth login`
- Permission denied → user may not have write access
- Not installed → provide the issue content for manual creation

## Phase 5: Post-Creation

After successful creation:
1. Show the issue URL
2. If created via `gh` fallback, note that native tool needs `GITHUB_TOKEN` configured for future use
