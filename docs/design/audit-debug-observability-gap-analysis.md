# Audit, Debug & Observability — Gap Analysis and Improvement Plan

> Date: 2026-04-04
> Status: Proposal
> Scope: Turn-level failure analysis, data persistence robustness, self-correction closed loop

---

## 1. Executive Summary

The astra-engine has substantial audit and debug infrastructure — journal, checkpoint, reflect, tool health, stall detection, session fork, and replay. However, a deep code-level analysis reveals a **structural "survivorship bias"**: the persistence pipeline is designed for the success path. When a turn fails, nearly all intermediate state is lost, making post-mortem analysis extremely difficult.

This document catalogs every existing capability, maps every failure scenario to its data loss profile, and proposes a prioritized improvement plan.

---

## 2. Existing Capabilities Inventory

### 2.1 Session Audit (`session_audit.rs`)

| Component | What It Provides |
|-----------|-----------------|
| `SessionAuditSummary` | Session-level aggregates: turn count, token usage, tool call success/failure, error/stall/checkpoint counts, models used, duration |
| `TurnDetail` | Single turn detail with child events |
| `ToolAnalytics` | Per-tool: call count, success rate, avg/max duration, last error |
| `CrossSessionStats` | Cross-session aggregates: total turns, tokens, tool failures, stalls |
| `CrossSessionToolAnalytics` | Tool analytics aggregated across all user sessions |
| `ErrorListResponse` | Error/anomaly events within a session |

**Data source**: MatrixOne `agent_events` table (cloud DB).

### 2.2 Reflect Service (`reflect.rs`)

| Component | What It Provides |
|-----------|-----------------|
| `ReflectReport` | Overview + diagnoses + insights + recommendations |
| `Diagnosis` | Root-cause classification by `ErrorClass` (ResourceLimit, Auth, Network, FileNotFound, ToolMisuse, Timeout, DatabaseError, Stall, Unknown), with severity, samples, fix_hint |
| `classify_error_content` | Rule-based error content → ErrorClass mapping |
| `generate_insights` | Detects: high error rate, tool over-reliance, stall patterns, session duration anomalies |
| `generate_recommendations` | Actionable suggestions based on diagnoses and insights |

**Limitation**: Pure rule engine (SQL aggregation + pattern matching). No LLM-powered deep analysis.

### 2.3 Stall Detection (`stall.rs`)

| Component | What It Provides |
|-----------|-----------------|
| `StallReflection` | what_happened / why / what_to_try + avoid_tools |
| `DivergenceStatus` | Healthy → Exploring → Diverging three-level state |
| `IntentDrift` | Detects agent drifting from user intent via keyword matching |
| `detect_nudge_ignored` | Detects if agent violated previous avoid_tools advice |
| Tool signature repetition | Detects identical tool calls with same arguments across rounds |

### 2.4 Tool Health (`tool_health.rs`)

| Component | What It Provides |
|-----------|-----------------|
| `ToolHealthTracker` | Per-tool success/failure/timeout/cache-hit tracking |
| Auto-deprioritize | 3 consecutive failures → deprioritize |
| Auto-rehabilitate | Success after deprioritize → rehabilitate |
| Resource limit | Immediate deprioritize on resource limit errors |
| `ToolHealthSummary` | Aggregate: deprioritized count, total timeouts, cache hits, flaky tools |

### 2.5 TurnGuard (`turn_guard.rs`)

Orchestrates stall detection, divergence detection, tool health, error recovery, and escalation into a per-turn `TurnVerdict`:

- Injects correction prompts into LLM messages
- Tracks `nudge_count` for escalation
- Progressive degradation: Warning → Critical (restrict to read-only) → 2nd Critical (force stop)
- Records `VerdictEvent` for audit trail

### 2.6 Session Restore & Checkpoint

| Component | What It Provides |
|-----------|-----------------|
| `SessionRestoreService` | Restore from local workspace or cloud |
| `RestoredSession` | turn count, tokens, recent_tools, learning_snapshot, plan state, contract state, conversation messages, blocked tools |
| `RestoredCheckpoint` | Restore to specific checkpoint number |
| `StepCheckpoint` (Light) | Cursor, step_id, progress, total_tokens |
| `StepCheckpoint` (Heavy) | Light + full conversation messages, budget state, blocked/recent tools, memory context, delegation state |
| `StepRecorder` | Per-turn event recording with file-backed persistence |

**Checkpoint write timing**: Heavy checkpoint written after each tool round in `agentic_post_tool_policy`. Light checkpoint written after each individual tool call.

### 2.7 Session Fork (`session_fork.rs`)

| Component | What It Provides |
|-----------|-----------------|
| `fork_local_session` | Copy all journal events from parent to new session |
| `SessionLineage` | parent_session_id, forked_after_turn, label |
| Workspace metadata | Carries forward correlation_id for audit chain |

