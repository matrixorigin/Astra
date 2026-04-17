---
name: github-pre-pr
description: "Pre-PR checklist automation: run make check, make format, cargo clippy, cargo fmt, and verify local tests pass before opening a pull request. Catches issues before CI."
user_invocable: true
when_to_use: "When the user wants to prepare a PR, says 'pre-pr check', 'before opening PR', 'pre-pr', or 'check before push'"
arguments:
  - name: SCOPE
    description: "What to check: 'full' (default), 'quick' (build only), or 'test' (build + test only)."
    required: false
allowed_tools:
  - bash
  - git_diff
  - read_file
  - grep
  - github_ci_status
---
# Pre-PR Checklist

Run all quality checks locally BEFORE opening a pull request. Catches issues before CI, saving time and avoiding failed PRs.

## Task

$ARGUMENTS

---

## Phase 1: Assess Scope

Use `git_diff` with `stat_only: true` to understand what changed. This helps determine which checks are most relevant.

## Phase 2: Run Checks

Execute checks in order of speed (fastest first). Stop on first failure and report.

### 2.1 Format Check

```bash
cargo fmt --check
```

If it fails, run `cargo fmt` to auto-fix, then re-check.

### 2.2 Lint Check

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

For project-specific lints, also check:
```bash
make check 2>/dev/null || echo "no make check target"
```

### 2.3 Build Check

```bash
cargo check --all-targets --all-features
```

### 2.4 Test Check

Run tests for changed crates only (from git diff):
```bash
# Extract changed crate names and run their tests
cargo test --package <crate_name>
```

For full test suite (slower):
```bash
cargo test --workspace
```

### 2.5 Integration Tests (if applicable)

```bash
cargo test --test '*' 2>/dev/null || echo "no integration tests"
```

## Phase 3: Report

Output a checklist:

```markdown
## Pre-PR Checklist

| Check | Status | Notes |
|-------|--------|-------|
| cargo fmt | ✅ / ❌ | {details} |
| cargo clippy | ✅ / ❌ | {details} |
| cargo check | ✅ / ❌ | {details} |
| cargo test | ✅ / ❌ | {details} |
| make check | ✅ / ❌ | {details} |

### Issues to Fix

{list any failures with file:line and error message}

### Recommendations

{suggestions for cleanup before opening PR}
```

### All Clear

If everything passes:
```markdown
✅ All pre-PR checks passed! Ready to open PR.
```

## Phase 4: Auto-Fix (When Possible)

For fixable issues:
- **Format**: `cargo fmt` (always safe)
- **Simple clippy**: if the lint suggests `--fix`, run `cargo clippy --fix --all-targets --allow-dirty`
- **Trivial build errors**: suggest the fix but don't auto-apply

## Scope Variants

| Scope | Runs |
|-------|------|
| `full` | fmt → clippy → check → test → make check |
| `quick` | fmt → check (fastest path to "will it compile?") |
| `test` | fmt → check → test (skip clippy) |
