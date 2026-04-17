---
name: github-create-issue
description: "Create a GitHub issue with proper title, description, labels, and assignees using `gh` CLI. Automates issue creation from user input or detected problems."
user_invocable: true
when_to_use: "When the user wants to create a GitHub issue, file a bug, report a problem, says 'create issue', 'file issue', 'open issue', or 'gh issue create'"
arguments:
  - name: TITLE
    description: "Issue title."
    required: true
  - name: BODY
    description: "Issue body/description. If omitted, generated from context."
    required: false
  - name: LABELS
    description: "Comma-separated labels (e.g., 'bug,high-priority')."
    required: false
  - name: ASSIGNEES
    description: "Comma-separated assignee usernames."
    required: false
allowed_tools:
  - bash
  - git_diff
  - read_file
  - grep
---
# GitHub Create Issue

Create a GitHub issue with proper title, description, labels, and assignees using the `gh` CLI.

## Task

$ARGUMENTS

---

## Phase 1: Validate Input

- `TITLE` is required — ask the user if not provided
- Determine issue type from context or user input:
  - **Bug**: something is broken
  - **Feature**: new functionality request
  - **Improvement**: enhancement to existing feature
  - **Task**: work item, refactoring, documentation

## Phase 2: Generate Issue Body

### Bug Report Template

```markdown
## Description

{clear description of the bug}

## Steps to Reproduce

1. {step 1}
2. {step 2}
3. {step 3}

## Expected Behavior

{what should happen}

## Actual Behavior

{what actually happens}

## Environment

- OS: {os}
- Rust: {version}
- Branch: {current_branch}

## Additional Context

{screenshots, logs, error messages, related issues}
```

### Feature Request Template

```markdown
## Problem Statement

{what problem does this feature solve?}

## Proposed Solution

{how should it work?}

## Alternatives Considered

{other approaches and why they were rejected}

## Additional Context

{references, examples, mockups}
```

## Phase 3: Suggest Labels

Based on issue type:

| Type | Suggested Labels |
|------|-----------------|
| Bug | `bug`, add priority label |
| Feature | `enhancement`, `feature-request` |
| Improvement | `improvement` |
| Task | `task` |

Priority labels: `critical`, `high-priority`, `medium-priority`, `low-priority`

## Phase 4: Create the Issue

```bash
gh issue create \
  --title "{title}" \
  --body "{body}" \
  --label "{labels}" \
  --assignee "{assignees}"
```

If `gh` is not available or not authenticated:
```bash
gh auth login
```

## Phase 5: Post-Creation

After successful issue creation:
1. Show the issue URL
2. Suggest linking related PRs (use `gh pr create` with `--issue {number}`)

### If Issue Creation Fails

Common issues:
- **Not authenticated**: `gh auth login`
- **Duplicate**: search existing issues first
- **Permission denied**: user may not have write access to the repo