**Limitation**: Copies ALL events regardless of `forked_after_turn` value. Cannot fork from a specific turn.

### 2.8 Debug Command (`slash_debug.rs`)

Interactive turn-by-turn inspection:
- Overview: turn count, checkpoint count, total tokens
- Per-turn: input, output, tools (args + results), injected messages, raw JSON
- `TurnMessagesView`: Delta comparison between adjacent heavy checkpoints

### 2.9 Replay Service (`replay.rs`)

- `replay_session`: Replay session events in sandbox (mock mode supported)
- `compare_replay`: Compare original vs replay event counts

### 2.10 Journal System (`session_journal.rs`)

Append-only JSONL with event types: session_start/end, turn, turn_error, compact, stall_detected, checkpoint, session_fork, delegation_started/completed, plan_progress, turn_guard_verdict, cloud_pull_sync_marker.

`ToolCallRecord` per tool call: name, ok, ms, error, input_bytes, output_bytes, args_preview, result_preview.

### 2.11 Learning & Evolution

| Component | What It Provides |
|-----------|-----------------|
| `PatternLibrary` | Tool chain patterns with success rate, quality score, time decay |
| `EntityGraph` | Entity → tool mapping with confidence decay |
| `ToolQualityTracker` | Tool selection quality tracking |
| `LearningSnapshot` | Persistable, cross-session syncable |

### 2.12 Event Ingestion (`event_ingestion.rs`)

Journal events → MatrixOne `agent_events` table via async bounded channel.
- At-least-once delivery (deterministic event_id, INSERT IGNORE)
- Backpressure: bounded channel (capacity 200); local journal is source of truth
- Graceful shutdown: flush on drop

### 2.13 Introspection Service (`introspection/database.rs`)

- Memory recall quality scoring
- Context health analysis (zone balance, pollution ratio, compaction effectiveness)
- Context trend computation
- Skills introspection


---

## 3. Data Persistence Architecture

### 3.1 Three-Layer Persistence Model

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Turn Execution                               │
│  stream_chat_sse() → run_agentic_loop_with_host() → [tool rounds]  │
└──────────┬──────────────────────┬───────────────────────┬───────────┘
           │                      │                       │
     ┌─────▼──────┐      ┌───────▼────────┐     ┌───────▼────────┐
     │  Layer 1    │      │   Layer 2      │     │   Layer 3      │
     │  Journal    │      │   Checkpoint   │     │   Cloud DB     │
     │  (JSONL)    │      │   (JSON files) │     │   (MatrixOne)  │
     ├─────────────┤      ├────────────────┤     ├────────────────┤
     │ Trigger:    │      │ Trigger:       │     │ Trigger:       │
     │ turn end    │      │ each tool      │     │ async channel  │
     │ (success or │      │ round in       │     │ fire-and-      │
     │  failure)   │      │ agentic loop   │     │ forget         │
     ├─────────────┤      ├────────────────┤     ├────────────────┤
     │ On failure: │      │ On failure:    │     │ On failure:    │
     │ warn + skip │      │ let _ = (skip) │     │ counter + log  │
     │ no retry    │      │ no retry       │     │ no retry       │
     ├─────────────┤      ├────────────────┤     ├────────────────┤
     │ Robustness: │      │ Robustness:    │     │ Robustness:    │
     │ truncated   │      │ non-atomic     │     │ channel full → │
     │ lines       │      │ write (no      │     │ silent drop    │
     │ skipped ✅  │      │ rename) ⚠️     │     │                │
     └─────────────┘      └────────────────┘     └────────────────┘
```

### 3.2 Success Path Data Flow

```
apply_turn_success()
  ├── commit_turn_journal_workspace_and_sidecars()
  │   ├── journal.append(turn_event)              ← with tool_calls, selection, budget
  │   ├── enqueue_ingestion(turn_event)           ← async to cloud
  │   ├── workspace.record_turn(tokens)           ← YAML update
  │   ├── [if checkpoint due] write_checkpoint()  ← local + cloud push
  │   ├── [stall_events] journal.append(stall)    ← per stall event
  │   └── [verdict_events] journal.append(verdict) ← per verdict event
  └── record_selector_turn_outcome()              ← learning update
```

### 3.3 Failure Path Data Flow

```
report_turn_error()
  └── journal.append(turn_error)
      ├── user_input: truncated to 500 chars
      ├── error: truncated to 500 chars
      ├── duration_ms
      └── (nothing else — no tools, no selection, no stall state)
