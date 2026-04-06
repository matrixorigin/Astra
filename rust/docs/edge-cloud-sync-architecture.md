# Edge-Cloud Sync Architecture Audit

**Date:** 2025-01-15 (paths and restore/skills sections verified against Rust implementation, 2026-04)  
**Scope:** `state_sync.rs`, `edge_tools.rs`, `session_journal.rs`, `session_restore.rs`, `step_restore.rs`, unified skill registry  
**Status:** Draft for Team Review

---

## 1. Executive Summary

This document analyzes the current edge-cloud synchronization implementation, identifies full-sync pain points, and maps incremental sync insertion points. The current system supports both full snapshots (~40KB) and delta sync (~2-5KB, 85-90% reduction).

---

## 2. Current Architecture Overview

### 2.1 Data Flow Diagram

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              EDGE (CLI)                                     │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  ┌──────────────────┐      ┌──────────────────────────────────────────────┐  │
│  │ ~/.astra/        │      │ ~/.astra/sessions/                           │  │
│  │ learning/        │      │   <session_id>.jsonl  (journal, append-only) │  │
│  │ {profile}.json   │      │   <session_id>/workspace.yaml                  │  │
│  └────────┬─────────┘      │   <session_id>/step_checkpoints/ (Protocol +    │  │
│           │                │     composite_snapshots.json)                  │  │
│           │                └────────┬─────────────────────────────────────┘  │
│           │                         │                                        │
│           ▼                         ▼                                        │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │                    StateSyncService Trait                             │  │
│  │  ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐   │  │
│  │  │ push_learning()  │  │ push_delta()     │  │ pull_learning()  │   │  │
│  │  └──────────────────┘  └──────────────────┘  └──────────────────┘   │  │
│  │         │                     │                    │                 │  │
│  │         ▼                     ▼                    ▼                 │  │
│  │  ┌──────────────────────────────────────────────────────────────┐   │  │
│  │  │           JournalWriter (JSONL append-only)                  │   │  │
│  │  │  - SessionStart, Turn, TurnError, Compact, Checkpoint, etc.  │   │  │
│  │  └──────────────────────────────────────────────────────────────┘   │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
│                              │                                              │
└──────────────────────────────┼──────────────────────────────────────────────┘
                               │ Network
                               ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                         CLOUD (MatrixOne)                                   │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  ┌────────────────────┐  ┌────────────────────┐  ┌────────────────────┐     │
│  │ learning_snapshots │  │ session_sync_log   │  │ user_preferences   │     │
│  │ - snapshot_id (PK) │  │ - sync_id (PK)     │  │ - pref_id (PK)     │     │
│  │ - user_id          │  │ - user_id          │  │ - user_id          │     │
│  │ - profile_name     │  │ - session_id       │  │ - pref_key         │     │
│  │ - snapshot_json    │  │ - sync_type        │  │ - pref_value       │     │
│  │ - entity_count     │  │ - sync_direction   │  └────────────────────┘     │
│  │ - pattern_count    │  │ - payload_size     │                             │
│  │ - version          │  │ - status           │  ┌────────────────────┐     │
│  │ - updated_at       │  │ - error_message    │  │ agent_sessions     │     │
│  └────────────────────┘  │ - created_at       │  │ agent_events       │     │
│                          └────────────────────┘  └────────────────────┘     │
│                                                                             │
│  ┌────────────────────┐  ┌────────────────────┐  ┌────────────────────┐     │
│  │ session_checkpoints│  │ ctx_snapshots      │  │ data_versioning_   │     │
│  │ - checkpoint_id    │  │ (introspection)    │  │ _checkpoints       │     │
│  │ - session_id       │  └────────────────────┘  └────────────────────┘     │
│  │ - state_json (Step)│  task_contracts (active durable tasks)              │
│  │ - summary (tier)   │                                                      │
│  └────────────────────┘                                                      │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 2.2 Core Components

