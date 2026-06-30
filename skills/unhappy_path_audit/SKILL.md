---
name: unhappy-path-audit
description: "Unhappy-path audit: reachability-driven causal tracing. Validate that every changed code path is (1) reachable, (2) logically correct, (3) consistent in error propagation and state. Covers resource safety (Q1-Q3: leak/hung/OOM) as a subset. Produces structured risk reports keyed to actual astra crates."
user_invocable: true
when_to_use: "When the user explicitly asks to audit failure behavior: 'unhappy path audit', 'unhappy path', 'failure path', 'error handling audit', 'reachability audit', 'dead code audit', 'resource leak audit', 'hung/OOM audit', or 'resource safety audit'. Do not trigger for generic 'review the diff' or ordinary code review."
arguments:
  - name: TARGET
    description: "Target module(s), crate(s), or branch to audit (e.g. 'astra-turn-core', 'runtime', 'HEAD~3..HEAD'). Default: uncommitted changes or current branch diff."
    required: false
  - name: FOCUS
    description: "Audit dimension: 'reachability', 'classification', 'error-propagation', 'state', 'resource' (Q1-Q3), or 'all' (default: all)"
    required: false
allowed_tools:
  - read_file
  - grep
  - glob
  - bash
  - git
---

# Unhappy Path Audit Skill — astra

## First Principles

**Unhappy path auditing = verifying 3 things about every code path, IN ORDER:**

| #   | Gate                      | Question                                                                                           | Failure Mode                |
| --- | ------------------------- | -------------------------------------------------------------------------------------------------- | --------------------------- |
| R0  | **What changed?**         | What is the actual diff? Which code paths were added, removed, or altered?                         | Analyzing stale code        |
| R1  | **Is it reachable?**      | Given all gating conditions (flags, mode switches, feature gates, config), does this path execute? | **Dead code**, false safety |
| R2  | **Is the logic correct?** | When it executes, does it produce correct results? Classification, error handling, state machine.  | **Misclassification**, bug  |
| R3  | **What actually breaks?** | Given it's wrong AND reachable, what is the concrete consequence? Not "might break" but "breaks".  | **Severity misassignment**  |

**This ordering is non-negotiable.** R0 before R1 before R2 before R3. A bug that is unreachable (R1 fail) is NOT a finding — it's a note. A bug whose consequences are misjudged (R3 skip) produces wrong severity.

**Resource safety (leak/hung/OOM) is Q1-Q3, a specialization within R2:**

| Q   | Proposition                                              | Maps to |
| --- | -------------------------------------------------------- | ------- |
| Q1  | Every resource creation has a guaranteed destruction     | R2      |
| Q2  | Every blocking wait has a guaranteed release             | R2      |
| Q3  | Every accumulation has an upper bound or recycling point | R2      |

But Q1-Q3 is ONLY applied to resources that actually exist in the R0 diff. Do NOT audit the entire crate's resource lifecycle — only the resources touched by the change set.

---

## Critical Rule: Reachability First, Always

> **The most common and most expensive audit error is filing a finding without confirming reachability.**

Before ANY finding enters the report, run this check:

```
grep for the gating condition (flag, mode, feature) → trace ALL call sites → confirm at least one call site reaches the code in the current configuration
```

**Anti-pattern (actual failure, 53-tool audit):**

```
1. Saw `allow_factual_retry: false` changed from `true` → filed as 🟡 "might weaken retry"
2. Did NOT grep for `allow_factual_retry` consumers → missed that the +119-line judge system is entirely gated by this flag
3. Actual finding: ALL new judge code is DEAD CODE → 🔴, not 🟡
4. Root cause: skipped R1
```

**Correct workflow:**

```
1. See `allow_factual_retry: false` (R0)
2. grep `allow_factual_retry` → find `should_attempt_factual_retry()` (R1 start)
3. grep `should_attempt_factual_retry` → find all call sites → confirm reachability (R1 complete)
4. grep `FactualRetryFallbackJudgeVerdict` → find the gated code → assess impact (R2)
5. Conclusion: dead code if flag is never true (R3)
```