```

### 3.4 The Root Cause: `stream_chat_sse` Drops State on Error

```rust
// sse_loop/mod.rs — current implementation
pub async fn stream_chat_sse(p: ChatTurnParams<'_>) -> Result<StreamResult, String> {
    let mut state = AgenticLoopState { /* ... */ };
    run_agentic_loop_with_host(&mut host, &mut state).await?;
    //                                                     ^^
    //  The ? operator propagates the error and drops `state`.
    //  ALL intermediate data is lost:
    //    - state.tool_call_records
    //    - state.stall_events
    //    - state.verdict_events
    //    - state.all_tools_used
    //    - state.total_prompt / total_completion
    //    - state.last_heavy_checkpoint
    //    - state.turn_guard.health (tool health changes)
    //    - state.turn_guard.errors (error statistics)
    Ok(build_stream_result(/* ... */))
}
```

This is the single most impactful architectural issue. Every downstream improvement depends on fixing this.


---

## 4. Failure Scenario Analysis

### 4.1 Data Loss Matrix

```
Scenario                    Journal     Heavy CP    Cloud DB    Learning    Workspace
────────────────────────────────────────────────────────────────────────────────────
Successful turn             ✅ full     ✅ per-round ✅ async   ✅ async    ✅ per-turn
SSE stream interrupted      ⚠️ error    ⚠️ N-1 rnd  ❌ maybe   ❌ none     ❌ none
Fatal error (ingest)        ⚠️ error    ⚠️ N-1 rnd  ❌ maybe   ❌ none     ❌ none
TurnGuard force_stop        ✅ verdict  ✅ written   ✅ async   ✅ async    ❌ none
Ctrl+C interrupt            ❌ nothing  ⚠️ N-1 rnd  ❌ none    ❌ none     ❌ none
Process crash (OOM/kill)    ⚠️ trunc    ⚠️ N-1 rnd  ❌ buffer  ❌ dirty    ⚠️ last write
Plan subtask failure        ⚠️ error    ⚠️ N-1 rnd  ❌ maybe   ❌ none     ❌ none
post_tool_result 401        ❌ nothing  ⚠️ N-1 rnd  ❌ incons  ❌ none     ❌ none
```

Legend: ✅ = complete, ⚠️ = partial, ❌ = lost/none

### 4.2 Scenario Details

#### 4.2.1 SSE Stream Interrupted (Network Error, LLM Timeout)

**What happens in code** (`bridge_inprocess.rs:645`):
```rust
Err(e) => {
    agent_warn!("llm", "in-process stream transport error: {e}");
    yield render_sse(&json!({"type":"error","message": ...}));
    tool_calls_map.clear();  // ← partially received tool calls wiped
    full_text.clear();        // ← partially received text wiped
    reasoning.clear();
    break;
}
```

Even if the LLM had already returned complete tool call instructions, a transport error on the final SSE chunk wipes everything.

**Data lost**: Partial LLM response, partial tool calls, usage info (may have received prompt_tokens but not completion_tokens).

**Data preserved**: Heavy checkpoints from previous rounds (if not the first round).

#### 4.2.2 Fatal Error from Ingest

**What happens** (`agentic_loop_host.rs:638`):
```rust
AgenticIngestIterationControl::Fatal(e) => return Err(e),
// No try_write_heavy_checkpoint(state) here!
```

The current round's messages state is not checkpointed. Compare with the BreakLoop and ContinueIterating paths which both call `try_write_heavy_checkpoint(state)`.

#### 4.2.3 Ctrl+C Interrupt

**What happens** (`repl_turn.rs:147`):
```rust
TurnAttempt::Interrupted => {
    state.last_turn_interrupted = true;
    return Ok(());
    // No journal event, no checkpoint, nothing.
}
```

Zero data is recorded. The user has no way to know what happened before the interrupt.

#### 4.2.4 Plan Subtask Failure

**What happens** (`plan_executor.rs:1155`):
```rust
Err(err) => {
    st.status = TaskStatus::Pending;
    let event = JournalEvent::turn_error(
        ctx.session_id.as_deref(),
        ctx.turn,
        ctx.model.as_deref(),
        &format!("plan_subtask:{}", next_id),
        &err,
        0,  // duration_ms = 0 (not tracked)
    );
    emit_event(&update_tx, &ctx, event);
}
```

Better than bare turn failure (at least records subtask ID), but still loses all tool call records and partial results from within the subtask.

#### 4.2.5 post_tool_result 401 (Auth Failure Mid-Turn)

**What happens** (`stream_render.rs:487`):
```rust
if is_auth_failure {
    cancel_token.cancel();  // ← aborts entire SSE stream
}
```

The tool executed successfully on the edge, but the result never reaches the server. The edge has the data in `EdgeToolExecResult` but it's not persisted anywhere. Server-side and edge-side state become inconsistent.

#### 4.2.6 Process Crash

**Journal**: Last line may be truncated JSON. `read_journal` handles this gracefully (`Err(_) => continue`).

**Heavy checkpoint**: Last file may be truncated JSON. `read_latest_heavy_checkpoint` returns `Err` (does not panic, but caller must handle).

**Ingestion buffer**: Up to 20 events or 5 seconds of events lost (not flushed to MatrixOne).

**Learning state**: Dirty changes in PatternLibrary, EntityGraph, Calibrator lost (not yet saved to `learning.json`).

**Workspace metadata**: Reflects state at last successful `write_workspace` call (typically the previous turn).


---

## 5. Self-Correction (知错) Capability Analysis

### 5.1 Existing Self-Correction Mechanisms

| Mechanism | Trigger | Action | Effectiveness |
|-----------|---------|--------|---------------|
| TurnGuard stall detection | Same tool signatures N rounds | Inject nudge message + avoid_tools | ✅ Works for repetitive stalls |
| Divergence detection | Exploration tools > budget | Inject correction prompt | ✅ Works for tool wandering |
| Intent drift detection | Tool names don't match query keywords | Inject "refocus" prompt | ⚠️ Keyword-based, misses semantic drift |
| Nudge-ignore detection | Agent uses avoided tools | Inject stronger warning | ✅ Detects violation |
| Tool health deprioritize | 3 consecutive failures | Remove from selection | ✅ Prevents repeated failure |
| Error recovery suggestions | Tool error classified | Suggest alternative tools | ⚠️ Suggestions are generic |
| Escalation (progressive) | Accumulated nudges + errors | Warning → Critical (read-only) → Force stop | ✅ Prevents runaway |
| Consecutive error budget | Same ErrorCategory N turns | Inject strategy-change nudge | ✅ Catches category-level loops |

### 5.2 Self-Correction Gaps

#### Gap 1: Correction → Execution → Result Causal Chain is Broken

TurnGuard injects a correction at turn N ("don't use read_file, use grep"). Turn N+1's tool results are recorded by `record_tool_result`, but there is no linkage back to turn N's correction. Cannot answer: "Did correction X lead to outcome Y?"

**Current state**: `VerdictEvent` records injections and avoid_tools, but the next turn's tool calls are not tagged with "this was in response to correction C".

#### Gap 2: Only Detects Violation, Not Compliance Effectiveness

`detect_nudge_ignored` checks if the agent used avoided tools:

```rust
pub fn detect_nudge_ignored(avoid_tools: &[String], current_tools: &HashSet<String>) -> Vec<String> {
    avoid_tools.iter()
        .filter(|t| current_tools.contains(t.as_str()))
        .cloned()
        .collect()
}
```

Missing cases:
- Agent followed advice but the alternative also failed → correction direction was wrong
- Agent followed advice and succeeded → correction was effective (positive signal not captured)
- Agent partially followed advice → mixed compliance not tracked

#### Gap 3: Learning Only From Success (Survivorship Bias)

`build_learning_outcome_from_payload` is called in `run_bridge_hook_side_effects`, which only fires on the bridge side-effect path — i.e., when the LLM successfully returns a response.

**Failed turns produce no learning outcome.** This means:
- `PatternLibrary` doesn't know a tool chain pattern failed
- `EntityGraph` doesn't know an entity → tool mapping was wrong
- `ToolQualityTracker` doesn't know a selection was low-quality
- Success rates in PatternLibrary are inflated (denominator excludes failures)

#### Gap 4: Correction History Not Persisted Across Sessions

TurnGuard's `last_reflection`, `nudge_count`, and `critical_turns` are in-memory only. When a session is restored, the correction history is lost. The system may repeat the same ineffective corrections.

`ToolHealthTracker` state IS persisted (via `tool_health_entries` in workspace), but the higher-level correction context (what was tried, what worked) is not.

#### Gap 5: SemanticDedup Detection Not Auditable

`SemanticDedup` detects duplicate tool calls and appends a hint to the result string, but:
- The hint is embedded in the tool result text (not structured)
- No separate audit record of "N duplicate calls detected in this turn"
- Cannot query "which sessions had the most duplicate calls" for evolution analysis

#### Gap 6: No "Wrong Tool Selection" Post-Hoc Analysis

The tool selector (`TfIdfSelector` / `LlmToolSelector`) produces a `SelectionResult` with selected tools and confidence, but:
- The selection rationale is not persisted (why tool X was chosen over tool Y)
- After a turn fails, there's no mechanism to re-evaluate "would a different selection have worked?"
- `LearnedContext` (entity boosts, pattern boosts) is available at selection time but not recorded in the journal


---

## 6. Robustness Assessment

### 6.1 What Is Already Robust

| Component | Robustness Property | Evidence |
|-----------|-------------------|----------|
| Journal reader | Tolerates truncated lines | `serde_json::from_str` failure → `continue` |
| FileBackedEventStore | Tolerates truncated lines | Same pattern as journal |
| Heavy checkpoint reader | Returns Err on corruption | Does not panic; caller handles |
| Ingestion | Idempotent writes | Deterministic event_id + INSERT IGNORE |
| Ingestion | Backpressure | Bounded channel; journal is source of truth |
| Session restore | Hybrid local/cloud | Falls back to cloud if local missing |

### 6.2 Robustness Gaps

#### Gap R1: Non-Atomic File Writes

Heavy checkpoint and workspace metadata use `std::fs::write` (or `serde_yaml::to_writer`), which is NOT atomic. If the process crashes mid-write, the file may contain partial content.

```rust
// Current: non-atomic
std::fs::write(&path, serde_json::to_string(&checkpoint)?)?;