| Component | File | Purpose |
|-----------|------|---------|
| `StateSyncService` | `state_sync.rs` | Abstract trait for sync operations |
| `MatrixOneSyncService` | `state_sync.rs` | Cloud sync via sqlx/MySQL |
| `LocalOnlySyncService` | `state_sync.rs` | No-op for offline mode |
| `JournalWriter` | `session_journal.rs` | Local JSONL persistence |
| `JournalEvent` | `session_journal.rs` | Event types for audit trail |
| `HybridRestoreService` | `session_restore.rs` | Local-first session restore; cloud fallback for `agent_sessions`, checkpoints, learning |
| `step_restore` | `runtime/.../step_restore.rs` | Step Protocol: heavy checkpoint + JSONL replay (distinct type from services `RestoredSession`) |
| `DeltaSnapshot` | `state_sync.rs` | Incremental sync payload |
| `VersionedSnapshot` | `state_sync.rs` | Optimistic locking support |

---

## 3. Current Full-Sync Mechanism

### 3.1 Full Snapshot Sync (`push_learning`)

```rust
// Pseudo-code from state_sync.rs analysis
async fn push_learning(
    &self,
    user_id: &str,
    profile: &str,
    snapshot_json: &str,      // Full JSON ~40KB
    entity_count: u32,
    pattern_count: u32,
    has_calibration: bool,
) -> SyncResult
```

**Serialization Format:**
```rust
// Payload encoding pipeline
snapshot_json (raw JSON)
    → GzEncoder (Compression::default())
    → base64::STANDARD.encode()
    → Stored in snapshot_json column
```

**Network Payload Structure:**
```
snapshot_json: base64(gzip(JSON))
- Raw JSON: ~40KB
- Compressed: ~8-12KB
- Base64 encoded: ~11-16KB
```

### 3.2 Versioned Sync (`push_learning_versioned`)

Uses optimistic locking to prevent concurrent overwrites:

```rust
pub struct VersionedSnapshot {
    pub json: String,       // Full snapshot JSON
    pub version: i64,       // Monotonically increasing
}
```

**Flow:**
1. Pull: Get `(json, version)` from cloud
2. Merge: Combine with local changes
3. Push: `UPDATE ... WHERE version = expected_version`
4. On conflict (version mismatch): Return `is_conflict=true`

### 3.3 Retry & Resilience

```rust
const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 100;
const MAX_BACKOFF_MS: u64 = 2000;

// Retryable errors: Io, PoolTimedOut, Protocol, specific MySQL codes
// (1040, 1205, 1213, 2006, 2013)
```

---

## 4. Incremental Sync (Delta) - Current Implementation

### 4.1 DeltaSnapshot Structure

```rust
pub struct DeltaSnapshot {
    pub baseline_epoch: u64,           // Unix timestamp of last sync
    pub entity_deltas: Vec<Value>,     // Changed entities only
    pub pattern_deltas: Vec<Value>,   // Changed patterns only
    pub calibration: Option<Value>,   // Full replacement (small)
    pub tool_health_deltas: Vec<Value>,
    pub delta_count: u32,
}
```

**Size Comparison:**
| Type | Typical Size | Reduction |
|------|--------------|-----------|
| Full Snapshot | ~40KB | Baseline |
| Delta Snapshot | 2-5KB | 85-90% |

### 4.2 Delta Sync Algorithm (`push_delta`)

```rust
async fn push_delta(
    &self,
    user_id: &str,
    profile: &str,
    delta_json: &str,           // JSON-serialized DeltaSnapshot
    expected_version: Option<i64>,
) -> SyncResult
```

**Server-side Merge Strategy:**
1. Fetch current snapshot from cloud
2. Parse delta and current snapshot
3. Merge entries:
   - **Entities**: Replace by `name` key
   - **Patterns**: Replace by `signature` key
   - **Calibration**: Full replacement
   - **Tool Health**: Replace by `name` key
4. Store merged result with incremented version

---

