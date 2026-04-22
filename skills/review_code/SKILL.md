---
name: review-code
description: "Code review skill focused on test quality: unhappy paths, error scenarios, E2E coverage with real database assertions, and production-grade test completeness."
user_invocable: true
when_to_use: "For test quality audit: unhappy paths, E2E coverage, DB assertions. Triggered by: 'check test coverage', 'test quality', 'unhappy paths covered', 'missing tests', 'E2E tests'. NOT for general code inspection."
arguments:
  - name: TARGET
    description: "What to review: 'staged', 'unstaged', 'branch:<name>', 'commit:<sha>', or file paths. Default: all uncommitted changes."
    required: false
  - name: FOCUS
    description: "Review focus: 'tests', 'unhappy', 'e2e', 'db', or 'all' (default: all)"
    required: false
allowed_tools:
  - git_diff
  - git_show
  - git_log
  - bash
  - read_file
  - grep
  - glob
---
# Review Code — Test Quality & Unhappy Path Focus

The core question for every change: **what happens when things go wrong, and do the tests prove it?**

## Task

$ARGUMENTS

---

## Step 1: Size Check

Call `git_diff(stat_only: true)` to get file list and line counts.

| Size | Next step |
|------|-----------|
| ≤ 80 lines, ≤ 5 files | Fetch full diff → go to Step 3 directly |
| > 80 lines or > 5 files | Go to Step 2 |

**TARGET resolution:**

| TARGET | Tool |
|--------|------|
| Default (uncommitted) | `git_diff()` |
| `staged` | `git_diff(staged: true)` |
| `branch:<name>` | `git_diff(ref: "main")` |
| `commit:<sha>` / `latest commit` | `git_log()` once → `git_show(sha)` |

---

## Step 2: Fetch Diff + Scan Signals

Fetch the full diff. Identify which test checks to run:

| Signal | Check |
|--------|-------|
| New/changed public function or endpoint | Happy path test exists? |
| Error handling code (`Err(`, `?`, `unwrap_or`) | Unhappy path test exists? |
| DB write (`INSERT`, `UPDATE`, `DELETE`, `execute`) | DB assertion test exists? |
| HTTP handler | E2E test with real server + DB? |
| Auth/permission check | 401/403 test exists? |
| State machine / lifecycle | Double-op and out-of-order tests? |

**Context budget:** at most 3 `read_file` calls. Use `grep` to find test files before reading them.

---

## Step 3: Report

**Output NOTHING while making tool calls.**

```
## Test Review: {target}
Scope: {n} files, +{added}/-{removed} lines

### 🔴 Missing Tests
- {file}:{line} — {what's missing and why it matters}

### 🟡 Weak Tests
- {file}:{line} — {what's weak: only is_ok()? no DB assertion? no error type check?}

### ✅ Strong Tests
{well-written tests worth acknowledging}

### 📝 Recommended Tests
{concrete scenarios to add, with pseudocode if helpful}
```

---

## Test Quality Checklist

**Unhappy paths** — for every changed error path, a test must:
- Trigger the specific error condition (not just call with valid input)
- Assert the specific error type/message, not just `is_err()`
- For HTTP: assert the status code AND error body

**E2E with DB** — for any HTTP endpoint that writes to DB:
```rust
// Required pattern
let resp = client.post("/endpoint").json(&body).send().await;
assert_eq!(resp.status(), 200);
// Must also verify in DB:
let row = sqlx::query("SELECT ... FROM table WHERE id = ?")
    .bind(&id).fetch_one(&pool).await.unwrap();
assert_eq!(row.get::<String, _>("field"), "expected");
```

**Rust test anti-patterns** (flag as 🟡):
- `assert!(result.is_ok())` — use `result.expect("context")` instead
- `assert!(result.is_err())` — assert the specific error variant
- `unwrap()` in test setup — use `expect("why this should succeed")`

**Severity:**
- 🔴 Public API endpoint with no unhappy path test; DB mutation with no DB assertion
- 🟡 Test exists but only checks `is_ok()`/`is_err()`; E2E test never queries DB
- 💡 Could add concurrent test; could add lifecycle journey test