// Needed: atomic via rename
let tmp = path.with_extension("tmp");
std::fs::write(&tmp, content)?;
std::fs::rename(&tmp, &path)?;  // rename is atomic on same filesystem
```

#### Gap R2: No fsync

After writing checkpoint/workspace files, there is no `file.sync_all()`. On OS crash (not just process crash), the most recent writes may be lost from the OS page cache.

#### Gap R3: Cloud Persistence Has No Retry

`dispatch_bridge_side_effect_request` spawns a tokio task that attempts one write. On failure:
```rust
record_persist_failure("core_event_persist", &error);
return;  // ← data lost, no retry, no WAL
```

No write-ahead log, no retry queue, no dead-letter mechanism.

#### Gap R4: Ingestion Channel Silent Drop

```rust
pub fn enqueue(&self, event: IngestionEvent) {
    let _ = self.tx.try_send(event);  // ← channel full → silently dropped
}
```

When MatrixOne is slow, events are silently dropped. Since `ReflectService` and `SessionAuditService` query the cloud DB, their analysis may have gaps that are invisible to the user.

#### Gap R5: Reflect Cannot Detect Its Own Data Gaps

`ReflectService` queries `agent_events` and produces a report, but has no way to know if events were dropped by ingestion. The report may say "0 errors" when in reality errors occurred but weren't ingested.

#### Gap R6: Server-Side vs CLI Error Handling Inconsistency

Server-side `create_run` (in `run_lifecycle.rs`) records status + usage even on failure:
```rust
Err(err) => {
    events.push(json!({"event_type": "run_error", "data": {"error": &err}}));
    engine.persist_status(&run_id, "failed", None, Some(&err)).await;
    engine.persist_usage(&run_id, tokens_in, tokens_out, tool_calls).await;
}
```

CLI `report_turn_error` records almost nothing:
```rust
fn report_turn_error(state, line, error, turn_start) {
    journal.append(turn_error(session_id, turn, model, line, error, duration_ms));
    // No token usage, no tool calls, no selection data
}
```


---

## 7. Target Vision

The goal is: **any failure can be debugged**. Specifically:

1. **Every turn failure has enough data for root-cause analysis** — tool calls attempted, selection rationale, partial LLM response, stall/divergence state, error classification
2. **Session history is complete and browsable** — timeline view, search/filter, cross-session comparison
3. **Breakpoint resume** — pause at any turn boundary, resume with full context
4. **Fork from any point** — create a new session branch from turn N or event E, with truncated history
5. **Self-correction is a closed loop** — corrections are tracked, their effectiveness is measured, lessons are persisted across sessions
6. **Analysis is robust to data loss** — missing cloud data falls back to local journal/checkpoint; reports indicate data completeness confidence

---

## 8. Improvement Plan

### 8.1 Priority P0: Rescue Partial Data on Turn Failure

**Problem**: `stream_chat_sse` returns `Result<StreamResult, String>`. On `Err`, all intermediate state in `AgenticLoopState` is dropped.

**Solution**: Change the error type to carry partial data.

```rust
pub struct PartialTurnData {
    pub tool_call_records: Vec<ToolCallRecord>,
    pub tools_used: Vec<String>,
    pub stall_events: Vec<(String, u32)>,
    pub verdict_events: Vec<VerdictEvent>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub tool_calls_count: u32,
    pub last_heavy_checkpoint: Option<StepCheckpoint>,
    pub tool_health_export: Vec<ToolHealthEntry>,
    pub session_id: Option<String>,
}