## 5. Session Journal (Local Audit Trail)

### 5.1 JournalEvent Types

```rust
pub enum JournalEventType {
    SessionStart,      // Session initialization
    Turn,              // Successful conversation turn
    TurnError,         // Failed turn
    Compact,           // Context compaction
    ConfigChange,      // Settings update
    Error,             // Non-turn error
    SessionEnd,        // Session termination
    StallDetected,     // Non-happy path detection
    Checkpoint,        // Manual/auto checkpoint
    TurnGuardVerdict,  // Unified non-happy-path audit
    PlanProgress,      // Subtask execution
}
```

### 5.2 JournalEvent Structure

```rust
pub struct JournalEvent {
    pub event_type: JournalEventType,
    pub ts: String,                    // ISO 8601 timestamp
    pub session_id: Option<String>,
    pub turn: Option<u32>,
    pub model: Option<String>,
    pub user_input: Option<String>,    // Truncated to 500 chars
    pub assistant_output: Option<String>, // Truncated to 1000 chars
    pub tool_count: Option<u32>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
    pub tool_calls: Option<Vec<ToolCallRecord>>,
    pub metadata: Option<Value>,       // Flexible extension point
    // ... additional fields
}
```

### 5.3 ToolCallRecord (Granular Audit)

```rust
pub struct ToolCallRecord {
    pub name: String,
    pub ok: bool,
    pub ms: u64,           // Execution time
    pub error: Option<String>,
}
```

### 5.4 Storage Format

- **Location**: `~/.astra/sessions/{session_id}.jsonl` (see `session_journal::local_sessions_dir()` in `session_journal.rs`; tests may override per-thread)
- **Format**: One JSON line per event (append-only)
- **Truncation**: User input 500 chars, assistant output 1000 chars
- **Disk Full Handling**: Logs error, drops event (graceful degradation)

---

## 6. Synchronization Pain Points

### 6.1 Full-Sync Bottlenecks

| Issue | Impact | Current Mitigation |
|-------|--------|-------------------|
| **Payload Size** | ~40KB uncompressed, ~16KB over wire | Gzip + Base64 compression |
| **Network Latency** | Each sync = 1 RTT + DB write time | Retry with exponential backoff |
| **Version Conflicts** | Concurrent sessions may conflict | Optimistic locking + conflict detection |
| **Merge Complexity** | Full snapshot must be fetched to merge | Delta sync for incremental updates |

### 6.2 Critical Path Analysis

```
Full Sync Latency:
┌─────────────┐   ┌─────────────┐   ┌─────────────┐   ┌─────────────┐
│  Serialize  │ → │  Compress   │ → │   Network   │ → │  DB Write   │
│   (~1ms)    │   │   (~2ms)    │   │  (~50ms)    │   │  (~10ms)    │
└─────────────┘   └─────────────┘   └─────────────┘   └─────────────┘
                                                      ↑
                                               Single-row UPDATE
                                               with version check

Delta Sync Latency:
┌─────────────┐   ┌─────────────┐   ┌─────────────┐   ┌─────────────┐
│  Compute    │ → │  Serialize  │ → │   Network   │ → │  DB Ops     │
│   Delta     │   │   (~0.5ms)  │   │  (~15ms)    │   │  (~20ms)    │
│  (~5ms)     │   │             │   │             │   │  Pull+Merge │
└─────────────┘   └─────────────┘   └─────────────┘   │  +Write     │
                                                      └─────────────┘
```

**Note:** Delta sync has higher DB operation cost (read+merge+write) but lower network overhead.

### 6.3 Conflict Resolution Limitations

1. **No Automatic Retry**: On version conflict, caller must re-pull, re-merge, and retry
2. **Last-Writer-Wins for Preferences**: No merge semantics for user preferences
3. **Full Entity Replacement**: Delta merge replaces entire entity, not field-level patches

---

## 7. Incremental Sync Insertion Points

### 7.1 Recommended Delta Triggers

