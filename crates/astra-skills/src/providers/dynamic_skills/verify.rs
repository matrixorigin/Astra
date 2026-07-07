pub fn skill_content() -> String {
    r#"---
name: verify
description: "Run project tests, linters, and type checks to verify code correctness — auto-detects project type"
version: "1.0.0"
allowed_tools:
  - bash
when_to_use: "User wants to confirm recent changes are correct: run the project's tests, linters, type checks, or formatter against the current state of the code"
category: quality
tags:
  - testing
  - quality
composition:
  composable: true
  idempotent: true
  max_duration_sec: 1800
---
# Verify: Comprehensive Code Verification

Run the project's verification toolchain to confirm code correctness.

**Working directory**: ${{CTX_WORK_DIR}}
**Project type**: ${{CTX_PROJECT_TYPE}}

## Step 1: Select Toolchain

Use the detected project type above. If blank or ambiguous, check for marker files (`Cargo.toml`, `package.json`, `go.mod`, etc.).

If the project has a `Makefile` with a `check` or `test` target, prefer that — it often wraps the correct sequence.

## Step 2: Run Checks (stop on first failure unless user asked to fix)

### Rust
```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

### Node.js / TypeScript
```
# Check package.json scripts first; fallback:
npx prettier --check .
npx eslint .
npx tsc --noEmit        # if TypeScript
npm test
```

### Go
```
gofmt -l .
go vet ./...
go test ./...
```

### Python
```
ruff format --check .    # or black --check .
ruff check .             # or flake8 .
mypy .                   # if configured
pytest
```

### Other
Read the `Makefile`, `justfile`, or CI config to find the correct commands.

## Step 3: Report

For each check: ✅ passed, ❌ failed (with error summary), or ⏭ skipped (tool not found).

## When NOT to Use
- Don't use for runtime/integration testing that requires external services
- If the user specifies a scope (file or module), run only relevant tests
"#.to_string()
}