pub struct TurnFailure {
    pub error: String,
    pub partial: PartialTurnData,
}

// stream_chat_sse returns:
pub type TurnResult = Result<StreamResult, TurnFailure>;
```

In `sse_loop/mod.rs`, replace `?` with explicit match:
```rust
let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
match outcome {
    Ok(_) => Ok(build_stream_result(/* ... */)),
    Err(e) => Err(TurnFailure {
        error: e,
        partial: extract_partial_from_state(&mut state),
    }),
}
```

**Impact**: Unlocks all downstream improvements. Without this, enriched TurnError, failure learning, and correction tracking are impossible.

**Files affected**: `sse_loop/mod.rs`, `repl_turn.rs`, `plan_executor.rs`, `command_router.rs`, `slash_state.rs`, `slash_info.rs`

### 8.2 Priority P0: Fatal Error Path Checkpoint

**Problem**: `AgenticIngestIterationControl::Fatal` does not write a heavy checkpoint.

**Solution**: One line addition in `agentic_loop_host.rs`:

```rust
AgenticIngestIterationControl::Fatal(e) => {
    try_write_heavy_checkpoint(state);  // ← add this
    return Err(e);
}
```

**Impact**: Ensures the last round's messages state is preserved even on fatal errors.

### 8.3 Priority P0: Record Ctrl+C Interrupts

**Problem**: `TurnAttempt::Interrupted` records nothing.

**Solution**: Write a journal event before returning:

```rust
TurnAttempt::Interrupted => {
    state.last_turn_interrupted = true;
    if let Some(journal) = state.journal.as_ref() {
        let evt = JournalEvent::turn_error(
            state.session_id.as_deref(),
            state.turn + 1,
            state.model.as_deref(),
            &line,
            "user_interrupted (Ctrl+C)",
            turn_start.elapsed().as_millis() as u64,
        );
        let _ = journal.append(&evt);
    }
    return Ok(());
}
```

### 8.4 Priority P1: Enriched TurnError Events

**Problem**: `turn_error` journal events contain only user_input (500 chars), error (500 chars), and duration_ms.

**Solution**: Use partial data from P0 to write enriched events:

```rust
fn report_turn_error(state: &ReplState, line: &str, failure: &TurnFailure, turn_start: Instant) {
    if let Some(journal) = state.journal.as_ref() {
        let mut evt = JournalEvent::turn_error(
            state.session_id.as_deref(),
            state.turn + 1,
            state.model.as_deref(),
            line,
            &failure.error,
            turn_start.elapsed().as_millis() as u64,
        );
        // Attach partial data
        if !failure.partial.tool_call_records.is_empty() {
            evt.tool_calls = Some(failure.partial.tool_call_records.clone());
        }
        evt.tokens_in = Some(failure.partial.prompt_tokens);
        evt.tokens_out = Some(failure.partial.completion_tokens);
        evt.tool_calls_count = Some(failure.partial.tool_calls_count);
        evt.metadata = Some(json!({
            "tools_used": failure.partial.tools_used,
            "stall_count": failure.partial.stall_events.len(),
            "verdict_count": failure.partial.verdict_events.len(),
            "error_type": "turn_failure",
        }));
        let _ = journal.append(&evt);
        enqueue_ingestion(state, &evt);
    }
}
```

### 8.5 Priority P1: Learn From Failures

**Problem**: Learning outcomes are only generated on the success path.

**Solution**: Generate failure learning outcomes from partial data:

```rust
pub struct FailureLearningOutcome {
    pub query: String,
    pub tools_attempted: Vec<String>,
    pub error_category: String,
    pub stall_detected: bool,
    pub correction_was_active: bool,
}
```

In `report_turn_error`, after writing the journal event:
```rust
if let Some(learning_writer) = &state.learning_writer {
    let outcome = FailureLearningOutcome {
        query: line.to_string(),
        tools_attempted: failure.partial.tools_used.clone(),
        error_category: classify_error(&failure.error),
        stall_detected: !failure.partial.stall_events.is_empty(),
        correction_was_active: !failure.partial.verdict_events.is_empty(),
    };
    learning_writer.record_failure(outcome);
}
```

PatternLibrary needs a `record_failure` method that decrements success_rate for the matching pattern.

### 8.6 Priority P1: Correction Closed Loop

**Problem**: Corrections are injected but their effectiveness is not tracked.

**Solution**: Add correction tracking to TurnGuard:

```rust
pub struct CorrectionRecord {
    pub turn: u32,
    pub correction_type: String,       // "stall_nudge", "divergence", "deprioritize"
    pub avoid_tools: Vec<String>,
    pub suggested_alternatives: Vec<String>,
}