| Trigger | Location | Description |
|---------|----------|-------------|
| **Turn-based** | After each conversation turn | Queue changes since last sync |
| **Time-based** | Every 30 seconds (configurable) | Batch accumulated changes |
| **Size-based** | When delta > 1KB | Prevent excessive accumulation |
| **Event-based** | On Checkpoint, Compact, ConfigChange | Critical events sync immediately |
| **Session End** | On session termination | Final flush of pending deltas |

### 7.2 Proposed Delta Buffer Architecture

```rust
// New component: DeltaBuffer (suggested location: state_sync.rs)
pub struct DeltaBuffer {
    baseline_epoch: u64,
    entity_changes: HashMap<String, Value>,  // Keyed by entity name
    pattern_changes: HashMap<String, Value>, // Keyed by pattern signature
    calibration: Option<Value>,
    tool_health_changes: HashMap<String, Value>,
    dirty: bool,
}

impl DeltaBuffer {
    /// Record an entity change
    pub fn record_entity(&mut self, entity: &Entity) {
        self.entity_changes.insert(entity.name.clone(), serialize(entity));
        self.dirty = true;
    }
    
    /// Generate DeltaSnapshot for sync
    pub fn to_delta(&self) -> DeltaSnapshot {
        DeltaSnapshot {
            baseline_epoch: self.baseline_epoch,
            entity_deltas: self.entity_changes.values().cloned().collect(),
            pattern_deltas: self.pattern_changes.values().cloned().collect(),
            calibration: self.calibration.clone(),
            tool_health_deltas: self.tool_health_changes.values().cloned().collect(),
            delta_count: self.total_changes(),
        }
    }
}
```

### 7.3 Integration Points

```
┌─────────────────────────────────────────────────────────────────┐
│                        Current Flow                             │
├─────────────────────────────────────────────────────────────────┤
│  Turn/Event → JournalWriter → (async) → push_learning()         │
│                                         (Full snapshot)         │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│                      Proposed Flow                              │
├─────────────────────────────────────────────────────────────────┤
│  Turn/Event → JournalWriter ─┬──→ DeltaBuffer.record_change()   │
│                              │                                  │
│                              └──→ Trigger ─┬──→ push_delta()    │
│                                            │    (Incremental)   │
│                                            └──→ push_learning() │
│                                                 (Periodic full)  │
└─────────────────────────────────────────────────────────────────┘
```

---

## 8. Checkpoint & Session Restore

### 8.1 Checkpoint Types

| Type | Location | Use Case |
|------|----------|----------|
| **Session Checkpoint** | `session_checkpoints` (MatrixOne) + local index under `~/.astra/sessions/<id>/` | Rewind to specific turn; Step Protocol stores `state_json` (heavy tier uses `summary = 'heavy'`) |
| **Composite snapshot index** | `~/.astra/sessions/<session_id>/step_checkpoints/composite_snapshots.json` | Multi-dimension restore (session/data/git refs); read by `HybridRestoreService::list_composite_snapshots` |
| **Data Versioning** | `data_versioning_checkpoints` | Experiment isolation |
| **Journal Checkpoint** | `JournalEvent::Checkpoint` | Event audit trail |
| **Active task contract** | `task_contracts` (`session_id`, `status = 'active'`) | Restored with cloud session; fallback from latest checkpoint `contract_state_json` |

### 8.2 Checkpoint Data Flow

```
┌─────────────────┐     ┌──────────────────┐     ┌─────────────────┐
│  Session        │────▶│  JournalEvent    │────▶│  Cloud Sync     │
│  Checkpoint     │     │  ::checkpoint()  │     │  (async)        │
└─────────────────┘     └──────────────────┘     └─────────────────┘
         │                                               │
         ▼                                               ▼
┌─────────────────┐                            ┌─────────────────┐
│  session_check- │                            │  agent_events   │
│  points table   │                            │  table          │
└─────────────────┘                            └─────────────────┘
```

