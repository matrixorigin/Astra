---
inclusion: always
---

# astra-engine Development & Testing Guide

You are working on astra-engine, a Rust-first agent platform. Your primary tasks are development, testing, and debugging.

## Quick Reference

```bash
make build              # Release workspace build
make test               # Unit + contract tests (no live DB)
make test-integration   # Full E2E with MatrixOne
make check              # lint + format + type checks (run before commit)
make lint               # cargo clippy --workspace -- -D warnings
make dev-start          # Start deps + API server
make dev-stop           # Stop all
```

## Workspace Structure

- `rust/crates/core/` — shared types
- `rust/crates/services/` — sessions, journals, durable tasks, cloud sync
- `rust/crates/runtime/` — Axum HTTP server, contract tests in `tests/`
- `rust/crates/astra-cli/` — CLI, edge tools, plan executor, code intel
- `rust/crates/astra-admin/` — admin CLI
- `skills/` — Agent Skills (SKILL.md format, see `skills/README.md`)
- `web/` — Next.js admin dashboard

## Rust Conventions

- Edition 2024, clippy warnings = errors
- Error types: `thiserror`, async: Tokio, HTTP: Axum 0.8, DB: SQLx (MySQL/MatrixOne)
- No `unsafe` without safety comments
- Contract tests: `rust/crates/runtime/tests/`

## Development Rules (MANDATORY)

### 1. Error Handling — Standardized, Highest Priority

Every error MUST be traceable and locatable. No silent failures.

- All errors must carry structured context: operation, entity ID, source error
- Use `thiserror` with descriptive variants — never `anyhow` in library code
- Every error variant must include enough context to locate the bug without a debugger
- Log at the boundary (HTTP handler, CLI entry), not deep in the call stack
- Never swallow errors: no `let _ = result;`, no empty `catch`, no `if let Ok(x) = ...` that ignores Err
- Error responses must include: error code, human message, request_id/trace_id for correlation

```rust
// ✅ Good: traceable, locatable
#[derive(thiserror::Error, Debug)]
enum SessionError {
    #[error("session {session_id} not found for user {user_id}")]
    NotFound { session_id: String, user_id: String },
    #[error("session {session_id} already closed at {closed_at}")]
    AlreadyClosed { session_id: String, closed_at: String },
    #[error("db query failed for session {session_id}: {source}")]
    Database { session_id: String, source: sqlx::Error },
}

// ❌ Bad: untraceable
return Err("something went wrong".into());
```

### 2. Persistent State — Traceable & Contextual

All persisted state must form a coherent, navigable chain for debugging and analysis.

- Every state-changing operation must be traceable: who, when, what, why
- Related records must be linkable: session → events → tasks → runs via foreign keys
- State transitions must be explicit: use enum status fields, not implicit flags
- Timestamps on every mutable row: `created_at`, `updated_at`
- Include `request_id` / `trace_id` in state records for cross-system correlation
- When reading persisted state for debugging, the full context chain must be reconstructable

### 3. Database Schema — Well-Designed for Workload

Schemas must match their actual read/write/query patterns.

- Design indexes for the actual query patterns, not just primary keys
- Separate hot (frequently updated) and cold (append-only) data
- Use appropriate column types: don't store UUIDs as TEXT if the DB supports UUID type
- Every table must have a clear owner (which service writes to it)
- Schema changes must be backward compatible — old code must not break on new schema

### 4. Vector & Full-Text Queries

- **Distance function consistency**: the distance function in similarity queries MUST match the index definition (e.g., if index uses cosine, query must use cosine — not L2)
- **Vector/full-text table mutations are slow**: avoid frequent UPDATE/DELETE on vector and full-text indexed tables. Prefer append + soft-delete patterns
- Design for batch insert, not row-by-row

### 5. SQL Performance

- **No JSON column filtering**: avoid WHERE clauses on JSON columns — they cause full table scans
- **Minimize full table scans**: ensure WHERE clauses hit indexes
- **Projection pruning**: SELECT only needed columns, never `SELECT *` in production code
- **Use `EXPLAIN ANALYZE`**: when unsure about query efficiency, verify the execution plan
- MatrixOne is MySQL-protocol HTAP with vector, full-text, git4data, stage, pubsub, datalink support

### 6. Testing — Parallel with Isolation

- Tests should run in parallel by default (`cargo test` parallelism)
- Each test must be fully isolated: unique user IDs, session IDs, table prefixes, or separate transactions
- Never depend on test execution order
- Never share mutable state between tests without synchronization
- For DB tests: each test creates its own data with unique identifiers, cleans up after itself
- E2E tests must verify DB state directly (SELECT after mutation), not just trust HTTP responses

## Testing Workflow

1. `make build` — verify compilation
2. `make test` — run workspace tests
3. `make lint` — clippy clean
4. `make format-check` — formatting
5. For DB-dependent tests: `make dev-start` then `make test-integration`

## Built-in Skills (read `skills/<name>/SKILL.md` for full instructions)

When the user asks you to perform any of the following tasks, read the corresponding SKILL.md file first and follow its phased workflow:

- **Review code changes**: `skills/review_changes/SKILL.md` — context-aware code review combining git diff with symbol-level impact analysis.
- **Review code (test quality)**: `skills/review_code/SKILL.md` — code review focused on unhappy paths, error scenarios, E2E test coverage with real DB assertions.
- **Verify task completion**: `skills/verify_task/SKILL.md` — run build/test/lint/grep checks against acceptance criteria, produce a delivery report.
- **Batch parallel execution**: `skills/batch_parallel/SKILL.md` — execute independent tasks in parallel using git worktrees.
- **Analyze session**: `skills/analyze_session/SKILL.md` — diagnose astra session issues (token waste, tool selection, stalls, loops).
- **Evaluate session**: `skills/evaluate_session/SKILL.md` — evaluate session performance metrics and optimization recommendations.
- **Optimize prompt**: `skills/optimize_prompt/SKILL.md` — analyze LLM prompt assembly to reduce context bloat.
- **Audit cloud sync**: `skills/audit_cloud_sync/SKILL.md` — audit edge-cloud sync integrity.
- **Trace delegation**: `skills/trace_delegation/SKILL.md` — trace multi-agent delegation flows.