---

## Audit Workflow

### Phase 0 — Establish the Change Set

```
1. git diff --stat (or git log for multi-commit)
2. Identify the minimal set of functions/paths that changed
3. Do NOT expand beyond the change set unless a changed path calls into unchanged code that is now affected
```

**Output**: a list of `<file:function>` that the audit will cover.

### Phase 1 — Per-Path Reachability (R1)

For each changed code path from Phase 0:

```
1. Identify all gating conditions:
   - Boolean flags (allow_factual_retry, verification_required)
   - Mode switches (Bypass/Auto/ApprovalRequired, child_inherited_mode)
   - Feature gates (#[cfg(feature = "...")])
   - Config values (from profiles, runtime config)
   - Error-path guards (if let Err(_) = ..., ?)

2. grep for each gating condition's consumers and callers

3. Trace the FULL call chain from entry point to changed code:
   - Entry: where does execution start that COULD reach this code?
   - Every if/match branch on the path: can it reach the changed code?
   - Terminal: does the path actually terminate at the changed code?

4. Verdict per path:
   - REACHABLE: at least one concrete scenario reaches it
   - DEAD: no configuration reaches it → NOTE, not finding
   - CONDITIONAL: reaches only under specific config → document the condition
```

**Stop rule**: if R1 returns DEAD, do NOT proceed to R2. Record as a note and move on.

### Phase 2 — Logic Correctness (R2)

For each REACHABLE or CONDITIONAL path:

**Sub-dimensions to check (apply only those relevant):**

| Dimension              | Check                                                                                   | Common patterns in astra                                                                     |
| ---------------------- | --------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| **Classification**     | Does the classification logic correctly categorize inputs?                              | `contains_any_keyword` substring matching, `split_whitespace().join(" ")` normalization gaps |
| **Error propagation**  | When this code produces/forwards an error, is context preserved?                        | `ErrorKind` stripped by `map_err(\|e\| format!(...))`, retry info lost                       |
| **State consistency**  | After partial success or error, is state consistent?                                    | Partial writes before error return, state machine stuck in intermediate state                |
| **Resource (Q1-Q3)**   | For changed resource operations: creation→destruction, wait→release, accumulation→bound | `tokio::spawn` without abort, `Vec::push` without clear, `Mutex` without timeout             |
| **Gating correctness** | Is the gating condition itself correct?                                                 | Mode comparison using `==` when `>=` is needed, inverted boolean                             |

**Resource Q1-Q3 drill-down (only for paths touching resources):**

```
Entry function
  └→ Layer 1: Execute Q1-Q3 on every resource/wait/accumulation point in the DIFF
      ├─ Termination satisfied at this layer → ✅
      └─ Termination depends on lower layer → ⬇ drill down
          └→ Recurse with Q1-Q3
```

**Anti-false-positive rule for Q2**: exhaust all release paths (Drop impl, AbortHandle, CancellationToken, timeout branch, channel close) before ruling hung.

### Phase 3 — Deduplication and Causal Grouping

```
1. Group findings by causal root, not by location:
   - Finding A (dead code) and Finding B (JSON parse bug in dead code) → ONE entry: "A blocks B"
   - Finding C (flag change) is the PRIMARY finding; B is a sub-note

2. Remove subordinate findings that are unreachable unless the primary is fixed

3. For each group, identify the PRIMARY finding (the one that, if fixed, resolves others)
```

**Anti-pattern (actual failure):**

```
❌ Listed as 3 independent findings:
   1. allow_factual_retry disabled (🟡)
   2. extract_first_json_object matches markdown fences (🟡)
   3. looks_like_factual_query deleted (🟡)

✅ Correct:
   1. allow_factual_retry disabled → ALL factual retry judge code is dead (🔴)
      - If re-enabled: extract_first_json_object has markdown fence bug (sub-note)
      - looks_like_factual_query removal is part of the same transition (sub-note)
```

### Phase 4 — Severity Calibration (R3)

**Assign severity ONLY after ALL findings are confirmed and deduplicated.**

