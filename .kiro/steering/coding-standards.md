---
inclusion: always
---

# Project-Specific Coding Standards

## Core Principles

- Prefer explicit, typed boundaries over clever shortcuts.
- Keep modules cohesive and named by domain responsibility.
- Preserve behavior while refactoring; improve layout without changing semantics unless intended.
- Reuse existing helpers before adding near-duplicates.

## Rust Expectations

- Use `Result`-based error flow; do not silently swallow failures.
- Prefer focused structs/enums over loosely shaped maps when the schema is known.
- Keep crate roots thin; move owned logic into domain modules.
- Use parameterized SQL and existing storage helpers.
- Keep re-exports intentional and stable when reducing churn matters.

## Documentation and Naming

- Name tests after current runtime behavior, not legacy migration history.
- Update adjacent docs when behavior or structure meaningfully changes.
- Avoid language-specific examples that no longer match the codebase.

## Validation Before Handoff

```bash
make format-check
make type-check
make lint
make test
```