### 8.3 `HybridRestoreService` (services crate) — verified behavior

Implementation: `rust/crates/services/src/session_restore.rs`.

- **`restore_session(session_id)`**
  - **Local-first**: If `workspace.yaml` exists for the session (`session_workspace::read_workspace`), returns `restored_from_cloud: false` with turn/tokens/plan/contract fields from workspace. When a DB pool is configured, still queries **`restore_recent_tools`**: last 5 `agent_events` rows with `event_type = 'turn_complete'`, merging `metadata.tools_used`.
  - **Cloud fallback**: Loads `agent_sessions`; `turn_count` comes from **`event_count`** on that row. Plan fields are parsed from **`metadata` JSON** (`extract_plan_from_metadata`). Contract: **`task_contracts`** for active row, else last **`contract_state_json`** from **`session_checkpoints`**.
  - **Learning**: Local branch calls `restore_learning("local", "default")` when a pool exists (queries `learning_snapshots` by user/profile — the `"local"` label is historical naming). With `HybridRestoreService::local_only()`, learning is not loaded from cloud here.

- **`list_checkpoints`**: Prefers **local** checkpoint index (`session_checkpoint::read_checkpoint_index`); if empty, **`session_checkpoints`** in MatrixOne.

- **`restore_to_checkpoint`**: Calls `restore_session`, then rewinds **`turn_count`**, **`total_tokens_in`**, **`checkpoint_count`** from the target checkpoint row; may fill **`contract_json`** from the checkpoint.

- **`pull_step_checkpoint_from_cloud`**: Selects the row with **`summary = 'heavy'`** and non-null **`state_json`**, **highest `number`** — matches how Step checkpoints are pushed (`push_step_checkpoint_to_cloud`).

- **`restore_to_composite_snapshot`**: Reads **only local** `composite_snapshots.json`. If `RestoreSelector.restore_session_state` and the snapshot references `NNNNNN-heavy.json`, delegates to **`restore_to_checkpoint`** (hence local index or cloud checkpoints as above).

**Skill fork sub-runs** do not use this API: they are a separate `AgenticLoopHost` path (`cli/skill_subrun.rs`); no `session_restore` mapping.

### 8.4 CLI `/resume` layering (astra)

`rust/crates/astra-cli/src/main.rs` (`handle_resume_command`):

1. Optional listing merges **`list_resumable_sessions(user_id)`** (cloud) with **`list_sessions_by_time`** (local); **cloud entry wins** on duplicate `session_id`.
2. After `HybridRestoreService::restore_session`, applies **`step_restore::restore_session`** (local Step Protocol + JSONL). On failure, **`pull_step_checkpoint_from_cloud`** as fallback.
3. Conversation history: **`restore_history_from_journal`** (local JSONL, session segmentation).

Two different **`RestoredSession`** types exist: **`astra_services::session_restore::RestoredSession`** (hybrid metadata) vs **`astra_runtime::pipeline::step_restore::RestoredSession`** (messages, idempotency cache, protocol version). The REPL uses both in sequence.

### 8.5 Skills: edge registry vs cloud catalog vs HTTP

- **Edge `UnifiedSkillRegistry` (REPL default)** — `repl_runtime.rs`: **`LocalSkillProvider`** + **`BundledSkillProvider`**, eager **`discover_all()`**, MCP discover, **`SkillWatcher`** for filesystem reload. **`DatabaseSkillProvider`** exists in code but is **not** wired into this default path (only unit-tested in `providers/database.rs`).
- **Cloud-assembled skill index** — In edge-cloud mode, the model-facing catalog slice is built during **cloud context assembly** for `/chat/turn` (see `docs/design/edge-cloud-execution.md`). That is **not** the same object as the edge process registry.
- **On-demand catalog HTTP** — `ThinClient::get_skills_query_text` → **`GET /skills`** for slash commands / marketplace flows (`command_router.rs`, `slash_skill.rs`). Separate from **`discover_all()`** cache invalidation; installing a skill often triggers **`unified_skill_registry.discover_all()`** to refresh the in-memory manifest cache.

