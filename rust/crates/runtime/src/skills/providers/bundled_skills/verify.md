---
name: verify
description: "Run project tests, linters, and type checks to verify code correctness — detect project type automatically"
version: "1.1.0"
triggers:
  - verify
  - check
  - validate
  - "run tests"
  - "make sure"
  - "does it pass"
when_to_use: "When the user wants to verify that recent changes haven't broken anything, or wants a comprehensive quality check"
category: quality
arguments:
  - name: SCOPE
    description: "Specific scope to verify (e.g., a module, test name, or file path)"
    required: false
tags:
  - testing
  - quality
  - ci
---
# Verify Skill

Run all relevant checks to confirm code correctness.

## Scope

$ARGUMENTS

## Process

### 1. Detect Project Type and Tools

Check for project markers and identify the correct tools:

| Marker | Language | Format | Lint | Type Check | Test | Build |
|--------|----------|--------|------|------------|------|-------|
| `Cargo.toml` | Rust | `cargo fmt --check` | `cargo clippy -- -D warnings` | `cargo check` | `cargo test` | `cargo build` |
| `package.json` | Node.js | `prettier --check .` | `eslint .` | `tsc --noEmit` | `npm test` / `bun test` | `npm run build` |
| `pyproject.toml` | Python | `black --check .` | `ruff check .` | `mypy .` | `pytest` | — |
| `go.mod` | Go | `gofmt -l .` | `golangci-lint run` | — | `go test ./...` | `go build ./...` |
| `Makefile` | Any | Check for `fmt` target | Check for `lint` target | Check for `check` target | Check for `test` target | Check for `build` target |

If a `Makefile` exists with relevant targets, prefer those over raw commands — the project may have custom configurations.

If `$ARGUMENTS` specifies a scope, focus checks on that area first for faster feedback.

### 2. Run Checks (in order — stop on first critical failure)

1. **Format check** — Fast, catches style issues
2. **Lint** — Catches common bugs and anti-patterns
3. **Type check** — Catches type errors (may overlap with build)
4. **Tests** — If scoped, run scoped tests first (`cargo test <module>`, `pytest -k <pattern>`) then full suite
5. **Build** — Ensures the project compiles/bundles correctly

For each check:
- Show the exact command being run
- Report pass/fail with timing
- On failure: show specific errors with file locations and line numbers

### 3. Report Results

Render a summary table:

| Check | Status | Time | Details |
|-------|--------|------|---------|
| Format | ✅ pass | 0.3s | — |
| Lint | ❌ fail | 1.2s | 3 errors in src/main.rs |
| Type | ⏭ skip | — | (blocked by lint) |
| Tests | ✅ pass | 4.1s | 42 passed, 0 failed |
| Build | ✅ pass | 2.0s | — |

### 4. Fix (if requested or if issues are simple)

- Address failures in order: format → lint → type → test → build
- Re-run each check after fixing to confirm
- Don't fix warnings unless explicitly asked
- For complex failures, explain the issue and suggest a fix rather than guessing

## Rules
- Run the most specific checks first (faster feedback loop)
- If a check tool isn't installed, note it and move on — don't fail the whole verification
- For large test suites, run only affected tests first when a scope is provided
- Report the total time taken for all checks
- If everything passes, say so clearly: "All checks pass ✅"