pub struct CorrectionOutcome {
    pub record: CorrectionRecord,
    pub followed: bool,               // did agent avoid the tools?
    pub used_alternative: bool,        // did agent use suggested alternative?
    pub next_turn_quality: ResultQuality, // was the next turn successful?
}
```

In `TurnGuard::evaluate()`, after generating a verdict with injections:
```rust
self.pending_correction = Some(CorrectionRecord { /* ... */ });
```

In the next turn's `record_tool_calls()`:
```rust
if let Some(correction) = self.pending_correction.take() {
    let current_tools: HashSet<_> = /* extract from tool_calls */;
    let followed = !correction.avoid_tools.iter().any(|t| current_tools.contains(t));
    let used_alt = correction.suggested_alternatives.iter().any(|t| current_tools.contains(t));
    self.correction_history.push(CorrectionOutcome {
        record: correction,
        followed,
        used_alternative: used_alt,
        next_turn_quality: ResultQuality::Success, // filled in after tool results
    });
}
```

Persist `correction_history` in the journal's verdict event metadata.

### 8.7 Priority P1: Atomic File Writes

**Problem**: Checkpoint and workspace writes are non-atomic.

**Solution**: Write-then-rename pattern:

```rust
fn atomic_json_write(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let content = serde_json::to_string_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, content.as_bytes())?;
    std::fs::rename(&tmp, path)
}
```

Apply to: `write_step_checkpoint`, `write_workspace`, `save_learning_state`.

### 8.8 Priority P2: Precise Fork (From Specific Turn)

**Problem**: `fork_local_session` copies all events regardless of `forked_after_turn`.

**Solution**: Filter events by turn number:

```rust
// In fork_local_session, replace the event copy loop:
let target_turn = opts.fork_after_turn.unwrap_or(forked_at_turn);
let mut current_turn = 0u32;
for mut evt in events {
    if matches!(evt.event_type, SessionStart | SessionEnd) { continue; }
    if evt.event_type == JournalEventType::Turn {
        current_turn += 1;
        if current_turn > target_turn { break; }
    }
    evt.session_id = Some(new_id.clone());
    out.push(evt);
    copied += 1;
}
```

Also need to:
- Truncate heavy checkpoints to the target turn
- Reset workspace metadata (turn_count, tokens) to match the truncated state
- If plan is active, rewind plan state to the corresponding subtask

### 8.9 Priority P2: Reflect Data Source Fusion

**Problem**: Reflect only queries cloud DB. If ingestion dropped events, analysis has invisible gaps.

**Solution**: Add local-first analysis mode:

```rust
pub trait ReflectService {
    // Existing: cloud DB analysis
    async fn build_evidence(&self, ...) -> ReflectReport;

