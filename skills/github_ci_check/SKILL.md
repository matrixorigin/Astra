---
name: github-ci-check
description: "Check CI/CD workflow status for a GitHub repository. Analyze failures, diagnose root causes, and provide actionable fix suggestions."
user_invocable: true
when_to_use: "When the user asks to check CI status, analyze CI failures, debug test failures in GitHub Actions, or says 'check CI', 'ci status', 'why is CI failing'"
arguments:
  - name: REPO
    description: "Repository as 'owner/repo' or bare project name. Defaults to current repo."
    required: false
  - name: DETAIL
    description: "Output detail: 'brief' (default), 'normal', 'detailed', or 'full'."
    required: false
allowed_tools:
  - github_ci_status
  - github_get_pr
  - read_file
  - grep
  - glob
  - bash
---
# GitHub CI Check

Check CI/CD workflow status, diagnose failures, and provide actionable fix suggestions.

## Task

$ARGUMENTS

---

## Phase 1: Resolve Repository

1. **If the user provided a full GitHub URL** (e.g., `https://github.com/owner/repo/actions`), parse `owner/repo` directly from the URL. This takes absolute priority.
2. If `REPO` is provided, use it directly
3. Otherwise, detect from git remote:
   ```bash
   git remote get-url origin 2>/dev/null
   ```
   Parse `owner/repo` from the URL. If detection fails, ask the user.

## Phase 2: Fetch CI Status

Use `github_ci_status` with `detail: "detailed"` to get failed jobs and first failed steps.

```json
{"repo": "owner/repo", "detail": "detailed", "limit": 3}
```

**If the tool returns an error:**
- "requires a configured GitHub client" → fall back to `bash`:
  ```bash
  gh run list --repo owner/repo --limit 3 --json name,conclusion,headBranch,startedAt,updatedAt,event 2>&1
  ```
- If `gh` also fails (not installed, not authenticated, no permission), report the error and stop.

If `resolved_by_search: true` in the result, note which repo was resolved.

## Phase 3: Analyze Failures

For each failed workflow run:

1. **Identify the failing job(s)** — `conclusion: "failure"` entries
2. **Find the first failed step** — usually the root cause; later steps cascade
3. **Categorize**:
   - **Build failure**: compilation error, missing dependency
   - **Test failure**: assertion failed, timeout, flaky test
   - **Lint/format failure**: clippy warning, rustfmt violation
   - **Infrastructure**: rate limit, runner unavailable, network timeout
   - **Config**: missing secrets, workflow syntax error

## Phase 4: Diagnose Root Cause

### Build Failures
- Extract compiler error message, file, and line
- Suggest the minimal fix

### Test Failures
- Identify failing test name and assertion
- Determine if flaky or real regression

### Lint/Format Failures
- Identify the clippy lint or formatting violation
- Suggest: `cargo fmt` or follow the lint message

### Infrastructure Failures
- Usually transient — suggest re-run

## Phase 5: Report

```markdown
## CI Status: {repo}

| Workflow | Branch | Status | Duration |
|----------|--------|--------|----------|
| {name} | {branch} | {conclusion} | {duration} |

### 🔴 Failing Jobs ({n})

#### {workflow_name} / {job_name}

**Step:** {step_name}
**Error:** {error_message}
**Root cause:** {analysis}
**Suggested fix:** {actionable suggestion}
```

### Severity Guide

| Severity | Criteria |
|----------|----------|
| 🔴 Blocking | Build failure, test regression, security scan fail |
| 🟡 Fix Soon | Lint warning, flaky test, deprecation notice |
| 💡 Info | Infrastructure retry, cosmetic CI output |
