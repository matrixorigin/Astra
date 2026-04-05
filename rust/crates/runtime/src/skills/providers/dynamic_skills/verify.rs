pub fn skill_content() -> String {
    format!(
        r#"---
name: verify
description: "Run project tests, linters, and type checks to verify code correctness — auto-detects project type"
version: "2.0.0"
allowed_tools:
  - bash
triggers:
  - verify
  - validate
  - "run tests"
  - "does it pass"
when_to_use: "When the user wants to verify that recent changes haven't broken anything, or wants a comprehensive quality check across the whole project"
category: quality
tags:
  - testing
  - quality
---
# Verify: Comprehensive Code Verification

Run the project's verification toolchain to confirm code correctness.

**Working directory**: ${{CTX_WORK_DIR}}

## Step 1: Detect Project Type

**Success criteria**: Identified the project type and available tooling.

Check for these markers (in order) and use the FIRST match:

| Marker | Project Type | 
|--------|-------------|
| `Cargo.toml` | Rust |
| `package.json` | Node.js (check for `bun.lockb` → Bun, else npm/yarn) |
| `go.mod` | Go |
| `pyproject.toml` or `requirements.txt` | Python |
| `build.gradle` or `pom.xml` | Java/Kotlin |
| `*.csproj` or `*.sln` | .NET (C#/F#) |
| `Gemfile` | Ruby |
| `Makefile` | Make-based (read targets to infer language) |

If the project has a `Makefile` with a `check` or `test` target, prefer that — it often wraps the correct sequence.

## Step 2: Run Checks (in order)

Execute each step. **Stop on first failure and report it.** If the user asked you to fix issues, fix them; otherwise report and continue to the next check.

### 2a. Format Check
| Project | Command |
|---------|---------|
| Rust | `cargo fmt --check` |
| Node | Check `package.json` scripts for `format`/`prettier` first; fallback `npx prettier --check .` |
| Go | `gofmt -l .` |
| Python | `ruff format --check .` or `black --check .` |

### 2b. Lint
| Project | Command |
|---------|---------|
| Rust | `cargo clippy --all-targets -- -D warnings` |
| Node | Check `package.json` scripts for `lint` first; fallback `npx eslint .` |
| Go | `golangci-lint run` or `go vet ./...` |
| Python | `ruff check .` or `flake8 .` |

### 2c. Type Check (if applicable)
| Project | Command |
|---------|---------|
| Node (TS) | `npx tsc --noEmit` |
| Python | `mypy .` or `pyright .` |

### 2d. Test
| Project | Command |
|---------|---------|
| Rust | `cargo test` |
| Node | `npm test` / `bun test` / project's test script |
| Go | `go test ./...` |
| Python | `pytest` or `python -m unittest discover` |

### 2e. Build (if applicable)
| Project | Command |
|---------|---------|
| Rust | `cargo build` |
| Node | project's build script if present |
| Go | `go build ./...` |

## Step 3: Report Results

**Success criteria**: Clear report with pass/fail status for each check.

For each check, report: ✅ passed, ❌ failed (with error summary), or ⏭ skipped (tool not found).

## When NOT to Use
- Don't use for runtime/integration testing that requires external services
- If the user specifies a SCOPE (file or module), run only relevant tests, not the full suite
"#,
    )
}