| Severity | Definition                                                         |
| -------- | ------------------------------------------------------------------ |
| 🔴       | Reachable AND causes definite incorrect behavior in current config |
| 🟡       | Reachable under specific config AND causes incorrect behavior      |
| 🟢       | Reachable but low-impact, OR unreachable but important design note |

**Calibration: for every finding, state the concrete scenario that triggers it.** If you cannot write "when user does X, system does Y instead of Z", the finding is not concrete enough.

---

## Output Format

### Phase 0 Output: Change Set

```
## Change Set
| # | File | Function/Path | Change Type |
|---|------|---------------|-------------|
| 1 | chat_turn_heuristics.rs | TaskExecutionProfile::default() | Modified: flag changed |
| 2 | turn_ingest.rs | parse_factual_retry_fallback_judge_response | Added: new function |
```

### Final Report

```
## Unhappy Path Audit: [target]

### 🔴 Critical (N)
| # | Finding | Reachability | Consequence |
|---|---------|-------------|-------------|
| 1 | [description] | Always | [concrete scenario] |

### 🟡 Medium (N)
...

### 🟢 Low / Notes (N)
...

### Causal Groups
- Finding #1 blocks #2, #3 → primary is #1

### Verified-OK
- [design decision]: confirmed correct because [reason]
```

---

## Bug Claim Verification Gates (G1-G5)

**Applied during Phase 2→3 transition. EVERY finding must pass ALL gates before entering the final report.**

| #   | Gate               | Action                                                                                                     | Anti-Pattern (actual failures)                                                                            |
| --- | ------------------ | ---------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| G1  | **FULL-GRAPH**     | Trace resource ownership to ALL terminal nodes. Do not stop at the next hop.                               | Traced `Arc::clone → worker` but stopped; missed `worker.Drop → AbortHandle` → task cancel                |
| G2  | **CAN-FAIL**       | Open the alleged failure function body; confirm it CAN actually panic/error/block.                         | Assumed `serde_json::to_string()` could hang; it's CPU-bound. Only `from_str` can fail on malformed input |
| G3  | **SYMMETRY**       | For growth claims (push/insert): grep the variable name + `clear()`/`truncate()`. Confirm NO reset exists. | Saw `events.push(...)`, missed `events.clear()` in `flush_batch()`                                        |
| G4  | **LINE-REREAD**    | Re-read every cited line AFTER forming the hypothesis. Do not trust memory.                                | Asserted state doesn't transition on error; re-read shows `state = State::Done` is unconditional          |
| G5  | **CALIBRATE-LAST** | Severity assigned ONLY after G1-G4 pass AND all findings are deduplicated.                                 | Filed as Medium before confirming reachability; finding was unreachable → severity meaningless            |

**Any finding failing G1-G4 is discarded, not downgraded.** G5 is a global gate applied after the full bug list is finalized.

---

## Common Failure Patterns (astra-specific)

### Pattern 1: Flag Gating → Dead Code

**Symptom**: a boolean flag change makes downstream code unreachable.
**Detection**: grep the flag name → find all consumers → trace call chains.
**Example**: `allow_factual_retry: false` → `should_attempt_factual_retry()` returns false → `FactualRetryFallbackJudgeVerdict` never instantiated.

### Pattern 2: Substring Matching Without Word Boundaries

**Symptom**: `"fix"` matches `"prefix"`. Classification logic over-triggers.
**Detection**: for every keyword in a matching list, grep the codebase for words that contain it as a substring.
**Example**: `EXPLICIT_MUTATION_DIRECTIVE_TERMS` — `"fix"` matches "prefix", "suffix", "fixation". `"create"` matches "recreate".

### Pattern 3: ErrorKind Stripping in map_err

**Symptom**: `map_err(|e| format!("... {}", e))` converts typed ErrorKind to String, losing retry policy.
**Detection**: grep `map_err(|` near `?` or `.await` in changed code.
**Example**: `core::ErrorKind::Retryable` → `format!("failed: {}", e)` → caller cannot check `is_retryable()`.

### Pattern 4: Partial State After Error Return

