# Multi-Agent Cloud Runtime Architecture

> **Status**: Living Design Document  
> **Version**: 1.0  
> **Scope**: Edge-cloud state management, multi-agent orchestration, and cloud-scale execution  
> **Audience**: Core contributors, architecture reviewers

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Industry Landscape & Competitive Position](#2-industry-landscape--competitive-position)
3. [Current Architecture: What We Have](#3-current-architecture-what-we-have)
4. [Gap Analysis: Design vs Implementation](#4-gap-analysis-design-vs-implementation)
5. [Target Architecture: Multi-Agent Cloud Runtime](#5-target-architecture-multi-agent-cloud-runtime)
6. [Edge-Cloud State Model](#6-edge-cloud-state-model)
7. [Multi-Agent Coordination Protocol](#7-multi-agent-coordination-protocol)
8. [Task Leasing & Distributed Execution](#8-task-leasing--distributed-execution)
9. [Learning Convergence for Multi-Agent](#9-learning-convergence-for-multi-agent)
10. [Token Efficiency at Scale](#10-token-efficiency-at-scale)
11. [Observability & Trust](#11-observability--trust)
12. [Migration Path](#12-migration-path)
13. [Appendix: Industry Comparison Matrix](#appendix-a-industry-comparison-matrix)

---

## 1. Executive Summary

mo-agent is an **Agentic Runtime** — a managed execution environment where every agent automatically receives auditable decisions, versioned memory, safe experimentation, cost control, and trust verification. The core thesis:

```
Agent Decision = f(prompt@version, skill@version, context@snapshot, memory@state, llm_params)
```

**Current state**: A mature single-agent, local-first system with production-ready learning sync, event ingestion, session management, and a 55-tool edge toolkit. The runtime implements a 5-stage cognitive pipeline (Perceive→Plan→Execute→Evaluate→Reflect) with progressive self-calibration.

**Target state**: A multi-agent cloud runtime where:
- Multiple agents coordinate on complex tasks via structured protocols
- Tasks are leased, checkpointed, and resumable across agent failures
- Learning converges across agents via conflict-free merge strategies
- Edge nodes drive execution while cloud provides orchestration, persistence, and governance

**Key differentiators vs industry**:
- **Cross-session learning** (no competitor does this — Codex, Claude Code, Cursor, Devin all start fresh)
- **Edge-cloud split execution** (tools local, reasoning cloud — unlike Codex's full-cloud or Claude's full-local)
- **Self-improving tool selection** (TF-IDF + LLM hybrid with progressive calibration)
- **Durable long-horizon tasks** (event-sourced state machine, not request-bound)

---

## 2. Industry Landscape & Competitive Position

### 2.1 Architectural Patterns Across Competitors

| System | Execution Model | State Model | Multi-Agent | Learning | Token Efficiency |
|--------|----------------|-------------|-------------|----------|------------------|
| **OpenAI Codex** | Cloud container, stateless | Client-side session | None | None cross-session | Aggressive pruning (~60%) |
| **Claude Code** | Local CLI, remote LLM | Local SQLite | None | None cross-session | 200K window, hierarchical |
| **Cursor/Windsurf** | IDE-embedded, local+cloud | FSWatch + AST index | None | None | Symbol-based injection (~95%) |
| **Devin/SWE-agent** | Docker isolation | Git snapshots + action log | None | None cross-session | Full state serialization |
| **LangGraph** | Framework, pluggable | Typed checkpoints (SQLite/PG) | Shared state graph | None | Per-thread isolation |
| **CrewAI** | Framework, role-based | Role-local, optional vector DB | Sequential/parallel chains | Optional entity memory | Full history replay |
| **mo-agent** | Edge-cloud split | JSONL journal + MatrixOne cloud | Designed (partial impl) | ✅ Cross-session EntityGraph | Intent-driven loading (~90%) |

### 2.2 Where We Lead

1. **Cross-session learning**: EntityGraph + PatternLibrary + ProgressiveCalibrator persist and evolve across sessions. No competitor does this.
2. **Edge-cloud architecture**: Tools execute locally (100ms latency), LLM reasoning goes to cloud. Combines Claude Code's privacy with Codex's cloud power.
3. **Self-improving selection**: ToolQualityTracker biases future selections based on historical outcomes. FallbackSelector uses TF-IDF fast path with LLM verification.
4. **Intent-driven context**: Load only task-relevant memory (preference query: ~100 tokens vs full memory: ~2400 tokens). 60% token savings.

### 2.3 Where We Must Catch Up

1. **Multi-agent coordination**: LangGraph and CrewAI have working multi-agent patterns; our coordination patterns are design-only
2. **Durable long-horizon tasks**: Devin and Codex can run hour-long tasks; our tasks are still request-bound
3. **IDE integration**: Cursor/Windsurf have native AST indexing; we have tree-sitter but CLI-only
4. **Real-time collaboration**: Windsurf supports multiplayer; we're single-user

---

## 3. Current Architecture: What We Have

### 3.1 Crate Structure

```
rust/crates/
├── mo-agent/        # Edge runtime: CLI REPL, 55 tools, sync adapters, plan decomposition
│   ├── main.rs              # Entry point, REPL loop, session management
│   ├── edge_tools.rs        # 55 tools: bash, file ops, git (gix), code intel, web, memory
│   ├── mo_agent/
│   │   ├── chat_stream.rs   # SSE streaming, LLM integration, event persistence
│   │   ├── repl_turn.rs     # Single turn execution
│   │   ├── plan_decompose.rs# Long-horizon planning
│   │   └── sync_adapters.rs # LearningAdapter, EventAdapter, TaskAdapter (stub)
│   └── edge_tools/
│       ├── code_intel.rs    # 10 tree-sitter AST tools
│       ├── git_gix.rs       # Pure-Rust git (no binary dependency)
│       └── build_test.rs    # Build-test loop with error delta tracking
│
├── runtime/         # Cognitive pipeline: tool selection, turn execution, learning
│   ├── tool_selector.rs     # LearnedContext, TfIdf/LLM/FallbackSelector
│   ├── tool_registry/       # 8 modules: registry, scoring, report, meta
│   ├── turn/                # 38 modules: bridge, stall detection, error recovery, health
│   │   ├── bridge_inprocess.rs  # In-process ChatTurnBridge with prompt caching
│   │   ├── tool_health.rs       # Session-scoped error budgets, deprioritization
│   │   └── stall.rs             # TurnGuard, intent drift detection
│   └── pipeline/            # 18 modules: cognitive engine
│       ├── engine.rs            # Perceive→Plan→Execute→Evaluate→Reflect
│       ├── entity.rs            # EntityGraph: entity→domain→tools knowledge
│       ├── pattern.rs           # PatternLibrary: tool chain patterns, drift detection
│       ├── calibration.rs       # ProgressiveCalibrator: 3-axis confidence thresholds
│       └── persistence.rs       # Local + cloud persistence for learning data
│
├── services/        # Cloud integration: sync, sessions, events, tasks
│   ├── sync_engine.rs       # SyncOrchestrator + DomainAdapter trait (5 domains)
│   ├── state_sync.rs        # Delta/snapshot sync with optimistic locking
│   ├── session_journal.rs   # Append-only JSONL logs with version tracking
│   ├── session_restore.rs   # HybridRestoreService: local + cloud
│   ├── event_ingestion.rs   # Async batch ingestion, at-least-once delivery
│   └── task_orchestrator.rs # TaskRecord lifecycle with checkpoint/resume
│
├── core/            # Shared config, logging, runtime limits
└── mo-admin/        # Admin CLI: credentials, model config, roles
```

### 3.2 State Model: What Exists Today

#### Session State
```
~/.mo-agent/sessions/{session_id}/
├── workspace.yaml       # Session metadata: git branch, model, title, token counts
├── {session_id}.jsonl   # Append-only journal: turns, tool calls, state changes
└── checkpoints/         # Numbered checkpoint snapshots
```

#### Learning State (per-user, per-profile)
```rust
EntityGraph {
    entities: HashMap<String, EntityKnowledge>,  // entity → domain → tools mapping
    // Time-decayed confidence, observation counts
}

PatternLibrary {
    patterns: Vec<ToolChainPattern>,  // tool sequences + success rates
    // Drift detection via moving average
}

ProgressiveCalibrator {
    intent_axes: HashMap<String, CalibrationAxis>,  // per-intent thresholds
    domain_axes: HashMap<String, CalibrationAxis>,
    task_axes: HashMap<String, CalibrationAxis>,
}

ToolQualityTracker {
    entries: Vec<ToolHealthEntry>,  // per-tool success/failure/latency
}
```

#### Sync State Machine
```
Clean ──write──▶ Dirty ──push──▶ Syncing ──ok──▶ Clean
                                    │
                                 conflict
                                    ▼
                                Conflict ──resolve──▶ Dirty
```

**Five sync domains**: Learning, Events, Tasks, Templates, Preferences  
**Transport**: MatrixOneTransport (currently wired for Learning only)

### 3.3 Production Readiness Assessment

| Component | Status | Evidence |
|-----------|--------|----------|
| Edge tool execution (55 tools) | ✅ Production | All tools implemented, no stubs |
| Session journal + workspace | ✅ Production | Append-only JSONL, version tracking |
| Event ingestion (async batch) | ✅ Production | Backpressure, idempotent, at-least-once |
| Learning sync (EntityGraph) | ✅ Production | Delta support, optimistic locking, gzip compression |
| Tool selection (FallbackSelector) | ✅ Production | TF-IDF + LLM hybrid, learned context reuse |
| Stall/error detection | ✅ Production | Intent drift, name stall, error budgets, circuit breaker |
| Code intelligence (tree-sitter) | ✅ Production | 10 AST tools, 8 languages, 44 tests |
| Git operations (gix) | ✅ Production | Pure-Rust, 46 tests, no binary dependency |
| Task orchestrator | ⚠️ Moderate | Checkpoint/resume works; no concurrent edit safety |
| Sync adapters (Event/Task) | ❌ Stub | Return errors on export/merge |
| Multi-agent coordination | ❌ Design only | Fan-out/pipeline patterns documented but not wired |
| Durable long-running tasks | ❌ Design only | AgentRun record exists; RunEngine not wired |

---

## 4. Gap Analysis: Design vs Implementation

### 4.1 Critical Gaps

| # | Gap | Design Doc | Implementation | Impact |
|---|-----|-----------|----------------|--------|
| G1 | **Durable agent runs** | durable-agent-runs.md: full RunEngine, AsyncToolRegistry, multi-day workflows | AgentRun record exists; RunEngine not wired to ChatLoop | Cannot run tasks spanning hours/days |
| G2 | **Multi-agent orchestration** | agents-and-orchestration.md: Fan-Out/Fan-In, Pipeline, Adversarial Review | Basic delegation skill exists; coordination patterns incomplete | Cannot run agent teams |
| G3 | **Task leasing & ownership** | Not designed | TaskRecord has `user_id` but no `agent_id` ownership, no lease TTL | Multiple agents can't claim tasks safely |
| G4 | **Distributed plan execution** | plans/cloud-edge-redesign-v2.md: PlanState with CRDT merge | Plan events logged; no distributed executor | Plans can't span multiple agents |
| G5 | **EventAdapter sync** | sync_engine.rs: DomainAdapter trait | Stub: returns errors on export | Events don't sync through unified engine |
| G6 | **TaskAdapter sync** | sync_engine.rs: DomainAdapter trait | Stub: returns errors on export | Tasks don't sync through unified engine |
| G7 | **Cross-agent learning merge** | state_sync.rs: observation-count-wins merge | Single-writer assumption; no 3-way merge | Multiple agents writing creates conflicts |

### 4.2 Design Docs That Outpace Implementation

| Document | Designed | Implemented |
|----------|----------|-------------|
| durable-agent-runs.md | 100% | ~15% (records only) |
| multi-agent-delegation-guide.md | 100% | ~40% (basic delegation) |
| cloud-edge-redesign-v2.md | 100% | ~30% (learning sync only) |
| context-window-management.md | 100% | ~60% (basic budget, no zones) |
| evaluation-and-evolution.md | 100% | ~30% (metrics schema, no auto-gate) |

### 4.3 What's Solid and Should Not Change

- **Local-first journal**: Append-only JSONL is the correct foundation. Fast, crash-safe, auditable.
- **Sync envelope state machine**: Clean→Dirty→Syncing→Conflict is correct. Extend, don't replace.
- **DomainAdapter trait**: The trait signature is well-designed. Fill in stub implementations.
- **FallbackSelector pattern**: TF-IDF fast path + LLM verification is the right hybrid. Extend with learned context.
- **LearnedContext flow**: Entity/pattern/calibration/tool hints as "priors, not hard requirements" is correct.

---

## 5. Target Architecture: Multi-Agent Cloud Runtime

### 5.1 Design Principles

1. **Local-first, cloud-enhanced**: Edge always works offline. Cloud adds durability, coordination, and governance.
2. **Event-sourced state**: All state changes are events. Current state = fold(events). Enables replay, audit, time-travel.
3. **Leased ownership**: Tasks and resources have TTL-based leases. If an agent dies, its lease expires and work is reclaimable.
4. **Conflict-free merge**: Use CRDTs and domain-specific merge strategies. Avoid distributed locks wherever possible.
5. **Progressive trust**: Start with single-agent. Add multi-agent coordination. Then add cross-team collaboration. Each layer adds trust requirements.

### 5.2 Architectural Layers

```
┌──────────────────────────────────────────────────────────────────────────┐
│                              CLIENTS                                     │
│  CLI (mo-agent) │ IDE Plugin │ Web UI │ SDK │ Webhook                    │
│  All speak the same /chat/turn SSE protocol                             │
└────────────────────┬─────────────────────────────────────────────────────┘
                     │
┌────────────────────▼─────────────────────────────────────────────────────┐
│                         EDGE RUNTIME                                     │
│  EdgeChatLoop │ 55 Local Tools │ MCP Servers │ Permission System         │
│  Drives agentic loop. Executes tools locally. Syncs state to cloud.     │
│                                                                          │
│  ┌─────────────────────────────────────────────────────────────────┐     │
│  │ Cognitive Pipeline: Perceive → Plan → Execute → Evaluate → Reflect │  │
│  │ FallbackSelector (TF-IDF + LLM) │ LearnedContext │ StallGuard    │  │
│  └─────────────────────────────────────────────────────────────────┘     │
│                                                                          │
│  ┌──────────────────────────────────────────────────┐                   │
│  │ SyncOrchestrator                                  │                   │
│  │ ├── LearningAdapter  (✅ production)              │                   │
│  │ ├── EventAdapter     (→ wire to batch ingestion)  │                   │
│  │ ├── TaskAdapter      (→ implement lease protocol) │                   │
│  │ ├── TemplateAdapter  (→ cold start bootstrap)     │                   │
│  │ └── PreferenceAdapter(→ bidirectional sync)       │                   │
│  └──────────────────────────────────────────────────┘                   │
└────────────────────┬─────────────────────────────────────────────────────┘
                     │ SSE + REST (/chat/turn, /sync/*, /tasks/*)
┌────────────────────▼─────────────────────────────────────────────────────┐
│                       CLOUD PLATFORM                                     │
│                                                                          │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐                   │
│  │ API Gateway   │  │ Task Router  │  │ Agent Registry│                  │
│  │ Auth, Rate    │  │ Lease Mgmt   │  │ Heartbeat    │                   │
│  │ Limit, Route  │  │ Priority Q   │  │ Capabilities │                   │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘                   │
│         │                  │                  │                           │
│  ┌──────▼──────────────────▼──────────────────▼───────┐                  │
│  │               Orchestration Layer                   │                  │
│  │  SessionOrchestrator │ PlanExecutor │ LearningCtrl  │                 │
│  │  ConflictResolver │ DriftDetector │ FeedbackMiner   │                 │
│  └──────────────────────┬─────────────────────────────┘                  │
│                         │                                                │
│  ┌──────────────────────▼─────────────────────────────┐                  │
│  │                 Storage Layer                       │                  │
│  │  MatrixOne (HTAP) │ Redis (cache) │ S3 (artifacts)  │                │
│  │                                                     │                 │
│  │  Tables:                                            │                 │
│  │  ├── agent_sessions    (session lifecycle)          │                 │
│  │  ├── agent_events      (append-only audit trail)    │                 │
│  │  ├── agent_tasks       (task records + checkpoints) │                 │
│  │  ├── learning_snapshots(gzip+base64, versioned)     │                 │
│  │  ├── plan_templates    (reusable plan patterns)     │                 │
│  │  ├── user_preferences  (model, explain mode, etc.)  │                 │
│  │  ├── agent_registry    (NEW: agent capabilities)    │                 │
│  │  └── task_leases       (NEW: distributed ownership) │                 │
│  └────────────────────────────────────────────────────┘                  │
└──────────────────────────────────────────────────────────────────────────┘
```

---

## 6. Edge-Cloud State Model

### 6.1 State Domains & Sync Semantics

| Domain | Direction | Conflict Strategy | Sync Trigger | Payload Size |
|--------|-----------|-------------------|-------------|-------------|
| **Learning** | Bidirectional | Union merge (observation-count-wins) | Session start/end | 2-5 KB delta |
| **Events** | Edge→Cloud | Append-only (INSERT IGNORE) | Per-turn async batch | 1-3 KB/event |
| **Tasks** | Bidirectional | Lease-based ownership | On task state change | 2-8 KB/task |
| **Templates** | Cloud→Edge | Read-only (cloud authoritative) | Cold start + periodic | 5-20 KB |
| **Preferences** | Bidirectional | Last-writer-wins (timestamp) | On change | <1 KB |

### 6.2 Unified SyncableState Trait

The existing `DomainAdapter` trait is the correct abstraction. The gap is **implementations**:

```rust
// EXISTING (sync_engine.rs) — keep this trait as-is
pub trait DomainAdapter: Send + Sync {
    fn domain(&self) -> SyncDomain;
    fn export_full(&self) -> Result<SyncPayload, SyncError>;
    fn export_delta(&self) -> Result<Option<SyncPayload>, SyncError>;
    fn merge_remote(&self, remote: &SyncPayload) -> Result<MergeResult, SyncError>;
    fn resolve_conflict(&self, local: &SyncPayload, remote: &SyncPayload)
        -> Result<SyncPayload, SyncError>;
    fn validate(&self, payload: &SyncPayload) -> Result<(), SyncError>;
    fn envelope(&self) -> SyncEnvelope;
    fn set_envelope(&self, envelope: SyncEnvelope);
    fn has_dirty_data(&self) -> bool;
    fn estimated_size(&self) -> usize;
    fn clear_dirty(&self) -> Result<(), SyncError>;
}
```

**What must change**: EventAdapter and TaskAdapter need real implementations instead of stubs.

### 6.3 EventAdapter: Wire to Batch Ingestion

The EventAdapter currently returns errors. It should delegate to the existing `IngestionSender`:

```rust
// TARGET: EventAdapter delegates to existing batch ingestion pipeline
impl DomainAdapter for EventAdapter {
    fn export_delta(&self) -> Result<Option<SyncPayload>> {
        // Events use their own IngestionWorker pipeline (batch size=20, flush interval=5s)
        // SyncOrchestrator should not duplicate this — return None to indicate
        // "I handle my own sync"
        Ok(None)
    }
    fn has_dirty_data(&self) -> bool {
        // Check IngestionSender's pending count
        self.sender.pending_count() > 0
    }
    // merge_remote: no-op (events are append-only, never pulled from cloud)
}
```

**Key insight**: EventAdapter doesn't need full DomainAdapter semantics. Events are write-once, edge→cloud. The existing IngestionWorker already handles batching, retries, and idempotency. The adapter should be a thin bridge, not a reimplementation.

### 6.4 Session State Restoration

Current `RestoredSession` restores:
- ✅ Session metadata (git branch, model, title)
- ✅ Recent tools (last 5 turn_complete events)
- ✅ Learning snapshot (gzip+base64 from cloud)
- ✅ Checkpoints (numbered, rewindable)
- ⚠️ Conversation messages (partial — recent only)
- ❌ Active plan state (minimal)
- ❌ Dynamic tool availability

**Target**: Full restoration including plan state and conversation context:
```rust
pub struct RestoredSession {
    // ... existing fields ...

    // NEW: Full plan restoration
    pub active_plan: Option<RestoredPlan>,
    pub plan_subtask_progress: Vec<SubtaskProgress>,

    // NEW: Conversation context for LLM continuity
    pub conversation_summary: Option<String>,  // LLM-generated summary of prior context
    pub key_decisions: Vec<DecisionRecord>,     // Auditable decision points
}
```

---

## 7. Multi-Agent Coordination Protocol

### 7.1 Agent Registry

Every agent instance registers with the cloud on startup and maintains a heartbeat:

```rust
pub struct AgentRegistration {
    pub agent_id: String,           // Unique per-instance (UUID)
    pub agent_type: AgentType,      // User, System, Orchestrator
    pub capabilities: Vec<String>,  // Tool names this agent can execute
    pub edge_node: String,          // Machine identifier
    pub status: AgentStatus,        // Active, Idle, Draining, Dead
    pub last_heartbeat: u64,        // Epoch seconds
    pub lease_ttl_secs: u32,        // How long before considered dead (default: 60)
    pub max_concurrent_tasks: u8,   // Capacity declaration
    pub current_task_count: u8,     // Current load
}

pub enum AgentType {
    User,         // Domain-specific coding agent
    System,       // Platform maintenance (regression, audit, tuning)
    Orchestrator, // Coordinates other agents
}

pub enum AgentStatus {
    Active,    // Processing tasks, heartbeating
    Idle,      // Alive but no active tasks
    Draining,  // Finishing current tasks, not accepting new ones
    Dead,      // Heartbeat expired (auto-set by cloud)
}
```

**Heartbeat protocol**: Agent sends `POST /agents/{agent_id}/heartbeat` every 30s. Cloud marks as Dead if no heartbeat for `lease_ttl_secs`. Dead agent's tasks become reclaimable.

### 7.2 Coordination Patterns

Three patterns, matching existing design docs but with concrete implementation:

#### Pattern 1: Fan-Out / Fan-In (Parallel)
```
Orchestrator identifies N independent subtasks
  ├── POST /tasks/{task_1}/lease → Agent A claims task_1
  ├── POST /tasks/{task_2}/lease → Agent B claims task_2
  └── POST /tasks/{task_3}/lease → Agent C claims task_3
  
All agents execute concurrently, sync results to cloud
  
Orchestrator polls or receives webhook:
  └── POST /plans/{plan_id}/merge → Combine results, detect conflicts
```

#### Pattern 2: Pipeline (Sequential)
```
Agent A completes subtask_1
  └── event: subtask_completed(subtask_1, output_artifact)
  
Cloud triggers next agent:
  └── POST /tasks/{task_2}/assign → Agent B starts with A's output as context
  
Agent B completes subtask_2
  └── event: subtask_completed(subtask_2, output_artifact)
  └── ...
```

#### Pattern 3: Adversarial Review (Iterative)
```
Agent A: Propose solution
  └── event: proposal_submitted(solution)

Agent B: Review proposal
  └── event: review_completed(approved | revision_needed, feedback)

If revision_needed:
  Agent A receives feedback, produces revised solution
  Loop until approved or max_iterations reached
```

### 7.3 Communication Model

Agents do **not** communicate directly. All communication is through cloud-persisted events:

```
Agent A ──event──▶ agent_events table ──query──▶ Agent B
                         │
                    causal_chain_id links all related events
```

**Why event-mediated, not direct**: 
- Full audit trail for every inter-agent message
- Dead agents don't break the protocol (events persist)
- Replay-compatible (re-execute multi-agent workflows from events)
- No need for service discovery or network connectivity between agents

---

## 8. Task Leasing & Distributed Execution

### 8.1 The Problem

Current `TaskRecord` has `user_id` but no agent ownership. Two agents can read the same task and start working on it simultaneously, producing conflicting results.

### 8.2 Task Lease Model

```rust
pub struct TaskLease {
    pub task_id: String,
    pub agent_id: String,          // Who holds the lease
    pub leased_at: u64,            // Epoch seconds
    pub expires_at: u64,           // Lease expiry (leased_at + ttl)
    pub ttl_secs: u32,             // Default: 300 (5 minutes), renewable
    pub lease_version: u64,        // Monotonic, for CAS operations
    pub checkpoint: Option<String>, // Last known good state (JSON)
}
```

**Lease protocol**:
```
1. CLAIM:   Agent A → POST /tasks/{id}/lease
            Cloud: IF task.status == Pending AND no active lease
                   THEN create lease, return lease_version
                   ELSE return 409 Conflict

2. RENEW:   Agent A → PUT /tasks/{id}/lease?version={v}
            Cloud: IF lease.agent_id == A AND lease.version == v
                   THEN extend expires_at, increment version
                   ELSE return 409 (lease stolen or expired)

3. RELEASE: Agent A → DELETE /tasks/{id}/lease
            Cloud: Mark task as completed/failed + remove lease

4. EXPIRE:  Cloud background job (every 30s):
            FOR each lease WHERE expires_at < NOW():
                Mark task as Pending (reclaimable)
                Log lease_expired event
```

**Key design decision**: Leases use optimistic locking (`lease_version` CAS), not distributed locks. This avoids deadlocks and works with MatrixOne's HTAP model.

### 8.3 Checkpoint-Based Resumption

When a lease expires (agent died), the next agent can resume from the checkpoint:

```rust
pub struct TaskCheckpoint {
    pub active_subtask_id: Option<String>,
    pub turn: u32,
    pub session_id: Option<String>,
    pub state: serde_json::Map<String, serde_json::Value>,  // Arbitrary state
    pub tools_executed: Vec<String>,                          // For dedup
    pub artifacts_produced: Vec<String>,                      // File paths
}
```

The resuming agent:
1. Claims the task via lease protocol
2. Loads checkpoint state
3. Validates artifacts (do the files still exist? are they consistent?)
4. Continues from `active_subtask_id` rather than restarting

### 8.4 Distributed Plan Execution

```rust
pub struct DistributedPlan {
    pub plan_id: String,
    pub goal: String,
    pub subtasks: Vec<SubtaskPlan>,
    pub coordination: CoordinationPattern,  // FanOut, Pipeline, Adversarial
    pub constraints: PlanConstraints,
}

pub struct PlanConstraints {
    pub max_total_turns: u32,      // Across all agents
    pub max_cost_budget_usd: f64,  // Total LLM cost cap
    pub timeout_secs: u64,         // Wall-clock deadline
    pub max_retries_per_subtask: u8,
}

pub struct SubtaskPlan {
    pub id: String,
    pub title: String,
    pub depends_on: Vec<String>,
    pub assigned_agent: Option<String>,  // None = unassigned, available for claim
    pub status: TaskStatus,
    pub effort: Option<String>,          // "small", "medium", "large"
    pub files: Vec<String>,              // Scope: which files this subtask may touch
    pub acceptance: Option<String>,      // Acceptance criteria
}
```

**Plan execution state machine**:
```
Plan Created
    │
    ▼
┌──────────────────────────────────────────────┐
│ For each subtask where depends_on all Done:  │
│   IF unassigned → post to task queue         │
│   Agent claims via lease → executes          │
│   On completion → mark Done, check next      │
│   On failure → retry up to max_retries       │
│                → if exhausted, mark plan Failed│
└──────────────────────────────────────────────┘
    │
    ▼
All subtasks Done → Plan Completed
```

---

## 9. Learning Convergence for Multi-Agent

### 9.1 The Challenge

Current learning sync assumes **single writer per user per profile**. With multiple agents:
- Agent A observes entity "React" used with tool `read_file` (confidence: 0.8)
- Agent B observes entity "React" used with tool `grep` (confidence: 0.6)
- Both push deltas to cloud simultaneously

### 9.2 Merge Strategies (Already Designed, Need Implementation)

| Data Type | Merge Strategy | Rationale |
|-----------|---------------|-----------|
| **EntityGraph** | Union merge, observation-count-wins | More observations = higher confidence |
| **PatternLibrary** | Union merge (combine patterns) | Different agents see different patterns |
| **Calibrator** | Weighted average by observation count | More data = more reliable threshold |
| **ToolQuality** | Weighted merge by invocation count | More invocations = better signal |
| **Preferences** | Last-writer-wins (timestamp) | User intent is singular |

### 9.3 Multi-Agent Learning Protocol

```
Agent A (edge):
  1. Pull learning snapshot at session start (version V)
  2. Accumulate local observations during session
  3. At session end, export delta (since V)
  4. Push delta with expected_version=V

Cloud (on push):
  IF current_version == expected_version:
    Apply delta, increment version → V+1
  ELSE (conflict: another agent pushed first):
    1. Compute 3-way merge: base(V) + delta_A + delta_B
    2. For entities: keep higher observation_count
    3. For patterns: union of both pattern sets
    4. For calibration: weighted average
    5. Store merged result as V+2
    6. Return merged snapshot to Agent A for local update
```

### 9.4 Confidence Gate for Learned Context

Already implemented in `tool_selector.rs`:

```rust
const MIN_LEARNED_ENTITY_CONFIDENCE: f64 = 0.30;

// Entity hints with decayed_confidence < 0.30 are filtered out
// This prevents stale cross-agent observations from polluting selection
```

This gate becomes more important with multi-agent: observations from other agents may be less relevant to the current agent's context. The confidence decay mechanism naturally handles this — observations not reinforced by the current agent will decay below the gate threshold.

---

## 10. Token Efficiency at Scale

### 10.1 Current Token Budget

```
System prompt:     ~2,000 tokens (identity, constraints, capabilities)
Learned context:   ~250-290 tokens (max 3 entity + 2 pattern + 2 calibration + 2 tool hints)
Tool catalog:      ~1,500 tokens (55 tools, compact descriptions)
Conversation:      Variable (elastic zone)
Memory injection:  ~100-2,400 tokens (intent-driven loading)
────────────────────────────────────────────
Total overhead:    ~3,750-6,190 tokens (before conversation)
```

### 10.2 Optimization Strategies

#### Strategy 1: Intent-Driven Memory Loading (Implemented)
```
Preference queries → profile only (~100 tok)
Commands          → procedural hints (~400 tok)
Feedback          → episodic last-2 (~600 tok)
Questions         → full memory (~2,400 tok)
```
**Result**: 60% average token savings vs always-load-everything.

#### Strategy 2: Learned Context Bounds (Implemented)
```
Max 3 entity hints, 2 pattern hints, 2 calibration hints, 2 tool hints
Worst case: ~290 tokens (~2-6% overhead)
Zero cost when no learning has occurred
```

#### Strategy 3: Prompt Cache Amplification (Implemented)
```
bridge_inprocess.rs caches system prompt by (profile_desc + tool_catalog) hash
Cache hit → skip ~3,500 token re-generation
32-entry LRU cache with full clear on overflow
```

#### Strategy 4: Parallel Tool Selection (Implemented)
```
PARALLEL_SAFE_TOOLS (34 tools): read_file, grep, glob, git_*, code_intel_*
Detected automatically → join_all pre-execution
Mutating tools (bash, write_file, git_commit): sequential fallback
```
**Result**: 2-5x faster multi-tool turns.

#### Strategy 5: Progressive Context Budgeting (Target)
```
High confidence (>90%):  Small context (~40K tokens)
  └── Core files only, no test fixtures, compressed history

Medium confidence (60-90%): Balanced (~80K tokens)
  └── Core + related files, some test code

Low confidence (<60%): Full context (~150K tokens)
  └── Everything including comments/docs
```

### 10.3 Multi-Agent Token Efficiency

When running N agents, total token cost scales as O(N) naively. Optimizations:

1. **Shared prompt prefix**: All agents in a plan share the same system prompt prefix → provider-side KV cache reuse
2. **Result caching**: Tool results cached by content hash. If Agent B needs the same file Agent A already read, serve from cache
3. **Incremental context**: Each agent only gets the delta from previous agent's output, not the full history
4. **Selective broadcast**: Only route events to agents whose task `depends_on` the producing agent's task

---

## 11. Observability & Trust

### 11.1 Decision Audit Trail

Every decision is reconstructable:
```
decision_id → {
    prompt@version,
    skill@version,
    context@snapshot (tool_results, conversation_history),
    memory@state (entity_graph, patterns, calibration),
    llm_params (model, temperature, max_tokens),
    tool_selection (strategy, confidence, alternatives_considered),
    output,
    user_feedback (if any)
}
```

### 11.2 Multi-Agent Observability

```
causal_chain_id links all events in a multi-agent workflow:

[plan_created] ──▶ [task_assigned(A)] ──▶ [tool_call(A, bash)] ──▶ [task_completed(A)]
                   [task_assigned(B)] ──▶ [tool_call(B, grep)] ──▶ [task_completed(B)]
                                                                          │
                                                                    [plan_merge]
                                                                          │
                                                                    [plan_completed]
```

All events carry:
- `event_id` (UUID, idempotent)
- `session_id` + `agent_id` (who)
- `causal_chain_id` (why — links to originating plan)
- `parent_event_id` (direct causality)
- `created_at` (DATETIME(6) — microsecond precision)

### 11.3 Health Metrics

| Metric | Source | Alert Threshold |
|--------|--------|-----------------|
| Agent heartbeat age | agent_registry | > lease_ttl_secs |
| Task lease expiry rate | task_leases | > 5% in 1 hour |
| Sync conflict rate | session_sync_log | > 10% of pushes |
| Event ingestion lag | IngestionStats | pending > 1000 |
| Tool error rate (per-tool) | ToolHealthTracker | > 30% in 10 invocations |
| Stall detection rate | TurnGuard | > 2 stalls per session |
| Learning drift | PatternLibrary | moving_avg delta > 0.3 |

---

## 12. Migration Path

### Phase 1: Complete Single-Agent Foundation

**Goal**: Fill stub implementations, make all 5 sync domains operational.

| Task | Effort | Dependency |
|------|--------|------------|
| Wire EventAdapter to IngestionSender | Small | None |
| Implement TaskAdapter with lease protocol | Medium | New `task_leases` table |
| Implement TemplateAdapter (read-only pull) | Small | None |
| Implement PreferenceAdapter (bidirectional) | Small | None |
| Fix unreachable code in FallbackSelector:962-972 | Small | None |

### Phase 2: Agent Registry & Task Leasing

**Goal**: Multiple agents can safely claim and execute tasks without conflicts.

| Task | Effort | Dependency |
|------|--------|------------|
| Create `agent_registry` table + heartbeat endpoint | Medium | Phase 1 |
| Create `task_leases` table + lease protocol endpoints | Medium | Phase 1 |
| Add `agent_id` to TaskRecord ownership model | Small | agent_registry |
| Background lease expiry worker | Small | task_leases |
| Lease-aware TaskAdapter sync | Medium | task_leases |

### Phase 3: Distributed Plan Execution

**Goal**: Plans can be decomposed and executed across multiple agents.

| Task | Effort | Dependency |
|------|--------|------------|
| DistributedPlan struct + PlanConstraints | Small | Phase 2 |
| Plan executor: dependency-ordered subtask dispatch | Large | Phase 2 |
| Fan-out coordination (parallel subtask execution) | Medium | Plan executor |
| Pipeline coordination (sequential with handoff) | Medium | Plan executor |
| Plan merge logic (combine subtask results) | Medium | Fan-out/Pipeline |
| Cost budget enforcement across agents | Small | Plan executor |

### Phase 4: Multi-Agent Learning Convergence

**Goal**: Learning from multiple agents converges correctly without data loss.

| Task | Effort | Dependency |
|------|--------|------------|
| 3-way merge for EntityGraph conflicts | Medium | Phase 2 |
| Weighted average merge for Calibrator | Small | 3-way merge |
| Union merge for PatternLibrary | Small | 3-way merge |
| Cross-agent confidence decay tuning | Medium | All merges |
| Integration tests: 2-agent concurrent learning | Large | All merges |

### Phase 5: Durable Long-Running Tasks

**Goal**: Tasks can span hours/days with pause/resume across agent lifetimes.

| Task | Effort | Dependency |
|------|--------|------------|
| Wire RunEngine into ChatLoop | Large | Phase 2 |
| AsyncToolRegistry for job submission | Medium | RunEngine |
| Event-based resumption triggers | Medium | RunEngine |
| Webhook integration for external events | Medium | Triggers |
| Multi-day workflow tests | Large | All above |

---

## Appendix A: Industry Comparison Matrix

| Capability | Codex | Claude Code | Cursor | Devin | LangGraph | CrewAI | **mo-agent (current)** | **mo-agent (target)** |
|-----------|-------|-------------|--------|-------|-----------|--------|----------------------|---------------------|
| Execution model | Cloud container | Local CLI | IDE embedded | Docker isolated | Framework | Framework | Edge-cloud split | Edge-cloud split |
| Session persistence | None | Local SQLite | IDE state | Git snapshots | Checkpoints | None default | JSONL + MatrixOne | JSONL + MatrixOne |
| Cross-session learning | ❌ | ❌ | ❌ | ❌ | ❌ | Optional | ✅ EntityGraph | ✅ Multi-agent merge |
| Multi-agent | ❌ | ❌ | ❌ | ❌ | ✅ Shared state | ✅ Role-based | ⚠️ Basic delegation | ✅ Leased tasks |
| Durable tasks | ❌ | Session resume | ❌ | ✅ Container | ✅ Checkpoints | ❌ | ⚠️ Record only | ✅ Event-sourced |
| Tool count | ~4 fixed | ~10 | LSP-based | Shell + browser | Pluggable | Pluggable | 55 (production) | 55+ |
| Code intelligence | Trained model | None native | AST + LSP | Shell grep | None | None | 10 tree-sitter tools | 10+ |
| Token efficiency | ~60% | ~40% (200K window) | ~95% (symbol) | ~50% | Per-thread | Full replay | ~90% (intent-driven) | ~95% (progressive) |
| Self-correction | Basic repair | None | LSP validation | Reflection loop | Node retry | Callback | StallGuard + ErrorRecovery | + Drift detection |
| Privacy (local-first) | ❌ Cloud only | ✅ | ✅ | ⚠️ Container | Configurable | Configurable | ✅ | ✅ |
| Offline support | ❌ | ⚠️ Needs API | ❌ | ❌ | Local model | Local model | ⚠️ Journal works | ✅ Queue + sync |

---

## Appendix B: Key File References

| Area | File | Key Lines |
|------|------|-----------|
| Sync engine | `services/src/sync_engine.rs` | DomainAdapter trait, SyncOrchestrator, SyncEnvelope |
| Learning sync | `services/src/state_sync.rs` | StateSyncService, optimistic locking, gzip |
| Event ingestion | `services/src/event_ingestion.rs` | IngestionWorker, batch flush, idempotency |
| Session restore | `services/src/session_restore.rs` | HybridRestoreService, RestoredSession |
| Session journal | `services/src/session_journal.rs` | JournalWriter, append-only JSONL |
| Task orchestrator | `services/src/task_orchestrator.rs` | TaskRecord, TaskCheckpoint, SubtaskPlan |
| Sync adapters | `mo-agent/src/mo_agent/sync_adapters.rs` | LearningAdapter (prod), EventAdapter (stub), TaskAdapter (stub) |
| Tool selector | `runtime/src/tool_selector.rs` | LearnedContext, FallbackSelector, confidence gate |
| Bridge | `runtime/src/turn/bridge_inprocess.rs` | Prompt cache, learned context injection |
| Chat stream | `mo-agent/src/mo_agent/chat_stream.rs` | Edge orchestration loop, parallel tool execution |
| Entity graph | `runtime/src/pipeline/entity.rs` | EntityKnowledge, decayed_confidence |
| Pattern library | `runtime/src/pipeline/pattern.rs` | ToolChainPattern, drift detection |
| Calibrator | `runtime/src/pipeline/calibration.rs` | ProgressiveCalibrator, 3-axis thresholds |
| Stall detection | `runtime/src/turn/stall.rs` | TurnGuard, intent drift, name stall |
| Error recovery | `runtime/src/turn/error_recovery.rs` | ErrorCategory, escalation thresholds |
| Code intelligence | `mo-agent/src/edge_tools/code_intel.rs` | 10 AST tools, tree-sitter, PARSER_CACHE |
| Git (pure Rust) | `mo-agent/src/edge_tools/git_gix.rs` | 8 git tools via gix, no binary dependency |

---

## Appendix C: Storage Schema (Current + Proposed)

### Current Tables (MatrixOne)
```sql
agent_sessions    -- PK: session_id, FK: user_id, indexes: user_status_updated
agent_events      -- PK: event_id, indexes: session_created, causal_chain_id
learning_snapshots-- PK: snapshot_id, UNIQUE: (user_id, profile_name), versioned
user_preferences  -- PK: pref_id, UNIQUE: (user_id, pref_key)
agent_tasks       -- PK: task_id, indexes: user_status_updated, parent_updated
plan_templates    -- PK: template_id, indexes: user_goal_project
session_checkpoints-- PK: checkpoint_id, UNIQUE: (session_id, number)
session_sync_log  -- PK: sync_id, indexes: user_session_created
```

### Proposed New Tables
```sql
-- Agent registry for multi-agent coordination
CREATE TABLE agent_registry (
    agent_id       VARCHAR(36) PRIMARY KEY,
    agent_type     VARCHAR(20) NOT NULL,      -- 'user', 'system', 'orchestrator'
    user_id        VARCHAR(36) NOT NULL,
    edge_node      VARCHAR(128),
    capabilities   JSON,                       -- tool names array
    status         VARCHAR(20) DEFAULT 'active',
    max_concurrent INTEGER DEFAULT 1,
    current_load   INTEGER DEFAULT 0,
    last_heartbeat DATETIME(6),
    created_at     DATETIME(6) DEFAULT NOW(6),
    INDEX idx_user_status (user_id, status),
    INDEX idx_heartbeat (last_heartbeat)
);

-- Task leases for distributed ownership
CREATE TABLE task_leases (
    task_id        VARCHAR(36) PRIMARY KEY,
    agent_id       VARCHAR(36) NOT NULL,
    lease_version  BIGINT NOT NULL DEFAULT 1,
    leased_at      DATETIME(6) NOT NULL,
    expires_at     DATETIME(6) NOT NULL,
    checkpoint     LONGTEXT,                   -- JSON checkpoint state
    FOREIGN KEY (task_id) REFERENCES agent_tasks(task_id),
    FOREIGN KEY (agent_id) REFERENCES agent_registry(agent_id),
    INDEX idx_expires (expires_at),
    INDEX idx_agent (agent_id)
);
```

---

*This document is the source of truth for multi-agent cloud runtime architecture. It supersedes aspirational descriptions in individual design docs where they conflict with the implementation-grounded analysis here.*
