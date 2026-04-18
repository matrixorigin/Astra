---
name: github-pre-pr
description: "Pre-PR checklist automation: run project-specific quality checks before opening a pull request. Catches issues before CI."
user_invocable: true
when_to_use: "When the user wants to prepare a PR, says 'pre-pr check', 'before opening PR', 'pre-pr', or 'check before push'"
arguments:
  - name: SCOPE
    description: "What to check: 'full' (default), 'quick' (build + lint only), or 'test' (build + test only)."
    required: false
allowed_tools:
  - bash
  - git_diff
  - git_status
  - read_file
  - grep
  - glob
---
# Pre-PR Checklist

Run quality checks locally BEFORE opening a pull request. Adapts to the project's build system.

## Task

$ARGUMENTS

---

## Phase 1: Detect Build System

Check what build tools the project uses, in order:

1. **Makefile** — check for `make check`, `make lint`, `make test`, `make format-check`:
   ```bash
   grep -E '^(check|lint|test|format|build)[^:]*:' Makefile 2>/dev/null | head -10
   ```
2. **package.json** — check for npm/yarn scripts:
   ```bash
   cat package.json 2>/dev/null | grep -E '"(test|lint|check|build|format)"' | head -10
   ```
3. **Cargo.toml** — Rust project, use cargo commands directly
4. **go.mod** — Go project
5. **pyproject.toml / setup.py** — Python project

**Always prefer project-specific targets** (e.g., `make check`) over generic commands (e.g., `cargo clippy`). The Makefile often wraps the right flags.

## Phase 2: Assess Scope

Use `git_diff` with `stat_only: true` to understand what changed. This helps determine which checks are most relevant.

## Phase 3: Run Checks

Execute checks in order of speed (fastest first). **Stop on first failure and report.**

### For projects with Makefile targets:

| Scope | Checks |
|-------|--------|
| `full` | `make format-check` → `make lint` → `make build` → `make test-offline` |
| `quick` | `make format-check` → `make lint` → `make build` |
| `test` | `make build` → `make test-offline` |

If a target doesn't exist, skip it and fall through to generic commands.

### Generic fallback (Rust):

| Order | Command |
|-------|---------|
| 1 | `cargo fmt --check` |
| 2 | `cargo clippy --workspace -- -D warnings` |
| 3 | `cargo check --workspace` |
| 4 | `cargo test --workspace` |

### Generic fallback (Node.js):

| Order | Command |
|-------|---------|
| 1 | `npm run lint` or `npx eslint .` |
| 2 | `npm run build` |
| 3 | `npm test` |

## Phase 4: Report

```markdown
## Pre-PR Checklist

| Check | Status | Notes |
|-------|--------|-------|
| format | ✅ / ❌ | {details} |
| lint | ✅ / ❌ | {details} |
| build | ✅ / ❌ | {details} |
| test | ✅ / ❌ | {details} |

### Issues to Fix
{list failures with file:line and error message}
```

If everything passes:
```
✅ All pre-PR checks passed! Ready to open PR.
```

## Phase 5: Auto-Fix (When Safe)

For fixable issues:
- **Format**: `cargo fmt` / `npx prettier --write .` (always safe)
- **Simple clippy**: `cargo clippy --fix --allow-dirty` (if lint suggests `--fix`)
- **Build/test errors**: suggest the fix but don't auto-apply