**Symptom**: `vec.push(x); f()?;` — x is pushed, f fails, function returns, vec has partial state.
**Detection**: look for any mutable operation BEFORE `?` in the same scope.
**Example**: message queued to channel, then `db.write()?` fails → message already sent but not persisted.

### Pattern 5: JoinHandle Drop Without AbortHandle

**Symptom**: `tokio::spawn(task)` with no corresponding `AbortHandle::abort()`.
**Detection**: grep `tokio::spawn(` → check if JoinHandle is stored and aborted on Drop/error path.
**Example**: spawned task outlives the scope that created it, continues running with stale references.

### Pattern 6: Mode/Enum Comparison Gaps

**Symptom**: new enum variant added but comparison logic uses `==` instead of exhaustive match, or uses `<`/`>` that doesn't account for new ordering.
**Detection**: for any enum change, grep all `==` comparisons against that enum.
**Example**: `PermissionMode::Bypass` added — `if mode == Auto` no longer covers all non-ApprovalRequired modes (Bypass is now a gap).


---

## Appendix A: astra Module Threat Model Reference

When auditing a specific crate, use this reference to focus on the most likely failure modes.

### A.0 — `core` (error foundation)

**ErrorKind** (`error_kind.rs`, 21 variants):
- Classification correctness: every `?` site that creates/converts an error must assign the correct ErrorKind
- `is_retryable()`: retry policy must match actual retry-worthiness
- `retry_delay_ms()`: backoff strategy must be correct
- `classify_tool_output()`: downgrade classifier must not misclassify transient vs permanent

**Audit focus**: grep `ErrorKind::` in changed code → verify variant choice → trace `is_retryable()` consumer impact

### A.1 — `astra-turn-core`

**Turn processing** (`agentic/`):
- Turn lifecycle: extraction → execution → verification → completion
- Factual retry judge: gated by `allow_factual_retry` → verify reachability
- Verification stop hook: gated by `verification_required` → verify no infinite wait

**Permission engine** (`permission/`):
- Mode transitions: Bypass → Auto downgrade in child agent fork
- Allowlist matching: AllowRules step after Guard
- Safety middleware: catastrophic command detection, `chmod 777 /` patterns

**Chat turn heuristics** (`chat_turn_heuristics.rs`):
- `EXPLICIT_MUTATION_DIRECTIVE_TERMS`: keyword matching correctness
- Profile assignment: mutating vs analysis vs default → affects verification, retry

**Audit focus**: flag gating reachability, keyword substring matching, mode transition correctness

### A.2 — `runtime`

**HTTP server**: connection pool exhaustion, request timeout, graceful shutdown
**WebSocket bridge**: reconnection, message loss on disconnect, backpressure
**DB pool**: connection leak, transaction rollback after partial writes
**Orchestration**: turn scheduling, agent lifecycle, pause/resume state

**Audit focus**: Q1 (connection/pool lifecycle), Q2 (shutdown wait ordering), state consistency after agent failure

### A.3 — `services`

**Event ingestion**: batching, retry on write failure, deduplication
**Sync engine**: conflict resolution, checkpoint recovery, partial sync state
**Durable tasks**: task lifecycle, retry-after-failure, orphaned tasks
**Cost ledger**: accumulation, flush interval, loss on crash

**Audit focus**: Q3 (batch accumulation bound), state consistency after partial sync, error propagation fidelity

### A.4 — `astra-messaging`

**Transport**: connection lifecycle, reconnect backoff, message serialization failure
**Delegation**: fan-out completion tracking, partial results aggregation
**Ack tracking**: ack timeout, duplicate ack, missing ack → retry storm
**Dead letter**: overflow, no consumer, disk fill

**Audit focus**: Q2 (ack wait timeout), Q3 (dead letter queue bound), Q1 (connection lifecycle)

### A.5 — `astra-pipeline`

**Checkpoint**: write failure during checkpoint, partial checkpoint recovery
**Crash recovery**: state reconstruction from incomplete journal
**Trace retention**: accumulation bound, eviction policy