    // New: local journal + checkpoint analysis
    fn build_evidence_local(session_id: &str) -> ReflectReport;
}
```

`build_evidence_local` reads the journal JSONL and checkpoint files directly, bypassing the cloud DB. The report includes a `DataCompleteness` section:

```rust
pub struct DataCompleteness {
    pub journal_events: u32,
    pub cloud_events: u32,          // 0 if offline
    pub missing_tool_results: u32,  // tool_call without matching tool_result
    pub orphan_events: u32,
    pub confidence: f64,            // 0.0-1.0
    pub warnings: Vec<String>,     // e.g. "3 events may be missing from cloud DB"
}
```

### 8.10 Priority P2: Tool Selection Decision Trace

**Problem**: Tool selection rationale is not persisted.

**Solution**: Add a `SelectionTrace` to the journal turn event:

```rust
pub struct SelectionTrace {
    pub query_signals: Vec<String>,
    pub candidate_scores: Vec<(String, f64)>,  // top 10
    pub boost_terms: Vec<String>,
    pub learned_context_summary: String,
    pub final_tools: Vec<String>,
    pub confidence: f64,
    pub strategy: String,  // "tfidf", "llm", "fallback"
}
```

Record in `JournalEvent::turn` via a new `.with_selection_trace()` builder method.

### 8.11 Priority P3: LLM-Powered Turn Analysis

**Problem**: Reflect is a rule engine. Cannot answer "why did the agent choose the wrong tool?"

**Solution**: Add an LLM analysis endpoint:

```rust
pub trait ReflectService {
    // New: LLM-powered single-turn deep analysis
    async fn analyze_turn(
        &self,
        session_id: &str,
        turn: u32,
        question: &str,  // "why did this turn fail?"
    ) -> TurnAnalysis;
}
```

Implementation: Load the heavy checkpoint for the turn, extract messages + tool calls + verdict, send to LLM with a structured analysis prompt. The LLM evaluates tool selection quality, argument correctness, and suggests what should have been done differently.


---

## 9. Implementation Dependency Graph

```
P0-A: PartialTurnData ──────────────────────────────────────┐
  │                                                          │
  ├──→ P1-A: Enriched TurnError (needs partial data)        │
  │                                                          │
  ├──→ P1-B: Learn From Failures (needs partial data)       │
  │                                                          │
  └──→ P1-C: Correction Closed Loop (needs verdict_events)  │
                                                             │
P0-B: Fatal Checkpoint ─── (independent)                     │
                                                             │
P0-C: Ctrl+C Recording ─── (independent)                    │
                                                             │
P1-D: Atomic Writes ─── (independent)                       │
                                                             │
P2-A: Precise Fork ─── (independent)                        │
                                                             │
P2-B: Reflect Fusion ─── (independent, but benefits from    │
  │                        enriched TurnError)               │
  │                                                          │
  └──→ P3: LLM Turn Analysis (needs checkpoint + journal)   │
                                                             │
