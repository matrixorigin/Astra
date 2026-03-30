# Multi-Agent Cloud Runtime Architecture

> **Status**: Living Design Document  
> **Version**: 1.1 (post-review revision)  
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
7. [MatrixOne-Native Acceleration](#7-matrixone-native-acceleration)
8. [Multi-Agent Coordination Protocol](#8-multi-agent-coordination-protocol)
9. [Task Leasing & Distributed Execution](#9-task-leasing--distributed-execution)
10. [Learning Convergence for Multi-Agent](#10-learning-convergence-for-multi-agent)
11. [Token Efficiency at Scale](#11-token-efficiency-at-scale)
12. [Observability & Trust](#12-observability--trust)
13. [Agent Safety & Isolation](#13-agent-safety--isolation)
14. [External Interop: A2A & MCP](#14-external-interop-a2a--mcp)
15. [Migration Path](#15-migration-path)
16. [Appendix: Industry Comparison Matrix](#appendix-a-industry-comparison-matrix)

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
- **MatrixOne as the agent brain** — other runtimes store state in a database; mo-agent *thinks* in its database. HTAP enables transactional state (leases, sessions) AND analytical workloads (learning convergence, drift detection, cost forecasting) without ETL
- **Cross-session learning** (no competitor does this — Codex, Claude Code, Cursor, Devin all start fresh)
- **Edge-cloud split execution** (tools local for interactive coding; cloud sandbox for background tasks — combines Claude Code's privacy with Codex's durability)
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

## 7. MatrixOne-Native Acceleration

> *"Other agent runtimes store state in a database. mo-agent thinks in its database."*

MatrixOne is not just a storage backend — it is the **computational backbone** of the agent runtime. As a cloud-native HTAP database with unique features (Git4Data, Stage, Vector, Fulltext, Pub/Sub, Snapshot, PITR, native multi-tenancy), MatrixOne enables capabilities that **no other agent runtime can replicate** using PostgreSQL, MySQL, or SQLite.

### 7.1 Git4Data: Data Branching for Agent Experiments

MatrixOne's Git4Data provides Git-like version control for data — branching, merging, diffing at the table/database level. This is **unique to MatrixOne** (no equivalent in PostgreSQL/MySQL).

**Use case: Multi-agent plan execution with safe rollback**

```sql
-- Before a multi-agent plan executes, snapshot the current state
CREATE SNAPSHOT plan_baseline_sp FOR DATABASE agent_workspace;

-- Each agent works on its own data branch
DATA BRANCH CREATE DATABASE agent_a_workspace FROM agent_workspace {snapshot="plan_baseline_sp"};
DATA BRANCH CREATE DATABASE agent_b_workspace FROM agent_workspace {snapshot="plan_baseline_sp"};

-- Agents execute independently in their branches...

-- After execution, diff to see what changed
DATA BRANCH DIFF agent_a_workspace.learning_observations {snapshot="after_a"}
  AGAINST agent_workspace.learning_observations {snapshot="plan_baseline_sp"};

-- Merge agent results back to main workspace
DATA BRANCH MERGE agent_a_workspace.results INTO agent_workspace.results;
DATA BRANCH MERGE agent_b_workspace.results INTO agent_workspace.results;

-- If anything goes wrong, restore from snapshot
RESTORE ACCOUNT FROM SNAPSHOT plan_baseline_sp;
```

**Why this matters**: Agents can experiment on data branches without risk. Merge conflicts are resolved at the database level with three-way merge (LCA detection), not in application Rust code. **Cherry-pick semantics** (coming soon) will enable selective merge — pick specific rows/changes from a branch, not just full merge. No other agent runtime has this capability.

### 7.2 Vector Search: Native Embedding-Based Memory Retrieval

MatrixOne has **native vector data type** (`vecf32`, `vecf64`) with IVFFlat index support — no external extension needed.

> **Note**: Use IVFFlat indexes for vector search. HNSW exists but IVFFlat is the recommended production index type.

```sql
-- Store agent memory with embeddings
CREATE TABLE agent_memories (
    memory_id     VARCHAR(36) PRIMARY KEY,
    user_id       VARCHAR(36) NOT NULL,
    content       TEXT NOT NULL,
    embedding     vecf32(1536),                        -- OpenAI embedding dimension
    memory_type   VARCHAR(20),                         -- episodic, semantic, procedural
    confidence    FLOAT DEFAULT 1.0,
    created_at    DATETIME(6) DEFAULT NOW(6),
    INDEX idx_user (user_id),
    FULLTEXT INDEX ft_content (content)                -- BM25 fulltext for hybrid retrieval
);

-- Create IVFFlat index for approximate nearest neighbor search
CREATE INDEX idx_vec USING ivfflat ON agent_memories(embedding) lists=100 op_type "vector_l2_ops";

-- Semantic memory retrieval via IVFFlat index
SELECT memory_id, content, confidence,
       L2_DISTANCE(embedding, @query_embedding) AS distance
FROM agent_memories
WHERE user_id = @user_id
  AND confidence > 0.30
ORDER BY L2_DISTANCE(embedding, @query_embedding) ASC
LIMIT 10;

-- Hybrid retrieval: IVFFlat vector similarity + BM25 fulltext in one query
-- This is the key advantage: no Pinecone + Elasticsearch + fusion layer
SELECT m.memory_id, m.content,
       L2_DISTANCE(m.embedding, @query_embedding) AS semantic_dist,
       MATCH(m.content) AGAINST(@keywords IN NATURAL LANGUAGE MODE) AS text_score
FROM agent_memories m
WHERE m.user_id = @user_id
ORDER BY (0.7 * (1.0 / (1.0 + semantic_dist)) + 0.3 * text_score) DESC
LIMIT 10;
```

**Why this matters**: Memory retrieval is the #1 latency bottleneck in agent runtimes. MatrixOne handles IVFFlat vector similarity AND BM25 ranking **in a single query**, eliminating the Pinecone + Elasticsearch + application-level fusion pattern used by everyone else.

### 7.3 Fulltext Search with BM25: Document-Scale Knowledge Base

MatrixOne's fulltext supports **multiple ranking algorithms** (TF-IDF, BM25) and can index **external files via DataLink**.

```sql
-- Agent knowledge base with external document indexing
CREATE TABLE agent_knowledge (
    doc_id        VARCHAR(36) PRIMARY KEY,
    title         TEXT,
    content       LONGTEXT,
    source_file   DATALINK,                            -- Reference to external file
    FULLTEXT INDEX ft_content (title, content),
    FULLTEXT INDEX ft_file (source_file)               -- Index external file content!
);

-- Insert with DataLink to external documentation
INSERT INTO agent_knowledge VALUES
    ('d1', 'API Guide', NULL, 'stage://docs/api-guide.md'),
    ('d2', 'Setup Guide', NULL, 'stage://docs/setup.md');

-- BM25-ranked search across both inline content and external files
SET ft_relevancy_algorithm = "BM25";
SELECT doc_id, title,
       MATCH(title, content) AGAINST('authentication token refresh' IN NATURAL LANGUAGE MODE) AS relevance
FROM agent_knowledge
WHERE MATCH(title, content) AGAINST('authentication token refresh')
ORDER BY relevance DESC LIMIT 5;

-- Boolean mode for precise queries
SELECT doc_id FROM agent_knowledge
WHERE MATCH(title, content) AGAINST('+authentication +token -deprecated' IN BOOLEAN MODE);
```

**Why this matters**: Agents need to search large codebases and documentation. MatrixOne's DataLink enables indexing **external files on S3/Stage without loading them into the database**, providing Elasticsearch-grade search without a separate system.

### 7.4 Publication/Subscription: Cross-Agent Data Sharing

> **Reality check**: MatrixOne pub/sub is **database-level replication**, not event streaming (Kafka/Redis). A publisher shares entire databases or specific tables with subscriber accounts. Subscribers get a **read-only, real-time view** of published data — changes are synchronized transactionally, but this is data sharing, not an event bus.

```sql
-- Orchestrator publishes shared plan/context data for all agents
-- @session: sys (orchestrator account)
CREATE PUBLICATION shared_plan_data DATABASE orchestrator_db
    TABLE plan_definitions, task_assignments, shared_context
    ACCOUNT agent_coder_acct, agent_reviewer_acct
    COMMENT 'Shared coordination data for multi-agent execution';

-- Agent subscribes to see orchestrator data in real-time
-- @session: agent_coder_acct
CREATE DATABASE coordination FROM sys PUBLICATION shared_plan_data;

-- Agent reads plan assignments (real-time view of orchestrator's tables)
SELECT * FROM coordination.task_assignments
WHERE assigned_agent = 'agent_coder' AND status = 'pending';

-- Agent reads shared context (updates from orchestrator visible immediately)
SELECT * FROM coordination.shared_context
WHERE plan_id = @current_plan;
```

**What pub/sub IS good for in mo-agent**:
- **Shared reference data**: Orchestrator publishes plan templates, task assignments, shared context. Agents subscribe and see updates in real-time without polling.
- **Learning knowledge sharing**: Publish converged learning state (entity graph, pattern library) so all agents benefit from each other's experience.
- **Cross-tenant data federation**: Platform-level shared resources (model configs, prompt templates) published to all customer accounts.

**What pub/sub is NOT**:
- ❌ Event streaming / message queue (use `INSERT` into event tables + poll for that)
- ❌ Bidirectional — subscribers cannot write back to publisher's tables
- ❌ Per-row event notification — it replicates entire table state, not individual events

```
Orchestrator writes plan → plan table updated → PUBLICATION
                                                     │
                                          ┌──────────┴──────────┐
                                          ▼                     ▼
                                   Agent A account         Agent B account
                                   (SUBSCRIPTION)          (SUBSCRIPTION)
                                   reads plan data         reads plan data
                                   (real-time view)        (real-time view)
```

**For event routing** (Agent A signals "task done" to Agent B), use a simpler pattern:
```sql
-- Event table in orchestrator DB (included in publication)
CREATE TABLE task_events (
    event_id   VARCHAR(36) PRIMARY KEY,
    task_id    VARCHAR(36),
    event_type VARCHAR(30),
    source     VARCHAR(36),
    payload    JSON,
    created_at DATETIME(6) DEFAULT NOW(6),
    INDEX idx_task (task_id, created_at)
);

-- Orchestrator writes event → agents see it via subscription view
-- Agents poll their subscription view with timestamp filter:
SELECT * FROM coordination.task_events
WHERE task_id = @my_task AND created_at > @last_seen
ORDER BY created_at;
```

**Why this matters**: Pub/sub eliminates the need for agents to connect directly to the orchestrator's database. Each agent reads from its own subscription — the database handles replication. Compared to polling the orchestrator's table directly, this provides **network isolation** (agents don't need orchestrator credentials) and **automatic data distribution**.

### 7.5 Snapshot + PITR: Agent Checkpoint & Time-Travel

MatrixOne's Snapshot and PITR provide **database-level checkpointing** that's far more powerful than application-level JSON checkpoints:

```sql
-- Create checkpoint before risky agent operation
CREATE SNAPSHOT checkpoint_turn_42 FOR DATABASE agent_workspace;

-- Configure automatic PITR for agent workspace (7-day retention)
CREATE PITR agent_pitr FOR DATABASE agent_workspace RANGE 7 'd';

-- If agent corrupts data, restore instantly
RESTORE DATABASE agent_workspace FROM SNAPSHOT checkpoint_turn_42;

-- Time-travel query: what did the learning state look like 2 hours ago?
-- (via PITR within retention window)
SELECT * FROM agent_workspace.learning_observations
    {snapshot = "checkpoint_turn_42"};
```

**Why this matters**: Current TaskCheckpoint is a JSON blob stored in a LONGTEXT column. MatrixOne's snapshots provide **zero-copy, instant database-level checkpoints** — faster, more complete, and atomic across all tables.

### 7.6 Stage: Cloud-Native Artifact Storage

MatrixOne's Stage provides unified access to external storage (S3, OSS, local filesystem):

```sql
-- Configure artifact storage for agent outputs
CREATE STAGE agent_artifacts URL = 's3://mo-agent-artifacts/'
    CREDENTIALS = {'AWS_KEY_ID'='...', 'AWS_SECRET_KEY'='...'};

-- Agent stores build artifacts
SELECT build_log INTO OUTFILE 'stage://agent_artifacts/session_123/build.log'
FROM agent_events WHERE session_id = 'session_123' AND event_type = 'build_output';

-- Agent loads context from external knowledge base
LOAD DATA INFILE 'stage://agent_artifacts/shared/codebase_index.parquet'
INTO TABLE codebase_symbols;

-- Nested stage for per-project organization
CREATE STAGE project_alpha_stage URL = 'stage://agent_artifacts/projects/alpha/';
```

**Why this matters**: Agents produce artifacts (build logs, code diffs, test results) that need durable storage. Stage eliminates application-level S3 client code — the database handles cloud storage natively.

### 7.7 Multi-Tenant Accounts: Native Agent Isolation

MatrixOne's account system provides **complete data isolation** per agent or per customer:

```sql
-- Create isolated account per customer (SaaS model)
CREATE ACCOUNT customer_acme ADMIN_NAME 'admin' IDENTIFIED BY '...';
CREATE ACCOUNT customer_beta ADMIN_NAME 'admin' IDENTIFIED BY '...';

-- Each customer's agents operate in complete isolation
-- @session: customer_acme
CREATE DATABASE workspace;
CREATE TABLE workspace.agent_sessions (...);
-- customer_beta cannot see customer_acme's data

-- Platform-level analytics across all tenants (sys account only)
-- @session: sys account
SELECT account_name,
       COUNT(DISTINCT session_id) AS active_sessions,
       SUM(token_usage_total) AS total_tokens
FROM system_metrics.agent_usage
GROUP BY account_name;

-- Share reference data via cluster tables
CREATE CLUSTER TABLE shared_plan_templates (...);
-- All accounts can read, only sys can write
```

**Why this matters**: PostgreSQL multi-tenancy requires row-level security policies (error-prone). MatrixOne provides **SQL-level account isolation** — agents in different accounts literally cannot see each other's tables.

### 7.8 Learning Convergence via HTAP Aggregation

Instead of implementing 3-way merge in Rust application code, push learning convergence **into MatrixOne**:

```sql
-- Cross-agent entity confidence convergence (HTAP: TP writes + AP aggregation)
CREATE VIEW learning_entity_convergence AS
SELECT entity_name, domain,
       SUM(observation_count) AS total_observations,
       SUM(observation_count * confidence) / SUM(observation_count) AS weighted_confidence,
       COUNT(DISTINCT agent_id) AS contributing_agents,
       MAX(last_observed) AS freshest_observation
FROM learning_observations
WHERE decayed_confidence > 0.30   -- confidence gate
GROUP BY entity_name, domain;

-- Tool selection drift detection via window functions
SELECT tool_name,
       AVG(success_rate) OVER (
           ORDER BY created_at ROWS BETWEEN 10 PRECEDING AND CURRENT ROW
       ) AS recent_avg,
       AVG(success_rate) OVER (
           ORDER BY created_at ROWS BETWEEN 100 PRECEDING AND 11 PRECEDING
       ) AS baseline_avg
FROM tool_invocation_metrics
WHERE user_id = ?
HAVING ABS(recent_avg - baseline_avg) > 0.3;

-- Time-windowed metrics aggregation for agent health
SELECT _wstart, _wend,
       COUNT(*) AS event_count,
       AVG(latency_ms) AS avg_latency,
       MAX(latency_ms) AS p99_latency
FROM agent_events
WHERE created_at > DATE_SUB(NOW(), INTERVAL 1 HOUR)
INTERVAL(created_at, 5, minute) FILL(value, 0);
```

### 7.9 Atomic Lease Operations (No Background Worker)

```sql
-- Atomic lease claim with implicit expiry: no background worker needed
UPDATE task_leases
SET agent_id = @claiming_agent,
    lease_version = lease_version + 1,
    leased_at = NOW(6),
    expires_at = DATE_ADD(NOW(6), INTERVAL @ttl_secs SECOND)
WHERE task_id = @target_task
  AND (expires_at < NOW(6)                    -- expired lease (auto-reclaim)
       OR agent_id = @claiming_agent);        -- own lease renewal

-- If UPDATE affected 0 rows → lease held by another active agent → 409 Conflict
```

### 7.10 MatrixOne Feature → Agent Subsystem Mapping

| MatrixOne Feature | Agent Subsystem | Replaces | Competitive Gap |
|-------------------|----------------|----------|-----------------|
| **Git4Data** (branch/merge/diff) | Multi-agent plan execution | Application-level conflict resolution | No equivalent in any database |
| **Vector** (vecf32 + IVFFlat) | Memory retrieval, RAG | External vector DB (Pinecone) | Native, no extension needed |
| **Fulltext** (BM25 + DataLink) | Knowledge base search | Elasticsearch + application code | Single-query hybrid search |
| **Pub/Sub** (publication) | Cross-agent data sharing | Direct DB access + credentials | Network-isolated replication |
| **Snapshot** | Agent checkpointing | JSON blob in LONGTEXT column | Instant, atomic, zero-copy |
| **PITR** | Session recovery, time-travel | Manual WAL management | Declarative retention policies |
| **Stage** | Artifact storage (S3/OSS) | Application S3 client | SQL-native cloud storage |
| **Multi-Tenant Accounts** | Customer/agent isolation | Row-level security policies | SQL-level account isolation |
| **Time Window** (INTERVAL/FILL) | Metrics aggregation | Application-level windowing | Native time-series syntax |
| **DataLink** | External file indexing | Load-then-index pipeline | Lazy-load, index-in-place |
| **HTAP** | Learning convergence + drift | Separate OLTP + OLAP systems | Single engine, no ETL |
| **Window Functions** | Drift detection, calibration | Rust moving average code | Database-powered analytics |

---

## 8. Multi-Agent Coordination Protocol

### 12.1 Agent Registry

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

### 12.2 Coordination Patterns

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

### 12.3 Communication Model

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

## 9. Task Leasing & Distributed Execution

### 12.1 The Problem

Current `TaskRecord` has `user_id` but no agent ownership. Two agents can read the same task and start working on it simultaneously, producing conflicting results.

### 12.2 Task Lease Model

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

### 9.3 Lease + Data Branch Lifecycle

Leases and Git4Data branches serve **complementary purposes**: leases manage task ownership (who works on what), branches manage data isolation (each agent works in its own database space). The combined flow:

```
1. CLAIM:    Agent A claims task via lease protocol
                │
2. BRANCH:   Orchestrator creates data branch for Agent A
             DATA BRANCH CREATE DATABASE agent_a_ws FROM workspace;
                │
3. EXECUTE:  Agent A works freely in agent_a_ws (no conflicts possible)
             Creates Snapshot checkpoints within its branch as needed
                │
4. COMPLETE: Agent A finishes → orchestrator reviews via DIFF
             DATA BRANCH DIFF agent_a_ws.results AGAINST workspace.results;
                │
5. MERGE:    DATA BRANCH MERGE agent_a_ws.results INTO workspace.results;
             (structural merge by DB, semantic conflicts resolved in Rust)
                │
6. RELEASE:  Agent A releases lease → branch cleaned up
             DROP DATABASE agent_a_ws;  -- or keep for audit
```

**Crash recovery** (lease expires):
```
1. Lease expires → task marked as reclaimable
2. Agent B claims task via lease
3. Agent B inspects abandoned branch (agent_a_ws still exists)
4. DATA BRANCH DIFF agent_a_ws AGAINST workspace → see partial work
5. Agent B decides: resume from branch (keep partial work) or restart
6. If restart: RESTORE DATABASE agent_a_ws FROM SNAPSHOT plan_baseline;
```

### 9.4 Checkpoint-Based Resumption

Checkpoints operate at **two levels**, each handling different concerns:

| Level | Mechanism | What It Captures | Use Case |
|-------|-----------|-----------------|----------|
| **Database** | `CREATE SNAPSHOT` | All table state, atomic, zero-copy | Rollback on failure, time-travel queries |
| **Domain** | `TaskCheckpoint` (Rust struct) | Subtask progress, tool dedup list, semantic state | Which subtask to resume, which tools already ran |

```rust
pub struct TaskCheckpoint {
    pub active_subtask_id: Option<String>,
    pub turn: u32,
    pub session_id: Option<String>,
    pub state: serde_json::Map<String, serde_json::Value>,  // Arbitrary state
    pub tools_executed: Vec<String>,                          // For dedup
    pub artifacts_produced: Vec<String>,                      // File paths
    pub snapshot_name: Option<String>,                        // Link to MO Snapshot
}
```

**Why both are needed**: A database Snapshot captures "what the data looks like" but not "what the agent was doing." TaskCheckpoint captures "agent was on subtask 3, already ran tests 1-5, next step is test 6." The `snapshot_name` field links the two: restore the Snapshot first (database state), then load TaskCheckpoint (domain state).

### 12.4 Distributed Plan Execution

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

## 10. Learning Convergence for Multi-Agent

### 12.1 The Challenge

Current learning sync assumes **single writer per user per profile**. With multiple agents:
- Agent A observes entity "React" used with tool `read_file` (confidence: 0.8)
- Agent B observes entity "React" used with tool `grep` (confidence: 0.6)
- Both push deltas to cloud simultaneously

### 12.2 Merge Strategies (Already Designed, Need Implementation)

| Data Type | Merge Strategy | Rationale |
|-----------|---------------|-----------|
| **EntityGraph** | Union merge, observation-count-wins | More observations = higher confidence |
| **PatternLibrary** | Union merge (combine patterns) | Different agents see different patterns |
| **Calibrator** | Weighted average by observation count | More data = more reliable threshold |
| **ToolQuality** | Weighted merge by invocation count | More invocations = better signal |
| **Preferences** | Last-writer-wins (timestamp) | User intent is singular |

### 10.3 Multi-Agent Learning Protocol

Learning merge operates at **two levels**: structural (database) and semantic (application).

```
Agent A (edge):
  1. Pull learning snapshot at session start (version V)
  2. Accumulate local observations in its data branch
  3. At session end, export delta (since V)
  4. Push delta with expected_version=V

Cloud (on push):
  IF current_version == expected_version:
    Apply delta directly, increment version → V+1
  ELSE (conflict: another agent pushed first):
    ┌─────────────────────────────────────────────────────────┐
    │  STEP 1: Structural merge (MatrixOne DATA BRANCH MERGE) │
    │  ──────────────────────────────────────────────────────  │
    │  DATA BRANCH DIFF agent_a.learning AGAINST cloud.learning│
    │  → Identifies conflicting rows (same entity, both changed)│
    │  → Non-conflicting rows merged automatically              │
    │                                                           │
    │  STEP 2: Semantic merge (Rust application logic)          │
    │  ──────────────────────────────────────────────────────  │
    │  For each conflicting entity:                             │
    │    EntityGraph → keep higher observation_count            │
    │    PatternLibrary → union of both pattern sets            │
    │    Calibrator → weighted average by observation count     │
    │    ToolQuality → weighted merge by invocation count       │
    │    Preferences → last-writer-wins (timestamp)             │
    │                                                           │
    │  Store merged result as V+2                               │
    │  Return merged snapshot to Agent A for local update       │
    └─────────────────────────────────────────────────────────┘
```

**Why both levels are needed**: `DATA BRANCH MERGE` handles row-level identity (detect which entities were modified by both agents). But it cannot decide that "higher observation_count wins" — that's a domain-specific rule. The database detects structural conflicts; Rust resolves semantic conflicts.

### 12.4 Confidence Gate for Learned Context

Already implemented in `tool_selector.rs`:

```rust
const MIN_LEARNED_ENTITY_CONFIDENCE: f64 = 0.30;

// Entity hints with decayed_confidence < 0.30 are filtered out
// This prevents stale cross-agent observations from polluting selection
```

This gate becomes more important with multi-agent: observations from other agents may be less relevant to the current agent's context. The confidence decay mechanism naturally handles this — observations not reinforced by the current agent will decay below the gate threshold.

---

## 11. Token Efficiency at Scale

### 12.1 Current Token Budget

```
System prompt:     ~2,000 tokens (identity, constraints, capabilities)
Learned context:   ~250-290 tokens (max 3 entity + 2 pattern + 2 calibration + 2 tool hints)
Tool catalog:      ~1,500 tokens (55 tools, compact descriptions)
Conversation:      Variable (elastic zone)
Memory injection:  ~100-2,400 tokens (intent-driven loading)
────────────────────────────────────────────
Total overhead:    ~3,750-6,190 tokens (before conversation)
```

### 12.2 Optimization Strategies

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

### 12.3 Multi-Agent Token Efficiency

When running N agents, total token cost scales as O(N) naively. Optimizations:

1. **Shared prompt prefix**: All agents in a plan share the same system prompt prefix → provider-side KV cache reuse
2. **Result caching**: Tool results cached by content hash. If Agent B needs the same file Agent A already read, serve from cache
3. **Incremental context**: Each agent only gets the delta from previous agent's output, not the full history
4. **Selective broadcast**: Only route events to agents whose task `depends_on` the producing agent's task

---

## 12. Observability & Trust

### 12.1 Decision Audit Trail

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

### 12.2 Multi-Agent Observability

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

### 12.3 Health Metrics

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

## 13. Agent Safety & Isolation

### 13.1 The Problem

When multiple agents execute on the same edge node, they share a filesystem. Agent A running `bash("npm install")` while Agent B runs `write_file("package.json")` creates race conditions and data corruption.

### 13.2 Isolation Model

Multi-agent isolation addresses **two concerns**: filesystem isolation (code/build artifacts) and data isolation (learning state, metrics, events).

| Concern | Edge (same machine) | Cloud (separate containers) |
|---------|--------------------|-----------------------------|
| **Filesystem** | `git branch` per agent + checkout | `git clone --branch` per container |
| **Data** | MatrixOne data branches (Git4Data) | MatrixOne multi-tenant accounts |

**Edge multi-agent** (lightweight, most common):
```
Agent A: git checkout -b agent-a-work → operates on its branch
Agent B: git checkout -b agent-b-work → operates on its branch
Merge:   git merge agent-a-work agent-b-work → standard git merge
```
No worktree needed — agents coordinate via branches and stash/checkout. Filesystem conflicts are rare when agents work on different subtasks with non-overlapping file scopes.

**Cloud multi-agent** (full isolation):
```
Container A: git clone --branch main → independent filesystem
Container B: git clone --branch main → independent filesystem
Each container also gets its own MO account for data isolation.
```

**Data isolation** is handled entirely by MatrixOne:
- Each agent works on its own **data branch** (Git4Data)
- Merge via `DATA BRANCH MERGE` (native 3-way merge with LCA)
- Coming soon: **cherry-pick semantics** — selectively merge specific changes, not full branch
- Accounts provide SQL-level namespace isolation (structurally impossible to access other agent's data)

For **cloud background agents**: container + MO account (both filesystem and data fully isolated).

### 13.3 Agent Permission Model

| Agent Type | bash | write_file | git_commit | network | delegate |
|-----------|------|-----------|-----------|---------|----------|
| **User** | ✅ (with user approval) | ✅ | ✅ | ✅ | ❌ (default) |
| **System** | ❌ (read-only tools) | ❌ | ❌ | ✅ (cloud APIs) | ❌ |
| **Orchestrator** | ❌ | ❌ | ❌ | ✅ | ✅ |

System agents (regression, audit, tuning) are **read-only** — they can analyze but not modify the codebase. This prevents a compromised system agent from corrupting user work.

### 13.4 Blast Radius Containment

If an agent fails catastrophically (infinite loop, disk fill, memory exhaustion):
1. **Resource limits**: Per-agent CPU/memory/disk caps (enforced by cgroup if containerized, or ulimit if worktree)
2. **Error budget**: ToolHealthTracker already caps errors per session. Extend to per-agent scope.
3. **Lease expiry**: Dead agent's lease expires, work is reclaimable by another agent
4. **Worktree cleanup**: Failed agent's worktree can be discarded without affecting other agents or main branch

---

## 14. External Interop: A2A & MCP

### 14.1 Why Interop Matters

The agent ecosystem is standardizing. Google's A2A (Agent-to-Agent) protocol and Anthropic's MCP (Model Context Protocol) are becoming interop standards. Being proprietary-only means:
- Cannot federate with external agents (customer's existing agent fleet)
- Cannot leverage the growing MCP tool ecosystem (1000+ MCP servers)
- Risk of ecosystem lock-out as standards solidify

### 14.2 MCP as Tool Extension Surface

mo-agent already supports MCP servers for tool discovery. Deepen this:

```
Edge Runtime
├── 55 built-in tools (edge_tools.rs)
├── MCP Server discovery (mcp_tools.rs)
│   ├── Local MCP servers (filesystem, git, database tools)
│   └── Remote MCP servers (cloud APIs, SaaS integrations)
└── Tool registry unifies all sources
```

**Key enhancement**: MCP tools should be first-class citizens in the ToolRegistry, eligible for learned context boosting, quality tracking, and selection optimization — not just pass-through wrappers.

### 14.3 A2A for Multi-Platform Agent Federation

When mo-agent needs to coordinate with agents on other platforms:

```
mo-agent Orchestrator
    │
    ├── mo-agent Worker A (native protocol)
    ├── mo-agent Worker B (native protocol)
    └── External Agent C (A2A protocol)
        ├── AgentCard discovery (/.well-known/agent.json)
        ├── Task creation (POST /tasks)
        └── Result streaming (SSE /tasks/{id}/events)
```

**Implementation strategy**: A2A adapter that translates between mo-agent's internal event model and A2A's task/message format. This is a **bridge**, not a replacement — internal coordination remains native for performance.

### 14.4 Interop Priority

| Standard | Priority | Rationale |
|----------|----------|-----------|
| **MCP tools** | ✅ Already supported | Extend to first-class registry integration |
| **MCP sampling** | 🟡 High | Let external tools request LLM completions through mo-agent |
| **A2A Agent Cards** | 🟡 High | Publish mo-agent capabilities for external discovery |
| **A2A Task protocol** | 🟢 Medium | Accept tasks from external orchestrators |
| **OpenAI Agents SDK** | 🟢 Low | Compatibility layer if demand emerges |

---

## 15. Migration Path

> **Note**: Phase ordering prioritizes **durable execution** (Phase 2) before multi-agent coordination (Phases 3-4), because multi-agent is useless without durable execution — agents that die when HTTP connections close cannot be coordinated.

### Phase 1: Complete Single-Agent Foundation

**Goal**: Fill stub implementations, make all 5 sync domains operational.

| Task | Effort | Dependency |
|------|--------|------------|
| Wire EventAdapter to IngestionSender | Small | None |
| Implement TaskAdapter with lease protocol | Medium | New `task_leases` table |
| Implement TemplateAdapter (read-only pull) | Small | None |
| Implement PreferenceAdapter (bidirectional) | Small | None |
| Fix unreachable code in FallbackSelector:962-972 | Small | None |

### Phase 2: Durable Long-Running Tasks

**Goal**: Tasks can span hours/days with pause/resume across agent lifetimes. This is prerequisite to multi-agent — agents that die when HTTP connections close cannot be coordinated.

| Task | Effort | Dependency |
|------|--------|------------|
| Wire RunEngine into ChatLoop | Large | Phase 1 |
| AsyncToolRegistry for job submission | Medium | RunEngine |
| Event-based resumption triggers | Medium | RunEngine |
| Cloud sandbox mode for background agents | Large | RunEngine |
| Webhook integration for external events | Medium | Triggers |

### Phase 3: Agent Registry & Task Leasing

**Goal**: Multiple agents can safely claim and execute tasks without conflicts.

| Task | Effort | Dependency |
|------|--------|------------|
| Create `agent_registry` table + heartbeat endpoint | Medium | Phase 2 |
| Create `task_leases` table + atomic CAS claim (MatrixOne-native) | Medium | Phase 2 |
| Add `agent_id` to TaskRecord ownership model | Small | agent_registry |
| Git worktree isolation for multi-agent edge | Medium | agent_registry |
| Lease-aware TaskAdapter sync | Medium | task_leases |

### Phase 4: Distributed Plan Execution

**Goal**: Plans can be decomposed and executed across multiple agents. Prioritize adversarial review (highest proven value for coding), then pipeline, then fan-out.

| Task | Effort | Dependency |
|------|--------|------------|
| DistributedPlan struct + PlanConstraints | Small | Phase 3 |
| **Phase 4a**: Adversarial review coordination (propose-review-revise) | Medium | Phase 3 |
| **Phase 4b**: Pipeline coordination (sequential with handoff) | Medium | Phase 4a |
| **Phase 4c**: Fan-out coordination (parallel with git-merge) | Large | Phase 4b |
| Plan merge logic (combine subtask results) | Medium | Phase 4b/4c |
| Cost budget enforcement across agents (MatrixOne AP query) | Small | Plan executor |

### Phase 5: Multi-Agent Learning Convergence

**Goal**: Learning from multiple agents converges correctly without data loss.

| Task | Effort | Dependency |
|------|--------|------------|
| MatrixOne materialized views for entity/pattern convergence | Medium | Phase 3 |
| Weighted aggregation for Calibrator (SQL, not Rust) | Small | Materialized views |
| Cross-agent confidence decay tuning | Medium | Aggregation |
| CDC-based event routing between agents | Large | Phase 3 |
| Integration tests: 2-agent concurrent learning | Large | All above |

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

*This document is the source of truth for multi-agent cloud runtime architecture. It supersedes aspirational descriptions in individual design docs where they conflict with the implementation-grounded analysis here. The design leverages MatrixOne's HTAP capabilities as the core competitive moat — not just as storage, but as the computational backbone for learning convergence, drift detection, and real-time observability.*