---

## 9. Recommendations

### 9.1 Short-term (Immediate)

1. **Enable Delta Sync by Default**
   - Change default sync mode from full to delta
   - Add configuration: `sync_mode: "delta" | "full" | "adaptive"`

2. **Add Delta Buffer to Session State**
   - Track entity/pattern changes in memory
   - Flush on configurable triggers (time, size, events)

3. **Optimize Merge Strategy**
   - Consider field-level patches for large entities
   - Add `changed_fields` tracking to reduce merge overhead

### 9.2 Medium-term

1. **Conflict-Free Replicated Data Types (CRDTs)**
   - Investigate CRDTs for automatic conflict resolution
   - Particularly useful for tool health and calibration data

2. **Differential Synchronization**
   - Implement diff-match-patch for text-heavy data
   - Reduces payload for pattern descriptions

3. **Sync Scheduler**
   - Batch syncs during idle periods
   - Prioritize critical events (errors, checkpoints)

### 9.3 Long-term

1. **Edge-to-Edge Sync**
   - Support multiple edge devices per user
   - Device-aware conflict resolution

2. **Selective Sync**
   - Allow users to exclude specific entity types
   - Profile-specific sync policies

---

## 10. Appendix: Data Schemas

### 10.1 learning_snapshots Table

```sql
CREATE TABLE learning_snapshots (
    snapshot_id VARCHAR(36) PRIMARY KEY,
    user_id VARCHAR(64) NOT NULL,
    profile_name VARCHAR(64) NOT NULL,
    snapshot_json LONGTEXT NOT NULL,  -- base64(gzip(json))
    entity_count BIGINT,
    pattern_count BIGINT,
    has_calibration INT,
    version BIGINT DEFAULT 1,
    created_at TIMESTAMP,
    updated_at TIMESTAMP,
    INDEX idx_user_profile (user_id, profile_name)
);
```

### 10.2 session_sync_log Table

```sql
CREATE TABLE session_sync_log (
    sync_id VARCHAR(36) PRIMARY KEY,
    user_id VARCHAR(64) NOT NULL,
    session_id VARCHAR(64),
    sync_type VARCHAR(32),      -- "learning", "delta", "preference"
    sync_direction VARCHAR(8),  -- "push", "pull"
    payload_size BIGINT,
    status VARCHAR(16),         -- "success", "error", "conflict", "pending"
    error_message TEXT,
    created_at TIMESTAMP,
    INDEX idx_user_status (user_id, status, created_at)
);
```

### 10.3 JournalEvent JSON Schema

```json
{
  "type": "turn",
  "ts": "2025-01-15T10:30:00Z",
  "session_id": "sess-uuid",
  "turn": 5,
  "model": "gpt-4",
  "user_input": "Hello...",
  "assistant_output": "Hi there...",
  "tool_count": 2,
  "tokens_in": 150,
  "tokens_out": 80,
  "duration_ms": 1200,
  "tools_selected": ["bash", "read_file"],
  "tools_used": ["bash"],
  "budget_used": 25,
  "budget_pressure": 0.3,
  "tool_calls": [
    {"name": "bash", "ok": true, "ms": 150}
  ],
  "metadata": null
}
```

---

## 11. Team Review Checklist

- [ ] **Pain Points Validated**: Do identified bottlenecks match observed issues?
- [ ] **Delta Strategy**: Is the proposed DeltaBuffer approach acceptable?
- [ ] **Trigger Selection**: Are the suggested sync triggers appropriate?
- [ ] **Schema Changes**: Any objections to proposed table modifications?
- [ ] **Migration Path**: How to transition existing sessions to delta sync?

---

**Document Status**: Ready for team review  
**Next Steps**: Schedule architecture review meeting, assign implementation tickets