P2-C: Selection Trace ─── (independent) ─────────────────────┘
```

P0-A is the critical path. All P1 improvements depend on it.

---

## 10. Effort Estimates

| ID | Title | Effort | Files Changed | Risk |
|----|-------|--------|---------------|------|
| P0-A | PartialTurnData on error | M | ~6 files (sse_loop, repl_turn, plan_executor, command_router, slash_state, slash_info) | Medium — changes return type across call chain |
| P0-B | Fatal checkpoint | XS | 1 line in agentic_loop_host.rs | None |
| P0-C | Ctrl+C recording | S | repl_turn.rs | None |
| P1-A | Enriched TurnError | S | repl_turn.rs, session_journal.rs | Low |
| P1-B | Learn from failures | M | learning.rs, pattern.rs, repl_turn.rs | Low |
| P1-C | Correction closed loop | M | turn_guard.rs, session_journal.rs | Low |
| P1-D | Atomic writes | S | step_checkpoint.rs, session_workspace.rs, persistence.rs | Low |
| P2-A | Precise fork | M | session_fork.rs, step_checkpoint.rs, session_workspace.rs | Medium — state rollback complexity |
| P2-B | Reflect fusion | M | reflect.rs (new method) | Low |
| P2-C | Selection trace | S | tool_selector.rs, session_journal.rs | Low |
| P3 | LLM turn analysis | L | reflect.rs (new endpoint), prompts | Medium — prompt engineering |

---

## 11. Success Metrics

| Metric | Current | Target |
|--------|---------|--------|
| Data fields in TurnError event | 3 (input, error, duration) | 10+ (+ tools, tokens, stalls, verdicts, selection) |
| Failure scenarios with zero data | 2 (Ctrl+C, post_tool_result 401) | 0 |
| Learning outcomes from failed turns | 0% | 100% |
| Correction effectiveness tracking | None | Per-correction follow/comply/outcome |
| Checkpoint atomicity | Non-atomic | Atomic (rename) |
| Reflect data source | Cloud DB only | Cloud DB + local journal fallback |
| Fork granularity | Whole session only | Per-turn |

---

## 12. Appendix: Key Source Files

| File | Role |
|------|------|
| `rust/crates/mo-agent/src/mo_agent/chat_stream/sse_loop/mod.rs` | `stream_chat_sse` entry point |
| `rust/crates/runtime/src/turn/agentic_loop_host.rs` | Agentic loop, checkpoint writes, fatal handling |
| `rust/crates/mo-agent/src/mo_agent/repl_turn.rs` | `apply_turn_success`, `report_turn_error`, `handle_chat_input` |
| `rust/crates/runtime/src/turn/turn_guard.rs` | TurnGuard: stall, divergence, escalation |
| `rust/crates/runtime/src/turn/stall.rs` | Stall detection, divergence, intent drift |
| `rust/crates/runtime/src/turn/tool_health.rs` | ToolHealthTracker |
| `rust/crates/runtime/src/turn/error_recovery.rs` | Error classification, retry, escalation |
| `rust/crates/runtime/src/bridge/side_effects.rs` | Cloud persistence (fire-and-forget) |
| `rust/crates/services/src/session_journal.rs` | Journal events, ToolCallRecord |
| `rust/crates/services/src/session_fork.rs` | Session fork |
| `rust/crates/services/src/session_restore.rs` | Session restore, checkpoint restore |
| `rust/crates/services/src/reflect.rs` | ReflectService, Diagnosis, ErrorClass |
| `rust/crates/services/src/session_audit.rs` | SessionAuditService, ToolAnalytics |
| `rust/crates/services/src/event_ingestion.rs` | Async event ingestion to MatrixOne |
| `rust/crates/runtime/src/pipeline/step_checkpoint.rs` | Heavy/light checkpoint read/write |
| `rust/crates/runtime/src/pipeline/persistence.rs` | LearningSnapshot, ToolHealthEntry |
| `rust/crates/runtime/src/pipeline/learning.rs` | TurnLearningOutcome |
| `rust/crates/runtime/src/pipeline/pattern.rs` | PatternLibrary |
| `rust/crates/runtime/src/pipeline/entity.rs` | EntityGraph |
| `rust/crates/runtime/src/tool_selector.rs` | TfIdfSelector, LlmToolSelector, SelectionResult |
| `rust/crates/mo-agent/src/mo_agent/plan_executor.rs` | Plan subtask execution and failure handling |
| `rust/crates/mo-agent/src/mo_agent/slash_debug.rs` | /debug command |
| `rust/crates/runtime/src/server/run_lifecycle.rs` | Server-side run lifecycle |
| `rust/crates/runtime/src/turn/bridge_inprocess.rs` | In-process LLM bridge, SSE stream handling |
