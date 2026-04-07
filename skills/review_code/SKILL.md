---
name: review-code
description: "Code review skill focused on test quality: unhappy paths, error scenarios, E2E coverage with real database assertions, and production-grade test completeness."
user_invocable: true
when_to_use: "When reviewing code changes, PRs, or evaluating test quality — especially to ensure unhappy paths, edge cases, and database-level verification are covered"
arguments:
  - name: TARGET
    description: "What to review: 'staged', 'unstaged', 'branch:<name>', 'commit:<sha>', or file paths. Default: all uncommitted changes."
    required: false
  - name: FOCUS
    description: "Review focus: 'tests', 'unhappy', 'e2e', 'db', or 'all' (default: all)"
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
---
# Review Code — Test Quality & Unhappy Path Focus

Code review skill that goes beyond "does it compile and pass happy path." The core question for every change: **what happens when things go wrong, and do the tests prove it?**

## Task

$ARGUMENTS

---

## Phase 1: Gather Changes

Use git to identify what changed:

```bash
# Stat overview first
git diff --stat HEAD
git diff --stat --staged

# Full diff for analysis
git diff HEAD
```

For branch/commit targets, adjust accordingly. Assess scope before deep dive — for large diffs, focus on core logic and test files.

---

## Phase 2: Happy Path Audit

For each changed function/endpoint, verify the happy path test exists. But this is the **minimum bar**, not the goal.

Checklist:
- [ ] Does a test call the function with valid input and assert the expected output?
- [ ] Does the test assert on **specific values**, not just "no error"?
- [ ] For HTTP endpoints: is the response body checked, not just status code?
- [ ] For DB operations: is the row actually queried back and verified?

Flag as 🟡 if happy path test exists but only checks `is_ok()` / `status == 200` without asserting the actual data.

---

## Phase 3: Unhappy Path Review (Critical)

This is the core of this skill. For every changed function, systematically check:

### 3.1 Error Input Scenarios

- **Invalid input**: empty strings, zero values, negative numbers, oversized payloads
- **Missing required fields**: null/None where not expected, missing headers
- **Malformed data**: bad JSON, wrong types, truncated input
- **Boundary values**: MAX_INT, empty collections, single-element edge cases

### 3.2 Failure Propagation

- **Downstream failures**: what if the DB is down? What if an HTTP call times out?
- **Partial failures**: what if step 2 of 3 fails — is step 1 rolled back?
- **Concurrent failures**: what if two requests race on the same resource?
- **Resource exhaustion**: connection pool full, disk full, memory pressure

### 3.3 State Corruption Scenarios

- **Double operations**: create twice, delete twice, close an already-closed session
- **Out-of-order operations**: resume before pause, complete before start
- **Stale state**: operate on a resource that was modified by another request
- **Orphaned resources**: parent deleted but children remain

### 3.4 Auth & Permission Failures

- **No auth**: request without token → 401
- **Expired token**: request with expired JWT → 401
- **Wrong role**: non-admin hitting admin endpoint → 403
- **Cross-tenant**: user A accessing user B's resource → 403 or 404

For each category, check:
1. Does a test exist for this scenario?
2. Does the test assert the **specific error** (error code, message), not just "it failed"?
3. Is the error response well-formed (proper HTTP status, error body)?

Flag severity:
- 🔴 No unhappy path test for a public API endpoint
- 🔴 Error case exists in code but no test proves it works
- 🟡 Test exists but only checks `is_err()` without asserting error type/message

---

## Phase 4: Test Quality Assessment

### 4.1 Test Realism

Bad tests (flag as 🟡):
- Tests that mock everything — they prove the mocks work, not the code
- Tests with `assert!(true)` or no assertions at all
- Tests that only check one field of a complex response
- Tests that use hardcoded magic values without explanation

Good tests (acknowledge as ✅):
- Tests that use the real stack (real HTTP server, real DB) where feasible
- Tests that set up realistic preconditions, not just empty state
- Tests that assert on multiple aspects of the result
- Tests that verify side effects (DB rows, events emitted, logs)

### 4.2 E2E Test Coverage

For any feature touching HTTP endpoints + database:

**Required E2E pattern** (based on this project's `system_matrix_http_e2e` style):
1. Bootstrap real Axum app with real DB connection
2. Perform the operation via HTTP (not by calling internal functions)
3. Assert HTTP response status AND body
4. Query the database directly (via SQLx) to verify the row was actually written/updated/deleted
5. Verify cross-table consistency (e.g., session created → events table has entry)

```rust
// GOOD: E2E with DB assertion
let resp = client.post("/sessions").json(&body).send().await;
assert_eq!(resp.status(), 200);
let session: Session = resp.json().await;
assert_eq!(session.status, "active");

// Verify in DB
let row = sqlx::query("SELECT status FROM sessions WHERE id = ?")
    .bind(&session.id)
    .fetch_one(&pool).await.unwrap();
assert_eq!(row.get::<String, _>("status"), "active");
```

```rust
// BAD: Only checks HTTP, trusts the response blindly
let resp = client.post("/sessions").json(&body).send().await;
assert_eq!(resp.status(), 200);  // What if response says 200 but DB write failed?
```

Flag as 🔴 if:
- New endpoint has no E2E test at all
- E2E test exists but never queries the database
- DB-mutating operation only tested via unit test with mocked DB

### 4.3 Database Verification Checklist

For any code that writes to the database:

- [ ] Test verifies the row exists after insert (`SELECT` + assert)
- [ ] Test verifies the row is updated after update (check changed fields AND unchanged fields)
- [ ] Test verifies the row is gone after delete (`fetch_optional` returns `None`)
- [ ] Test verifies constraints: unique violations return proper error, not panic
- [ ] Test verifies cascade: deleting parent affects children correctly
- [ ] Test verifies isolation: user A's operation doesn't affect user B's data

### 4.4 Complex Scenario Coverage

Beyond single-operation tests, check for multi-step journey tests:

- **Full lifecycle**: create → use → modify → close → verify final state
- **Concurrent access**: two clients operating on same resource
- **Recovery**: crash mid-operation → restart → verify consistent state
- **Migration**: old data format still works after schema change

Reference: this project's `journey_full.rs` is the gold standard — it walks through sessions → agents → events → chat turns → SSE → logout, asserting DB state at each step.

---

## Phase 5: Specific Patterns to Flag

### 5.1 Rust-Specific Anti-Patterns in Tests

```rust
// 🔴 unwrap() in test setup without context — when it fails, you get no info
let result = do_thing().unwrap();

// ✅ Better: expect() with context
let result = do_thing().expect("do_thing should succeed with valid input");

// 🔴 Ignoring the error variant
assert!(result.is_ok());

// ✅ Better: match and assert the actual value
let value = result.expect("should succeed");
assert_eq!(value.name, "expected_name");

// 🔴 Testing error path without checking which error
assert!(result.is_err());

// ✅ Better: assert the specific error
let err = result.unwrap_err();
assert!(err.to_string().contains("not found"), "expected NotFound, got: {err}");
```

### 5.2 Missing Negative Tests for This Codebase

Based on the existing `nonhappy_path.rs` and `journey_extended.rs` patterns, flag if:
- New auth endpoint missing 401/403 tests
- New session operation missing "already closed" test
- New task operation missing "not found" / "already completed" test
- New SSE endpoint missing "connection drop" handling test
- Circuit breaker / stall detection not tested for the new code path

---

## Phase 6: Review Report

Use Markdown format (no ASCII box — it breaks with CJK/emoji width):

```markdown
---
title: "Code Review: {commit_or_branch}"
scope: "{n} files, +{added}/-{removed} lines"
---

## Test Coverage Summary

| Category | Coverage |
|----------|----------|
| Happy path | {covered}/{total} functions |
| Unhappy path | {covered}/{total} error scenarios |
| E2E with DB | {covered}/{total} endpoints |
| Journey/lifecycle | {yes/no} |

## Issues

### 🔴 Missing Tests ({n})

{for each issue}
- **{file}:{line}** — {what's missing and why it matters}
{end}

### 🟡 Weak Tests ({n})

{for each issue}
- **{file}:{line}** — {what's weak and how to strengthen}
{end}

## ✅ Strong Tests

{acknowledge well-written tests with brief praise}

## 📝 Recommended Tests to Add

{concrete test scenarios with pseudocode}
```

### Severity Guide

| Severity | Criteria |
|----------|----------|
| 🔴 Critical | Public API with no unhappy path test; DB mutation with no DB assertion; error path exists in code but untested |
| 🟡 Important | Test exists but weak (only `is_ok()`/`is_err()`); no E2E for endpoint that has unit test; missing boundary test |
| 💡 Suggestion | Could add concurrent test; could add journey test; could improve assertion messages |
| ✅ Strong | Real E2E with DB verification; comprehensive error scenarios; realistic multi-step journey |