**Audit focus**: Q3 (trace unbounded growth), state consistency after crash recovery

### A.6 — `astra-sandbox`

**Process isolation**: child process lifecycle, orphan processes, signal handling
**Bash execution**: timeout enforcement, output capture bound, stdin pipe closure
**Resource limits**: CPU/memory cgroup limits, disk quota

**Audit focus**: Q1 (child process reaping), Q2 (timeout guarantees), Q3 (output buffer bound)

### A.7 — `astra-cli`

**Edge tools**: local process lifecycle, file watcher, cache management
**TUI**: render loop, input handling, terminal resize recovery
**MCP client**: connection management, tool list caching, sandbox retry

**Audit focus**: Q1 (local process lifecycle), Q2 (MCP connection timeout), state consistency in TUI

### A.8 — `web/`

**Next.js frontend**: API call retry, loading state, error boundary
**Rendering**: partial render on data fetch failure, hydration mismatch

**Audit focus**: error propagation to user, retry on transient failures

---

## Appendix B: Rust/Tokio Wait Termination Model

When auditing Q2 (wait → termination) in async Rust, check these patterns:

| Wait Mechanism               | Release Mechanism                          | Must Verify                                                     |
| ---------------------------- | ------------------------------------------ | --------------------------------------------------------------- |
| `tokio::spawn(task)`         | `AbortHandle::abort()` or `CancellationToken` | AbortHandle stored and called in Drop or error path             |
| `rx.recv()` (tokio channel)  | All `tx` clones dropped or channel closed  | Sender count reaches 0 on ALL code paths (including error)      |
| `Mutex::lock().await`        | Lock released at end of scope (RAII)       | No `std::mem::forget` or leak of MutexGuard                     |
| `select! {}`                 | Every branch terminates                    | At least one branch fires; if `else` branch, default fires      |
| `CancellationToken::cancelled()` | `CancellationToken::cancel()`          | cancel() called in Drop or error path of owner                  |
| `Notify::notified()`         | `Notify::notify_one()` or `notify_waiters()`| Notify call guaranteed to fire after notified() is awaited      |
| `Semaphore::acquire()`       | `Semaphore::add_permits()` or Drop         | Permits returned on ALL paths; check for leak                   |
| `Barrier::wait()`            | Enough waiters arrive                      | Can the required count ever be reached on error paths?          |
| I/O (`read`, `write`)        | Timeout or peer close                      | `tokio::time::timeout` wrapping the I/O future                  |
| HTTP request                 | Response, timeout, or connection close     | Timeout set on client; connection pool has eviction             |
| DB query                     | Result, timeout, or pool shutdown          | Statement timeout; pool shutdown drains in-flight queries       |

**Drill-down rule**: for each wait, identify the release mechanism in the SAME scope first. If not found, drill down to the owner scope. If owner scope is unbounded (static, global), that's a finding.

---

## Appendix C: False Positive Patterns

Common patterns that LOOK like bugs but are NOT. Check these before filing.

| Pattern                                  | Why It's NOT a Bug                                                      |
| ---------------------------------------- | ----------------------------------------------------------------------- |
| `tokio::spawn` without visible abort     | JoinHandle stored in struct that has Drop impl calling abort            |
| `Vec::push` without clear in same function | Variable passed to `flush_batch()` which calls `.clear()`             |
| `Arc::clone` without visible drop        | Arc is stored in a bounded collection that is drained                   |
| `?` before side-effect cleanup           | Earlier operation is idempotent, or cleanup happens in Drop             |
| `unwrap()` on async result               | Invariant guaranteed by caller; may need `.expect()` with reason        |
| `panic!` in Drop impl                    | Only on already-panicking path (guarded by `std::thread::panicking()`)  |
| Channel send without recv                | Channel is `broadcast` with `max_receive_count = 0` allowed             |
| `JoinSet` with spawned tasks not awaited | `JoinSet::shutdown().await` in Drop or main                             |
| `Weak::upgrade()` fails silently         | Strong ref lifecycle is bounded by owning struct; Weak is for cycle breaking |
