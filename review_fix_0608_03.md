## Code Review: Branch `fix_0608_03` — First-Principles Design & Unhappy Path Coverage

**Scope:** 110 files, +18,065 / -4,024 lines (35 commits)  
**Focus:** Safety invariants, lifecycle correctness, validation completeness

---

### 🔴 Critical

#### 1. **shell_ops.rs:602-644 — Detach handle not restored on error path**

**File:** `crates/astra-tools/src/shell_ops.rs`  
**Problem:** The detach handle lifecycle fix (commit `bbfcea67`) correctly restores the handle on the `Completed` path but fails to restore it on error paths. If `run_bash_with_output` returns `Err`, the handle remains consumed, preventing subsequent bash calls in the same turn from being detachable.

**First-principles analysis:**  
The code uses a manual take/restore pattern instead of RAII scope guards. The invariant "handle is available for the next bash call" should be enforced structurally, not procedurally. The current implementation violates the principle that resource cleanup must be exception-safe.

**Evidence from diff:**
```rust
let detach_handle = if let Some(slot) = ctx.detach_shell_handle.as_ref() {
    slot.lock().await.take()  // Handle removed from slot
} else {
    None
};

match run_bash_with_output(...).await {
    Ok(BashRunOutcome::Completed(output)) => {
        if let Some(slot) = detach_slot.as_ref() {
            *slot.lock().await = Some(detach_handle);  // ✓ Restored on success
        }
        output
    }
    Ok(BashRunOutcome::Detached(payload)) => {
        // Handle consumed by payload ✓
        ...
    }
    Err(e) => {
        // ✗ Handle NOT restored — resource leak
        return Err(e);
    }
}
```

**Fix recommendation:**
```rust
let _restore_guard = scopeguard::guard(detach_handle, |handle| {
    if let Some(slot) = detach_slot.as_ref() {
        *slot.lock().await = Some(handle);
    }
});
```
Or manually restore in a `finally` block. The guard approach is preferred because it makes the invariant structural.

---

#### 2. **shell_ops.rs:472 — rm -rf protection delegates to external crate without validation**

**File:** `crates/astra-tools/src/shell_ops.rs:472`  
**Problem:** The function `is_rm_catastrophic_rm_path` is imported from `astra_sandbox` but the actual implementation is not visible in this branch. The tests show it blocks root/home/system paths, but the logic is opaque. If the sandbox crate has a bug or is updated independently, this code silently weakens.

**First-principles analysis:**  
Safety-critical validation should be explicit and auditable at the call site. Delegating to an external crate is acceptable if:
1. The crate is version-pinned and audited.
2. The function contract is documented (what paths are blocked? what edge cases?).
3. Integration tests verify the contract holds.

**Evidence from diff:**
```rust
use astra_sandbox::{CommandRisk, analyze_command_risks, is_rm_catastrophic_rm_path};

if (lower.contains("rm -rf") || lower.contains("rm -fr")) && is_rm_catastrophic_rm_path(&lower) {
    return Err("Error: rm -rf targeting root/home/system path is blocked".into());
}
```

**Tests (line 4319-4344):**
```rust
assert!(validate_execute_bash_command("rm -rf /").is_err());
assert!(validate_execute_bash_command("rm -rf ./build").is_ok());
```

**Fix recommendation:**
1. Add a comment linking to the `astra_sandbox` documentation or source.
2. Add edge case tests: `rm -rf /etc/passwd`, `rm -rf ~/.*`, `rm -rf $HOME/..`
3. Consider adding a local wrapper function that logs when the check is invoked for auditability.

---

#### 3. **task_mgmt.rs:5775 lines — Validation logic is comprehensive but error paths may leave state inconsistent**

**File:** `crates/astra-tools/src/task_mgmt.rs`  
**Problem:** The file has grown to 5,775 lines with extensive validation (duplicate detection, blocker validation, metadata schema enforcement). However, the error handling strategy is unclear. If `task.update` fails mid-way (e.g., metadata validation passes but DB write fails), the state may be partially updated.

**First-principles analysis:**  
State mutations should be atomic or idempotent. The validation logic is procedural and interleaved with state mutations, making it hard to reason about invariants. A better design:
1. Validate all inputs upfront (pure function).
2. Perform all mutations in a single transaction.
3. Return a single error type that distinguishes validation errors from storage errors.

**Evidence from diff (line ~1200):**
```rust
let sid = self.sid();
match self.store.update_task(...).await {
    Ok(updated) => {
        // Post-update validation?
    }
    Err(e) => {
        // What state is the task in now?
        return Err(e);
    }
}
```

**Fix recommendation:**
1. Extract validation into a `validate_task_update` function that returns `Result<(), ValidationError>`.
2. Wrap the DB write in a transaction (if the store supports it).
3. Add a comment documenting the error contract: "Validation errors are returned before any mutation; storage errors indicate the task state is inconsistent and should be reloaded."

---

### 🟡 Important

#### 4. **session_todo_sweeper.rs — Auto-pause logic may race with concurrent updates**

