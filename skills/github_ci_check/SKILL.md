---
name: github-ci-check
description: "Check CI/CD workflow status for a GitHub repository. Analyze failures, diagnose root causes, and provide actionable fix suggestions for failing jobs/steps."
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
  - read_file
  - grep
  - glob
---
# GitHub CI Check

Check CI/CD workflow status, diagnose failures, and provide actionable fix suggestions.

## Task

$ARGUMENTS

---

## Phase 1: Fetch CI Status

Use `github_ci_status` to get the latest workflow runs. Start with `detail: "detailed"` to get failed jobs and first failed steps.

```json
{"repo": "owner/repo", "detail": "detailed", "limit": 3}
```

If the result has `resolved_by_search: true`, note which repo was resolved.

## Phase 2: Analyze Failures

For each failed workflow run:

1. **Identify the failing job(s)** — look at `conclusion: "failure"` entries
2. **Find the first failed step** — this is usually the root cause; later steps may fail as cascading effects
3. **Categorize the failure**:
   - **Build failure**: compilation error, missing dependency, syntax error
   - **Test failure**: assertion failed, timeout, flaky test
   - **Lint/format failure**: clippy warning, rustfmt violation, make check
   - **Infrastructure**: rate limit, runner unavailable, network timeout
   - **Config**: missing secrets, wrong matrix, workflow syntax error

## Phase 3: Diagnose Root Cause

For each failure category:

### Build Failures
- Extract the compiler error message
- Identify the failing file and line number
- Check if it's a type mismatch, missing import, or API change
- Suggest the minimal fix

### Test Failures
- Identify the failing test name and assertion
- Check if the test is flaky (fails intermittently)
- Determine if it's a test bug or a real regression
- Suggest fix: update assertion, fix test setup, or fix production code

### Lint/Format Failures
- Identify the clippy lint or formatting violation
- For clippy: suggest the recommended fix (often just following the lint message)
- For rustfmt: suggest running `cargo fmt`
- For `make check`: check what the Makefile target does

### Infrastructure Failures
- Rate limit: suggest retry or reducing parallelism
- Runner issues: usually transient, suggest re-run
- Network timeout: check if it's a dependency download issue

## Phase 4: Report

Output a structured report:

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

## Phase 5: Local Verification (Optional)

If the user wants to verify locally before pushing:

- For build failures: `cargo check` or `cargo build`
- For test failures: `cargo test <test_name>` or `cargo test --package <crate>`
- For lint: `cargo clippy -- -D warnings`
- For format: `cargo fmt --check`
- For make check: `make check`
