---
name: init-project
description: "Set up a new project or configure an existing one — scaffolding, deps, CI, tooling"
version: "1.0.0"
triggers:
  - init
  - "new project"
  - scaffold
  - "set up"
  - bootstrap
  - "create project"
when_to_use: "When the user wants to create a new project from scratch or add standard tooling/configuration to an existing project"
category: scaffolding
arguments:
  - name: SPEC
    description: "Project description, language/framework, or specific tooling to set up"
    required: true
tags:
  - scaffolding
  - setup
  - project
---
# Project Initialization

Set up a new project or configure an existing one with best-practice tooling.

## Specification

$ARGUMENTS

## Process

### 1. Determine Scope

Parse `$ARGUMENTS` to understand:
- **New project or existing?** Check if current directory has existing code
- **Language/framework**: Rust, TypeScript, Python, Go, etc.
- **Project type**: library, CLI, web app, API server, monorepo
- **Specific requests**: CI setup, Docker, testing framework, linting

### 2. Scaffold Structure

For a new project, create the standard structure:

**Rust**: `cargo init`, workspace layout if needed, `src/lib.rs` + `src/main.rs`
**TypeScript**: `package.json`, `tsconfig.json`, `src/`, ESM configuration
**Python**: `pyproject.toml`, `src/<package>/`, `tests/`
**Go**: `go mod init`, `cmd/`, `internal/`, `pkg/`

### 3. Configure Tooling

Set up development essentials:
- **Formatter**: rustfmt / prettier / black / gofmt
- **Linter**: clippy / eslint / ruff / golangci-lint
- **Testing**: cargo test / vitest / pytest / go test
- **Type checking**: cargo check / tsc / mypy
- **Git hooks**: pre-commit (format + lint check)
- **Editor config**: `.editorconfig`, relevant IDE settings

### 4. CI/CD (if requested)

Create GitHub Actions workflow (`.github/workflows/ci.yml`):
- Format check
- Lint
- Type check
- Test (with coverage if easily available)
- Build

### 5. Documentation

Create minimal but useful docs:
- `README.md`: project name, description, how to build/run/test
- `CONTRIBUTING.md` (if collaborative): setup, conventions, PR process
- `.gitignore`: language-appropriate exclusions

### 6. Verify

- Build the project to confirm it compiles
- Run the test suite (even if empty) to confirm tooling works
- Commit the initial scaffold

## Rules
- Use the latest stable versions of all tools and dependencies
- Don't over-engineer the initial setup — start simple, grow as needed
- Follow the language community's standard project layout
- If `$ARGUMENTS` is empty, ask what kind of project the user wants
- Prefer well-maintained, widely-used tools over obscure alternatives
- Include a `.gitignore` from day one