**File:** `crates/runtime/src/server/session/session_todo_sweeper.rs`  
**Problem:** The sweeper auto-pauses stale `in_progress` tasks (line ~200-300 in the diff). However, if a task is being actively updated by the agent while the sweeper runs, the sweeper may pause a task that is actually progressing.

**First-principles analysis:**  
Concurrency control should use optimistic locking (version numbers) or pessimistic locking (row locks). The current logic checks `last_updated > 24h` but does not account for in-flight updates. A better design:
1. Use a `updated_at` column that is atomically updated with the task.
2. Check `updated_at` at pause time (not just at query time).
3. Use `SELECT ... FOR UPDATE` or a CAS loop.

**Evidence from diff (test at end):**
```rust
let metadata: serde_json::Value = serde_json::from_str(&row.1).expect("metadata json");
assert_eq!(metadata["auto_paused_reason"], "stale_in_progress > 24h");
```

**Fix recommendation:**
1. Add a `version` column to the task schema.
2. Use `UPDATE ... WHERE version = ?` to detect concurrent modifications.
3. If the update fails, retry the check (the task may have been legitimately updated).

---

#### 5. **Branch is dirty — staged and unstaged changes suggest incomplete work**

**Problem:** The branch has 4 staged files (+277/-117) and 3 unstaged files (+114/-46). This means the review is not fully representative of the final state.

**First-principles analysis:**  
A review should be performed on a clean, committed state. The dirty state suggests:
1. The author is still iterating.
2. The review may miss critical changes in the unstaged files.
3. The branch may not be ready for merge.

**Fix recommendation:**
1. Commit or stash the unstaged changes.
2. Re-run the review on the clean state.
3. If the unstaged changes are critical, include them in the review explicitly.

---

#### 6. **Branch size violates atomicity — 110 files / 18K lines is too large for a single "fix" branch**

**Problem:** The branch contains 35 commits across 110 files with 18K+ lines changed. This is too large for a single "fix" branch and makes it hard to:
1. Understand the scope of changes.
2. Identify regressions.
3. Roll back specific changes if needed.

**First-principles analysis:**  
A branch should represent a single, cohesive unit of work. The commit messages suggest multiple unrelated changes:
- `Make empty tool calls recoverable`
- `Keep bash detach reusable during a turn`
- `Expose background jobs as jobs`
- `Make task adopt atomic`
- `Align plan and script tool argument UX`

These should be separate branches with separate reviews.

**Fix recommendation:**
1. Split the branch into smaller, focused branches.
2. Re-review each branch independently.
3. Merge them in dependency order (e.g., `bash detach fix` → `task board hardening` → `job UX improvements`).

---

### 💡 Suggestions

#### 7. **shell_ops.rs — Consider using `std::mem::take` for clearer ownership transfer**

**File:** `crates/astra-tools/src/shell_ops.rs:605`  
**Improvement:** The current code uses `slot.lock().await.take()` which is correct but verbose. `std::mem::take` is more idiomatic and makes the ownership transfer explicit.

**Current:**
```rust
let detach_handle = if let Some(slot) = ctx.detach_shell_handle.as_ref() {
    slot.lock().await.take()
} else {
    None
};
```

**Suggested:**
```rust
let detach_handle = ctx.detach_shell_handle
    .as_ref()
    .map(|slot| std::mem::take(&mut *slot.lock().await))
    .flatten();
```

**Benefit:** Clearer intent, less nesting.

---

#### 8. **task_mgmt.rs — Add structured logging for validation failures**

**File:** `crates/astra-tools/src/task_mgmt.rs`  
**Improvement:** The validation logic returns string errors. Add structured logging with context (task ID, field name, invalid value) to aid debugging.

**Current:**
```rust
return Err("Error: task.update requires at least one update field".to_string());
```

**Suggested:**
```rust
tracing::warn!(task_id = %task_id, "task.update called with no fields");
return Err("Error: task.update requires at least one update field".to_string());
```

**Benefit:** Easier to diagnose issues in production.

---

### ✅ Looks Good

1. **rm -rf protection is path-aware and well-tested.** The tests at line 4319-4344 verify that catastrophic paths are blocked while project-relative paths are allowed. This is a first-principles improvement over blanket blocking.

2. **Detach handle lifecycle fix is correct on the happy path.** The handle is properly restored after normal completion, and the test at line 4977 verifies reusability.

3. **Task validation is comprehensive.** The duplicate detection, blocker validation, and metadata schema enforcement are thorough and well-structured.

4. **Sweeper auto-pause is defensive.** The logic to pause stale tasks prevents the "exactly one in_progress at a time" invariant from being violated.

---

### Summary

**Design flaws:**
- Detach handle lifecycle is not exception-safe (use scope guards).
- Branch size violates atomicity (split into smaller branches).
- Validation and mutation are interleaved (separate them).

**Safety regressions:**
- Detach handle not restored on error path (resource leak).
- rm -rf protection delegates to external crate without audit trail.

**Missing validation:**
- Concurrent update races in sweeper (use optimistic locking).
- Edge cases in rm -rf path detection (add more tests).

**Recommendation:** Address the critical issues (1-3), then split the branch and re-review. The foundational work is sound, but the execution needs refinement.
