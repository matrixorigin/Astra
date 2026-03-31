# Multi-Agent Cloud Runtime Architecture

> **Status**: Living Design Document  
> **Version**: 1.4.41 (Phase 0 partial: slice 47 — **`headless_tool_stderr_lines`**, **`skill_instructions_merge`**, **`chat_turn_api_error`**, **`chat_turn_step_plan`**)  
> **Scope**: Edge-cloud state management, multi-agent orchestration, and cloud-scale execution  
> **Audience**: Core contributors, architecture reviewers

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Industry Landscape & Competitive Position](#2-industry-landscape--competitive-position)
3. [Current Architecture: What We Have](#3-current-architecture-what-we-have)
4. [Gap Analysis: Design vs Implementation](#4-gap-analysis-design-vs-implementation)
5. [Target Architecture: Multi-Agent Cloud Runtime](#5-target-architecture-multi-agent-cloud-runtime)
   - 5.3 [Headless Cloud Runtime: Client-Runtime Decoupling](#53-headless-cloud-runtime-client-runtime-decoupling)
   - 5.4 [Responsibility Split: What Moves Where](#54-responsibility-split-what-moves-where)
   - 5.5 [Thin Client Protocol](#55-thin-client-protocol)
     - 5.5.1 [Reference implementation: `mo-thin-client`](#551-reference-implementation-mo-thin-client)
     - 5.5.2 [Lightweight edge executor](#552-lightweight-edge-executor)
     - 5.5.3 [CLI registry and heartbeat environment](#553-cli-registry-and-heartbeat-environment)
6. [Edge-Cloud State Model](#6-edge-cloud-state-model)
   - 6.2.1 [Runtime sync adapters (implemented)](#621-runtime-sync-adapters-implemented)
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

**Current state**: A mature single-agent, local-first system with production-ready learning sync, event ingestion, session management, and a 50-tool edge toolkit. The runtime implements a 5-stage cognitive pipeline (Perceive→Plan→Execute→Evaluate→Reflect) with progressive self-calibration.

**Target state**: A **headless cloud runtime** where:
- The server is the single source of truth — all cognitive capabilities exposed via API
- Thin clients (CLI, Web, IDE) share the same protocol (§5.5) and are stateless
- Edge executors run local tools (bash, fs, git) and return results via callback
- Multiple agents coordinate on complex tasks via structured protocols
- Tasks are leased, checkpointed, and resumable across agent failures
- Learning converges across agents via conflict-free merge strategies

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
├── mo-agent/        # Edge: CLI REPL, 50 tools, thin-client SSE loop (still hosts chat_stream)
│   ├── main.rs              # Entry point, REPL loop, session management
│   ├── edge_tools.rs        # 50 tools: bash, file ops, git (gix), code intel, web, memory
│   ├── mo_agent/
│   │   ├── chat_stream/     # `mod.rs` + `sse_loop/` (`mod.rs` entry, `agentic_sse_loop`, `agentic_loop_turn`); headless §5.5
│   │   └── repl_turn.rs     # Single turn execution
│   └── edge_tools/
│       ├── code_intel.rs    # 10 tree-sitter AST tools
│       ├── git_gix.rs       # Pure-Rust git (no binary dependency)
│       └── build_test.rs    # Build-test loop with error delta tracking
│
├── runtime/         # Cognitive pipeline: tool selection, turn execution, learning, plan mode
│   ├── plan_decompose.rs    # Long-horizon planning, templates, execution preview (moved from CLI)
│   ├── sync_adapters.rs     # LearningAdapter, MatrixOneTransport, Event/Task adapters (moved from CLI; not in services — avoids runtime↔services cycle)
│   ├── tool_selector.rs     # LearnedContext, TfIdf/LLM/FallbackSelector
│   ├── tool_registry/       # 8 modules: registry, scoring, report, meta
│   ├── turn/                # 38 modules: bridge, stall detection, error recovery, health
│   │   ├── bridge_inprocess.rs  # In-process ChatTurnBridge; calls services for active LLM resolution
│   │   ├── tool_health.rs       # Session-scoped error budgets, deprioritization
│   │   └── stall.rs             # TurnGuard, intent drift detection
│   └── pipeline/            # 18 modules: cognitive engine
│       ├── engine.rs            # Perceive→Plan→Execute→Evaluate→Reflect
│       ├── entity.rs            # EntityGraph: entity→domain→tools knowledge
│       ├── pattern.rs           # PatternLibrary: tool chain patterns, drift detection
│       ├── calibration.rs       # ProgressiveCalibrator: 3-axis confidence thresholds
│       └── persistence.rs       # Local + cloud persistence for learning data
│
├── services/        # Cloud integration: sync, sessions, events, tasks, LLM row resolution
│   ├── models.rs            # resolve_active_llm_model + ResolvedActiveLlmModel (infra_llm_models + Fernet)
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
**Transport**: [`MatrixOneTransport`](../../rust/crates/runtime/src/sync_adapters.rs) for **Learning, Tasks, Templates, Preferences**; **Events** use the ingestion pipeline instead of orchestrator push/pull (see §6.2.1).

### 3.3 Production Readiness Assessment

| Component | Status | Evidence |
|-----------|--------|----------|
| Edge tool execution (50 tools) | ✅ Production | All tools implemented, no stubs |
| Session journal + workspace | ✅ Production | Append-only JSONL, version tracking |
| Event ingestion (async batch) | ✅ Production | Backpressure, idempotent, at-least-once |
| Learning sync (EntityGraph) | ✅ Production | Delta support, optimistic locking, gzip compression |
| Tool selection (FallbackSelector) | ✅ Production | TF-IDF + LLM hybrid, learned context reuse |
| Stall/error detection | ✅ Production | Intent drift, name stall, error budgets, circuit breaker |
| Code intelligence (tree-sitter) | ✅ Production | 10 AST tools, 8 languages, 44 tests |
| Git operations (gix) | ✅ Production | Pure-Rust, 46 tests, no binary dependency |
| Task orchestrator | ⚠️ Moderate | Checkpoint/resume works; Phase 3 adds **lease-backed** ownership for concurrent agents (not full RunEngine) |
| Sync adapters (all five domains) | ✅ Production | [`runtime::sync_adapters`](../../rust/crates/runtime/src/sync_adapters.rs): Events via ingestion side-channel; Tasks lease-filtered export; Templates pull-only; Preferences bidirectional |
| Multi-agent coordination | ⚠️ Partial | **Registry + task leases + `TaskAdapter`** shipped; fan-out / pipeline / adversarial patterns still design-stage (Phase 4) |
| Durable long-running tasks | ❌ Design only | AgentRun record exists; RunEngine not wired |

### 3.4 Architectural Debt: Layer Violations

Code-level audit reveals **5 components stuck in the wrong layer** that block multi-client and multi-agent progress:

| Component | Current Layer | Evidence | Should Be | Why It Matters |
|-----------|--------------|----------|-----------|---------------|
| **SyncOrchestrator** | `mo-agent` `ReplState` | `main.rs:436` | Cloud service state | CLI session holds cloud sync — can't share across Web/IDE clients |
| **MatrixOne pool** | `mo-agent` `ReplState` | `main.rs:406` | Service layer | DB connection pool is infrastructure, not CLI concern |
| **Event ingestion sender** | `mo-agent` `ReplState` | `main.rs:402` | Service layer | Cloud event publishing is not a CLI responsibility |
| **LearningAdapter bridge** | `runtime` `sync_adapters.rs` | Bridges `pipeline` learning modules with `services::SyncEngine` | ✅ **Done** (in `runtime`, not `services` — `services` must not depend on `runtime`) | CLI imports `mo_agent_runtime::sync_adapters::*` |
| **InProcessChatTurnBridge** | `runtime` `turn/bridge_inprocess.rs` | LLM call path | 🟡 **Partial** | Active-model **DB query + decrypt** moved to `services::resolve_active_llm_model`; bridge still holds `MatrixOneSettings` / pool wiring for streaming |

**Crate size confirms the imbalance** (lines of Rust code):

| Crate | LOC | Role | Assessment |
|-------|----:|------|-----------|
| `mo-agent` | ≈47k (Rust LOC, approximate) | CLI — should be thin | **Still heavy**: `chat_stream`, `ReplState` infra, edge tools; plan/sync adapters moved to `runtime` |
| `runtime` | 56,748 | Cognitive engine + HTTP API | Mostly correct; InProcessBridge is the outlier |
| `services` | 26,441 | Cloud backend | Clean; only depends on `core` |
| `core` | ~2,000 | Shared config/types | ✅ Correct |

**Dependency graph** (verified, acyclic):
```
mo-agent ──▶ runtime ──▶ services ──▶ core
   └──────────────────────▶ services
   └──────────────────────────────────▶ core
```

The graph is correct (no cycles), but `mo-agent` being the **only crate that can bridge runtime↔services** creates a bottleneck: any new client (Web, IDE) would need to duplicate the bridging code or depend on `mo-agent`.

**Two binary targets already exist** (good foundation):
- `mo-agent` (CLI): `crates/mo-agent/src/main.rs` — 5,198 lines, REPL + edge tools
- `mo-agent-server` (API): `crates/runtime/src/main.rs` — 11 lines, pure HTTP server

---

## 4. Gap Analysis: Design vs Implementation

### 4.1 Critical Gaps

| # | Gap | Design Doc | Implementation | Impact |
|---|-----|-----------|----------------|--------|
| G1 | **Durable agent runs** | durable-agent-runs.md: full RunEngine, AsyncToolRegistry, multi-day workflows | AgentRun record exists; RunEngine not wired to ChatLoop | Cannot run tasks spanning hours/days |
| G2 | **Multi-agent orchestration** | agents-and-orchestration.md: Fan-Out/Fan-In, Pipeline, Adversarial Review | Basic delegation skill exists; coordination patterns incomplete | Cannot run agent teams |
| G3 | **Task leasing & ownership** | §9 / Phase 3 | `task_leases` + `agent_tasks.agent_id`, HTTP lease APIs, [`DatabaseTaskLeaseService`](../../rust/crates/services/src/multi_agent.rs) (transaction + `FOR UPDATE`) | **Largely addressed**; remaining: git worktree isolation, richer orchestration UX |
| G4 | **Distributed plan execution** | plans/cloud-edge-redesign-v2.md: PlanState with CRDT merge | Plan events logged; no distributed executor | Plans can't span multiple agents |
| G5 | **EventAdapter vs sync engine** | sync_engine.rs: `DomainAdapter` | **Implemented**: [`EventAdapter`](../../rust/crates/runtime/src/sync_adapters.rs) uses dedicated ingestion (`export_delta` → `None`; `export_full` intentionally unsupported) | Events do not ride `SyncOrchestrator` push/pull — by design they use `IngestionSender` |
| G6 | **TaskAdapter sync** | sync_engine.rs: `DomainAdapter` | **Implemented**: [`TaskAdapter`](../../rust/crates/runtime/src/sync_adapters.rs) exports **dirty ∩ leased** tasks; [`push_tasks_pack_held_mysql`](../../rust/crates/services/src/multi_agent.rs) enforces holder on push | Remaining gap is product/UX (RunEngine, long-running tasks), not the adapter stub |
| G7 | **Cross-agent learning merge** | state_sync.rs: observation-count-wins merge | Single-writer assumption; no 3-way merge | Multiple agents writing creates conflicts |
| G8 | **Dual cognitive loops (Phase 0)** | One headless cloud runtime | `mo-agent` [`chat_stream/sse_loop/`](../../rust/crates/mo-agent/src/mo_agent/chat_stream/sse_loop/) **and** [`bridge_inprocess.rs`](../../rust/crates/runtime/src/turn/bridge_inprocess.rs) both run multi-turn LLM loops | Blocks a single thin-client story; duplicates stall/token/schema logic unless consolidated; **SSE** (39); **stall metrics** (40); **ingest** (41); **post-tool + stall preflight + verdict audit type** (42); **headless indices/parse/timeout/unknown-tool** (43); **reflect hydrate + headless postprocess/journal** (44); **budget pressure + headless step/cache/checkpoint/journal** (45); **explain lines + selection context + headless opening/pairs** (46); **headless stderr + skill merge + API error + step plan** (47) |

### 4.2 Design Docs That Outpace Implementation

| Document | Designed | Implemented |
|----------|----------|-------------|
| durable-agent-runs.md | 100% | ~15% (records only) |
| multi-agent-delegation-guide.md | 100% | ~40% (basic delegation) |
| cloud-edge-redesign-v2.md | 100% | ~30% (learning sync only) |
| context-window-management.md | 100% | ~60% (basic budget, no zones) |
| evaluation-and-evolution.md | 100% | ~30% (metrics schema, no auto-gate) |

### 4.3 What's Solid and Should Not Change

- **Phase 0 progress (v1.4.41)**: **`sse_loop/`** is two implementation modules + `mod.rs` entry (`stream_chat_sse` → `AgenticSseLoopState` → `run_all_turns` / `agentic_loop_turn`). **`bridge_inprocess`** + **`ChatTurnSseFramer`** share **`SseBlankLineUtf8Buf`** (39). Stall metrics + verdict penalties in **`stall`** (40). **`agentic_turn_ingest`** (41). Post-tool policy + stall preflight in **`agentic_post_tool_policy`** / **`agentic_stall_preflight`** (42); **`AgenticVerdictAuditEvent`** in runtime, CLI **`VerdictEvent`** = type alias. Headless round indexing + flat parse + timeout/unknown-tool helpers in **`headless_tool_assembly`** (43). Reflect hydrate + headless postprocess/journal + semantic hint wrapper (44). Budget pressure + headless step/cache/checkpoint/journal + skill merge (45). Prepare-turn explain + selection context + headless opening/pairs (46). Headless stderr line builders + skill-instruction merge + HTTP error string + `record_plan` helper (47). Remaining: move the multi-turn loop body to a server handler; **`ReplState`** / **`AppState`** infra largely converged on **`MatrixCloudRuntime`**.
- **Local-first journal**: Append-only JSONL is the correct foundation. Fast, crash-safe, auditable.
- **Sync envelope state machine**: Clean→Dirty→Syncing→Conflict is correct. Extend, don't replace.
- **DomainAdapter trait**: The trait signature is well-designed. **Learning, Events, Tasks, Templates, and Preferences** now have real [`runtime::sync_adapters`](../../rust/crates/runtime/src/sync_adapters.rs) implementations (see §6.2.1); residual “stub” language in older sections is obsolete for those domains.
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

The target is a **three-tier architecture** where the cloud runtime is the brain, thin clients are the face, and edge executors are the hands:

```
┌──────────────────────────────────────────────────────────────────────────┐
│                         THIN CLIENTS                                     │
│  CLI (mo-agent) │ Web UI │ IDE Plugin │ SDK │ Webhook                    │
│                                                                          │
│  All speak the same Thin Client Protocol (§5.5):                         │
│  • SSE stream for chat turns                                             │
│  • REST for state CRUD (sessions, memory, plans, skills)                 │
│  • WebSocket for real-time collaboration                                 │
│  • Tool approval/rejection callbacks                                     │
│                                                                          │
│  Clients are STATELESS — no LLM config, no sync orchestration,           │
│  no learning state. They render, dispatch, and relay.                     │
└────────────────────┬─────────────────────────────────────────────────────┘
                     │ Thin Client Protocol (SSE + REST + WS)
┌────────────────────▼─────────────────────────────────────────────────────┐
│                    HEADLESS CLOUD RUNTIME                                 │
│                                                                          │
│  ┌────────────────────────────────────────────────────────────────┐      │
│  │  Control Plane                                                  │      │
│  │  API Gateway │ Auth │ Rate Limit │ Agent Registry │ Task Router │      │
│  └────────┬───────────────────────────────────────────────────────┘      │
│           │                                                              │
│  ┌────────▼───────────────────────────────────────────────────────┐      │
│  │  Cognitive Engine (today: runtime crate)                        │      │
│  │  ├── FallbackSelector (TF-IDF + LLM tool selection)            │      │
│  │  ├── StallGuard + ErrorRecovery + Circuit Breaker               │      │
│  │  ├── LearnedContext (EntityGraph, PatternLibrary, Calibrator)    │      │
│  │  ├── Prompt Cache + Token Budget Management                     │      │
│  │  ├── LLM orchestration (target server-side; today `chat_stream` in CLI)           │      │
│  │  └── Active model row + decrypt: `services::resolve_active_llm_model`            │      │
│  └────────┬───────────────────────────────────────────────────────┘      │
│           │                                                              │
│  ┌────────▼───────────────────────────────────────────────────────┐      │
│  │  State Plane (today: services crate)                            │      │
│  │  ├── Session lifecycle (create, restore, checkpoint, replay)     │      │
│  │  ├── Task orchestration (plan, lease, checkpoint, resume)        │      │
│  │  ├── Memory / Skill / Context CRUD                               │      │
│  │  ├── Sync engine (5 domains: Learning, Events, Tasks, etc.)      │      │
│  │  ├── Approval workflow (tool gate, plan approval, escalation)    │      │
│  │  └── Event ingestion (batch, idempotent, at-least-once)          │      │
│  └────────┬───────────────────────────────────────────────────────┘      │
│           │                                                              │
│  ┌────────▼───────────────────────────────────────────────────────┐      │
│  │  Storage Layer (MatrixOne HTAP)                                 │      │
│  │  Sessions │ Events │ Tasks │ Leases │ Learning │ Preferences     │      │
│  └────────────────────────────────────────────────────────────────┘      │
│                                                                          │
└────────────────────┬─────────────────────────────────────────────────────┘
                     │ Tool Execution Requests (JSON-RPC / gRPC)
┌────────────────────▼─────────────────────────────────────────────────────┐
│                      EDGE EXECUTORS                                      │
│                                                                          │
│  One per client session. Runs on user's machine (or cloud sandbox).      │
│                                                                          │
│  ┌──────────────────────────────────────────────────────────────────┐    │
│  │  Local Tool Runtime                                               │    │
│  │  ├── bash / shell execution                                       │    │
│  │  ├── File system read/write                                       │    │
│  │  ├── Git operations (gix, pure Rust)                              │    │
│  │  ├── Code intelligence (tree-sitter, 10 AST tools)               │    │
│  │  ├── MCP server connections                                       │    │
│  │  └── Build/test loop with error delta tracking                    │    │
│  └──────────────────────────────────────────────────────────────────┘    │
│                                                                          │
│  Edge executor is DUMB — it receives tool invocations, runs them         │
│  locally, and returns results. No LLM calls, no state management.        │
│                                                                          │
│  Privacy boundary: code, credentials, and filesystem never leave edge.   │
└──────────────────────────────────────────────────────────────────────────┘
```

**Key insight**: Today, `mo-agent` CLI is simultaneously a thin client, cognitive engine (`chat_stream`), AND edge executor (tens of kLOC Rust). The target separates these into independently deployable components sharing a common protocol.

### 5.3 Headless Cloud Runtime: Client-Runtime Decoupling

#### The Problem

The current architecture tightly couples the CLI to the agent runtime. Evidence:

1. **`chat_stream/sse_loop/`** in `mo-agent` still hosts the multi-turn LLM interaction loop — stall detection, token budgeting, schema pruning, response guards. **Parallel local tool execution was removed**; the headless path assembles results from SSE `tool_request` / edge callbacks (§5.5). Moving the remaining loop into `runtime` is still open.

2. **`ReplState` in `main.rs:380-447`** holds infrastructure state that belongs in a server:
   ```rust
   // These fields are in the CLI's REPL state — they should be server-side:
   sync_orchestrator: Option<SyncOrchestrator>,      // cloud sync coordination
   matrixone_pool: Option<Arc<sqlx::Pool<MySql>>>,   // database connection pool
   ingestion_sender: Option<IngestionSender>,         // cloud event publishing
   learning_snapshot: Option<String>,                 // cloud learning data
   ```

3. ~~**`sync_adapters.rs` in `mo-agent`**~~ **Resolved**: file lives in `runtime/src/sync_adapters.rs`; CLI wires `SyncOrchestrator` only.

4. **`InProcessChatTurnBridge`** ~~full infra coupling~~ **Partially resolved**: `infra_llm_models` lookup + Fernet decrypt live in `services::resolve_active_llm_model` (`models.rs`). Further slimming would push more bridge wiring behind `ModelService` trait calls.

#### The Solution: Headless Cloud Runtime

A **headless cloud runtime** is an agent runtime that has no UI, no local filesystem access, and no client-specific code. It provides all cognitive capabilities via API:

```
                  ┌─────────────────────────────┐
                  │   Headless Cloud Runtime     │
                  │                               │
  CLI ────────────▶  /chat/stream   (SSE)        │
  Web ────────────▶  /sessions/*    (REST)       │
  IDE ────────────▶  /memory/*      (REST)       │
  SDK ────────────▶  /tools/invoke  (callback)   │
                  │  /tasks/*       (REST)       │
                  │  /plans/*       (REST)       │
                  │  /skills/*      (REST)       │
                  │  /approval/*    (WS)         │
                  │                               │
                  │  All clients share:           │
                  │  • Same auth (JWT)            │
                  │  • Same protocol              │
                  │  • Same state plane           │
                  │  • Same cognitive engine       │
                  └──────────────┬────────────────┘
                                 │
                   Tool execution callback
                                 │
                  ┌──────────────▼────────────────┐
                  │   Edge Executor (per client)   │
                  │   bash, fs, git, code_intel    │
                  │   Registered via /agents/edge  │
                  └───────────────────────────────┘
```

**What "headless" means concretely**:
- The server binary (`mo-agent-server`) becomes the single source of truth for all agent state
- LLM calls happen server-side only (not in CLI)
- Tool selection, stall detection, token budgeting — all server-side
- CLI sends user messages and receives structured events (tool requests, text chunks, plan updates)
- Edge executor is a lightweight process that registers its tool capabilities and executes tool invocations

#### What Must Move (with code evidence)

| Component | From | To | Evidence | Effort |
|-----------|------|----|----------|--------|
| `chat_stream/sse_loop/` core loop | `mo-agent` | `runtime` server handler | Multi-turn orchestration still in CLI crate; headless tool path done | Large |
| `plan_decompose.rs` | ~~`mo-agent`~~ → **`runtime`** | `runtime` | ✅ **Done** — `runtime/src/plan_decompose.rs` | — |
| `sync_adapters.rs` | ~~`mo-agent`~~ → **`runtime`** | `runtime` (not `services`: avoids dep cycle) | ✅ **Done** — `runtime/src/sync_adapters.rs` | — |
| `SyncOrchestrator` construction | `ReplState` (`main.rs:436`) | `AppState` (`state_builder.rs`) | CLI holds cloud sync state | Small |
| `MatrixOne pool` | `ReplState` (`main.rs:406`) | `AppState` (already has `shared_pool`) | Duplicate pool in CLI | Small |
| `IngestionSender` | `ReplState` (`main.rs:402`) | Server-side event pipeline | CLI publishes cloud events directly | Small |
| `InProcessChatTurnBridge` model SQL | ~~`runtime/turn/`~~ | `services` | ✅ Query/decrypt extracted to `resolve_active_llm_model` | Remaining: pool/settings wiring in bridge |
| Skill registry + loader | `agentic_loop_turn.rs` (skill text merge) + `sse_loop/mod.rs` entry | `runtime` `SkillService` | Skill loading is cognitive, not CLI | Medium |

#### What Stays on Edge (must NOT move)

| Component | File | Why It Stays |
|-----------|------|-------------|
| `bash` tool execution | `edge_tools.rs` | Runs on user's machine, privacy |
| File system read/write | `edge_tools.rs` | Local filesystem access |
| Git operations (gix) | `edge_tools/git_gix.rs` | Local repo, no network dependency |
| Code intelligence | `edge_tools/code_intel.rs` | Tree-sitter AST parsing, local files |
| Build/test loop | `edge_tools/build_test.rs` | Local build toolchain |
| MCP server connections | `mcp_client.rs` | User's MCP servers |
| Permission manager | `permission_manager.rs` | Local approval UX |
| REPL UI + rendering | `repl_ui.rs`, `stream_render.rs` | Terminal rendering |

#### Open Design Challenges

The headless cloud runtime introduces **three challenges** that are not yet fully addressed:

**1. Latency amplification from tool callbacks**

Each tool invocation adds a cloud→edge→cloud network round-trip. For tools like `read_file` (~1ms local), this could add 50-200ms per call. A 5-tool turn adds 250ms-1s.

*Mitigation strategies (to be designed)*:
- **Batch tool invocations**: Cloud groups multiple independent tool calls into one callback
- **Speculative execution**: Edge pre-executes likely follow-up reads while waiting for cloud
- **Tool result caching**: Edge caches recently-read files; cloud sends cache-check instead of re-read
- **Latency threshold**: Tools below a latency threshold run on the current bridge model (inline); tools above threshold (bash, build, test) use async callback

**2. Offline degradation**

The headless model requires cloud connectivity for every LLM call. The "local-first" claim must be qualified:
- **Local-first for state**: Session journal, learning snapshot, tool execution all work offline ✅
- **Cloud-required for cognition**: LLM calls, tool selection, plan decomposition require cloud ⚠️
- **Degradation mode**: When cloud is unreachable, edge executor can replay last plan step, continue running approved tools, or queue messages for later. But it cannot start new cognitive work.
- **Future**: Local small model fallback for basic tool selection when cloud is unreachable

**3. Connection resilience**

What happens when edge disconnects mid-tool-execution?
- Tool execution is idempotent at the edge level (file reads, bash commands)
- Cloud tracks pending tool requests with `request_id` and timeout
- Edge reconnects → re-fetches pending requests → resubmits results
- If edge fails to reconnect within lease TTL, cloud marks the turn as failed and allows retry
- **Must be designed**: Reconnection protocol with request deduplication

### 5.4 Responsibility Split: What Moves Where

A precise mapping of every responsibility in the current `mo-agent` CLI to its target location:

#### Cloud (Headless Runtime) — Control Plane + State Plane

| Responsibility | Current Owner | Target Owner | Notes |
|---------------|--------------|-------------|-------|
| LLM model resolution & API key management | `services::resolve_active_llm_model` | `services` (shared helper) | ✅ Active row + decrypt; bridge calls into services |
| Chat turn orchestration (stall, budget, guard) | `chat_stream/sse_loop/` + `explain_reports.rs` | `runtime` server handler | Core cognitive loop still in CLI |
| Tool selection (TF-IDF + LLM) | `runtime` `tool_selector.rs` | `runtime` (stays) | Already server-side ✅ |
| Session lifecycle (create, restore, checkpoint) | Split: `services` + `main.rs` | `services` (consolidate) | Remove `ReplState` session fields |
| Task/plan state machine | `services` `task_orchestrator.rs` | `services` (stays) | Already server-side ✅ |
| Plan decomposition | `runtime` `plan_decompose.rs` | `runtime` | ✅ Moved out of CLI |
| Learning sync + merge | `runtime` `sync_adapters.rs` | `runtime` (adapter crate layer) | ✅ Moved; uses `services` traits without `services`→`runtime` cycle |
| Event ingestion | `mo-agent` `ReplState` | `services` (already has `IngestionWorker`) | Remove from CLI |
| Memory/skill/context CRUD | `runtime` route handlers | `runtime` (stays) | Already server-side ✅ |
| Approval workflow | Not implemented | `services` new module | Cloud-resident for multi-client |

#### Edge (Executor) — Tool Runtime

| Responsibility | Current Owner | Target Owner | Notes |
|---------------|--------------|-------------|-------|
| 50 local tools | `mo-agent` `edge_tools.rs` + subdirs | Edge executor process | Packaged as standalone |
| Tool invocation → result | `chat_stream` assembles from edge round | Edge executor via callback API | Headless: SSE `tool_request` → edge → `post_tool_result` (§5.5) |
| Permission gates (bash, write) | `permission_manager.rs` | Edge executor local UX | User approves locally |
| MCP server management | `mcp_client.rs` | Edge executor | User's MCP servers |
| Build/test error tracking | `build_test.rs` | Edge executor | Local build state |

#### Thin Client — Render + Dispatch

| Responsibility | Current Owner | Target Owner | Notes |
|---------------|--------------|-------------|-------|
| User input (text, slash commands) | `main.rs` REPL | Thin client | Parse → dispatch to cloud API |
| SSE stream rendering | `stream_render.rs` | Thin client | Render cloud events |
| Session switching | `slash_session.rs` | Thin client → cloud API | Client calls `/sessions/*` |
| Memory management | `slash_memory.rs` | Thin client → cloud API | Client calls `/memory/*` |

### 5.5 Thin Client Protocol

All clients (CLI, Web, IDE) share a single protocol to interact with the headless cloud runtime:

#### Chat Turn Protocol (SSE)

```
POST /chat/stream
Content-Type: application/json
Authorization: Bearer <jwt>

{
  "session_id": "s-123",
  "message": "Fix the login bug",
  "edge_executor_id": "edge-abc",    // NEW: which edge can execute tools
  "capabilities": ["bash", "fs", "git", "code_intel"]  // NEW: available tools
}

← SSE stream:
event: session_info
data: {"session_id": "s-123", "run_id": "r-456"}

event: tool_request        // NEW: cloud asks edge to run a tool
data: {"request_id": "tr-1", "tool": "bash", "args": {"command": "ls -la"}}

event: text_delta
data: {"content": "I'll check the directory structure..."}

event: plan_update
data: {"subtask_id": "st-1", "status": "in_progress"}

event: approval_required   // NEW: cloud pauses for user approval
data: {"request_id": "ap-1", "tool": "write_file", "path": "src/auth.rs"}

event: done
data: {"tokens_used": 4500}
```

#### Tool Execution Callback (Edge → Cloud)

```
POST /tools/result
Authorization: Bearer <jwt>
X-Mo-Edge-Id: edge-abc

{
  "request_id": "tr-1",
  "status": "success",
  "output": "total 24\ndrwxr-xr-x  5 user user 4096 ...",
  "duration_ms": 45
}
```

#### Approval Callback (Client → Cloud)

```
POST /approval/respond
Authorization: Bearer <jwt>

{
  "request_id": "ap-1",
  "decision": "allow",          // "allow" | "deny" | "allow_session"
  "reason": "User approved"
}
```

#### State CRUD (REST, shared by all clients)

```
GET    /sessions                         # list sessions
POST   /sessions                         # create session
GET    /sessions/{id}                    # get session details
DELETE /sessions/{id}                    # end session
POST   /sessions/{id}/replay            # replay session

GET    /memory/search?q=...             # search memories
POST   /memory/store                    # store memory
POST   /memory/feedback                 # relevance feedback

GET    /tasks                           # list tasks
POST   /tasks                           # create task
PATCH  /tasks/{id}                      # update task status
POST   /tasks/{id}/checkpoint           # save checkpoint

GET    /skills                          # list available skills
POST   /skills/{id}/configure           # configure skill

GET    /context                         # get current context window
POST   /context/prune                   # manual context pruning

GET    /plans/{id}                      # get plan state
POST   /plans/{id}/approve              # approve plan step
POST   /plans/{id}/resume               # resume paused plan
```

> **Note**: Many of these routes already exist in `router_builder.rs` (~50 routes). The gap is that today's CLI bypasses them (calling internal Rust functions directly) and the Web UI only uses ~15 routes as read-only GETs. The thin client protocol makes all clients go through the same API surface.

#### 5.5.1 Reference implementation: `mo-thin-client`

Shared crate: `rust/crates/mo-thin-client` (dependency of `mo-agent-cli` and any future Web/IDE clients).

| Layer | Module / type | Role |
|-------|----------------|------|
| Paths | [`paths.rs`](../../rust/crates/mo-thin-client/src/paths.rs) | Canonical URL constants and `session` / `task` / `context_capture` helpers — keep in sync with `runtime/src/server/router_builder.rs`. |
| Bodies | [`protocol.rs`](../../rust/crates/mo-thin-client/src/protocol.rs) | `ChatStreamRequest` (§5.5 `edge_executor_id`, `capabilities`), `ToolResultRequest`, `ApprovalRespondRequest`, `EdgeRegisterRequest`, `EdgeHeartbeatRequest`, `TaskLeaseMutationRequest`, `StreamEvent`, session DTOs. |
| Light edge | [`edge.rs`](../../rust/crates/mo-thin-client/src/edge.rs) | §5.5.2: `advertise_executor`, `builtin_capability_preset`, `edge_register_with_capabilities`, `MO_EDGE_ID_HEADER`. |
| Transport | [`ThinClient`](../../rust/crates/mo-thin-client/src/client.rs) | `reqwest`-based HTTP + SSE (`chat_stream` / `post_chat_turn`), auth, sessions, skills, memory, tasks (incl. lease helpers), context snapshots, `get_url` for off-origin probes (e.g. Memoria health). |
| Admin CLI | [`mo-admin-cli`](../../rust/crates/mo-admin/) | Same crate: **only** [`ThinClient`](../../rust/crates/mo-thin-client/src/client.rs) for server calls (no standalone `reqwest` client). |

**Chat endpoints (two surfaces, same SSE framing):**

- **`POST /chat/stream`** — preferred for typed streaming; body is [`ChatStreamRequest`](../../rust/crates/mo-thin-client/src/protocol.rs). Use `ThinClient::chat_stream` / `chat_stream_collect`.
- **`POST /chat/turn`** — used by the current CLI agentic loop (tools, retries); same `Accept: text/event-stream`, JSON body shaped like the server’s chat-turn handler. Use `ThinClient::post_chat_turn` / `post_chat_turn_retry_429`.

**§5.5 callbacks** — `mo-agent-server` exposes `POST /tools/result`, `POST /approval/respond` (JWT + `AppState::edge_callback_ledger`). **Registry**: `POST /agents/edge` and `POST /agents/edge/heartbeat` persist `edge_agent_registry` (typed bodies in `mo-thin-client`). **Task leases**: `GET /tasks/{id}/lease`, `POST .../lease/claim|release|renew` (JWT + `X-Mo-Edge-Id`). [`ThinClient`](../../rust/crates/mo-thin-client/src/client.rs): `post_tool_result`, `post_approval`, `post_agents_edge_register`, `post_agents_edge_heartbeat`, `get_task_lease`, `post_task_lease_claim` / `release` / `renew`.

**Server-aligned additions (not all listed in the prose above):** `GET/POST /tasks`, task lease paths above, `PUT /tasks/{id}/status`, `GET /tasks/{id}/progress`, `GET/POST /context`, `GET /context/{id}`, `POST /memory/retrieve`, `POST /chat/route` — path helpers in [`paths.rs`](../../rust/crates/mo-thin-client/src/paths.rs) and `ThinClient` in `client.rs`.

#### 5.5.2 Lightweight edge executor

The **edge** tier is intentionally **thin**: it is not a second copy of the cognitive runtime. A *light edge* process:

| In scope | Out of scope (stays cloud / `mo-agent-server`) |
|----------|-----------------------------------------------|
| Hold JWT (or receive per-turn token), call `POST /chat/stream` with SSE | LLM calls, tool selection, stall detection, plan decomposition |
| Parse [`StreamEvent`](../../rust/crates/mo-thin-client/src/protocol.rs); on `tool_request`, run **local** tools (bash, fs, git, tree-sitter, …) | `SyncOrchestrator`, MatrixOne pool, `IngestionSender`, session journal |
| `POST /tools/result` with [`ToolResultRequest`](../../rust/crates/mo-thin-client/src/protocol.rs) + `X-Mo-Edge-Id` | EntityGraph, PatternLibrary, progressive calibration |
| Optional: `POST /approval/respond` if the UX is on the same machine | Cross-session learning merge |
| Optional: `POST /agents/edge` (+ heartbeat) with [`EdgeRegisterRequest`](../../rust/crates/mo-thin-client/src/protocol.rs); task lease mutations with [`TaskLeaseMutationRequest`](../../rust/crates/mo-thin-client/src/protocol.rs) for multi-agent coordination | `SyncOrchestrator` / MatrixOne pool (server-side) |

**Dependency budget**: `mo-thin-client` (+ serde / async runtime) and a **local tool runner** crate or embedded module. **Must not** depend on `mo-agent` (CLI), `runtime` pipeline, or `services` — otherwise the edge stops being deployable as a small sidecar (CI runner, IDE helper, headless worker).

**Advertise capabilities** on every chat turn so the cloud router knows which `tool_request` events to emit:

- Rust: [`advertise_executor`](../../rust/crates/mo-thin-client/src/edge.rs) on [`ChatStreamRequest`](../../rust/crates/mo-thin-client/src/protocol.rs) sets `edge_executor_id` and, if empty, fills [`builtin_capability_preset`](../../rust/crates/mo-thin-client/src/edge.rs) (`bash`, `fs`, `git`, `code_intel`).
- Header constant: [`MO_EDGE_ID_HEADER`](../../rust/crates/mo-thin-client/src/edge.rs) (`X-Mo-Edge-Id`) — used by [`ThinClient::post_tool_result`](../../rust/crates/mo-thin-client/src/client.rs).

**Registry**: [`paths::AGENTS_EDGE`](../../rust/crates/mo-thin-client/src/paths.rs) / [`AGENTS_EDGE_HEARTBEAT`](../../rust/crates/mo-thin-client/src/paths.rs) call MatrixOne `edge_agent_registry` (after JWT). Use the same string for `body.edge_agent_id` and (typically) `X-Mo-Edge-Id` unless you split logical agent vs transport instance.

**Event loop (conceptual)**:

```text
open SSE (chat/stream with edge_executor_id + capabilities)
  → for each StreamEvent::ToolRequest { run locally → post_tool_result }
  → for each StreamEvent::ApprovalRequired { optional local UX → post_approval }
  → render text/plan events for UI
```

This is the same protocol as CLI/Web/IDE **thin clients**; the edge differs only in **also** executing tools and posting callbacks. Today’s `mo-agent` CLI still bundles thin client + **`chat_stream` cognitive loop** + edge tools — `plan_decompose` / `sync_adapters` now live in **`runtime`**. Next step: extract the remaining `chat_stream` loop to the server and ship a **standalone light edge** (§5.5.2).

**CLI wiring (current)**: each `/chat/turn` payload includes `edge_executor_id` (env `MO_EDGE_EXECUTOR_ID` or `edge-{uuid}` per process) and `capabilities` from [`builtin_capability_preset`](../../rust/crates/mo-thin-client/src/edge.rs). [`consume_turn_sse`](../../rust/crates/mo-agent/src/mo_agent/stream_render.rs) handles SSE in order: `tool_request` → execute → [`post_tool_result`](../../rust/crates/mo-thin-client/src/client.rs); `approval_required` → [`PermissionManager::resolve_cloud_approval`](../../rust/crates/mo-agent/src/mo_agent/permission_manager.rs) → [`post_approval`](../../rust/crates/mo-thin-client/src/client.rs). If the same tool+args also appears as `tool_call`, the agentic loop **reuses** `edge_callback_outputs` and skips a second local execution. The legacy `tool_call`-only path remains for servers that do not emit `tool_request`.

#### 5.5.3 CLI registry and heartbeat environment

After the REPL banner, if silent/auth left a valid access token, [`edge_lifecycle.rs`](../../rust/crates/mo-agent/src/mo_agent/edge_lifecycle.rs) (`register_and_start_heartbeat`) performs a single **`POST /agents/edge`** (typed [`EdgeRegisterRequest`](../../rust/crates/mo-thin-client/src/protocol.rs), same string for `edge_agent_id` and [`X-Mo-Edge-Id`](../../rust/crates/mo-thin-client/src/edge.rs) as `chat_stream`), optional enrichment of `hostname` / `worktree_path` from `HOSTNAME` or `COMPUTERNAME` and `std::env::current_dir()`, then a background loop of **`POST /agents/edge/heartbeat`**. Register failures are non-fatal (dim stderr; chat continues). On REPL exit the heartbeat task is **aborted**.

| Variable | Behavior |
|----------|----------|
| `MO_EDGE_REGISTRY` | **Enabled** when unset or set to any value other than `0`, `false`, or `off` (exact string match). Set to one of those three to **disable** cloud register and heartbeat entirely (no HTTP, no background task). |
| `MO_EDGE_HEARTBEAT_SECS` | Interval between heartbeats in seconds; default **120**. Set to **0** to register once (if enabled) but **not** start the background heartbeat task. Non-numeric values fall back to the default. |

---

## 6. Edge-Cloud State Model

> **Scope note**: §5.5.2 defines the *transport and responsibility* boundary for a minimal edge. §6 below covers **durable state sync** (learning, events, tasks) between edge-associated clients and cloud — orthogonal to how small the edge binary is.

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

**Current state (2026)**: [`EventAdapter`](../../rust/crates/runtime/src/sync_adapters.rs) and [`TaskAdapter`](../../rust/crates/runtime/src/sync_adapters.rs) are **implemented** — not stubs. The narrative below in §6.3–6.4 remains useful as design rationale (ingestion side-channel for events).

#### 6.2.1 Runtime sync adapters (implemented)

| Adapter | `SyncDomain` | Behavior | Code |
|---------|--------------|----------|------|
| **LearningAdapter** | Learning | Versioned push/pull via `StateSyncService` / `MatrixOneTransport` | [`sync_adapters.rs`](../../rust/crates/runtime/src/sync_adapters.rs) |
| **EventAdapter** | Events | Write path: `IngestionSender` + worker — `export_delta` returns `None`, `has_dirty_data` is `false` so the orchestrator does not double-push | [`EventAdapter`](../../rust/crates/runtime/src/sync_adapters.rs) |
| **TaskAdapter** | Tasks | Local mirror; **export** only tasks that are both dirty and present in [`TaskLeaseHoldCache`](../../rust/crates/services/src/multi_agent.rs); **pull** merges server pack into mirror; server enforces lease on [`push_tasks_pack_held_mysql`](../../rust/crates/services/src/multi_agent.rs) | [`TaskAdapter`](../../rust/crates/runtime/src/sync_adapters.rs) |
| **TemplateAdapter** | Templates | Pull-only cache of plan templates; cloud authoritative; push disabled (`PushTrigger::Never`) | [`TemplateAdapter`](../../rust/crates/runtime/src/sync_adapters.rs) |
| **PreferenceAdapter** | Preferences | Bidirectional key/value sync over `user_preferences` via `MatrixOneTransport` | [`PreferenceAdapter`](../../rust/crates/runtime/src/sync_adapters.rs) |

**Tests**: `services` crate includes [`multi_agent` unit tests](../../rust/crates/services/src/multi_agent.rs) (hold cache, TTL clamp, unconfigured services) and **ignored** MySQL integration tests in [`tests/multi_agent_integration.rs`](../../rust/crates/services/tests/multi_agent_integration.rs) (`MO_AGENT_MULTI_AGENT_IT=1`, `cargo test -p mo-agent-services multi_agent_integration -- --ignored`).

### 6.3 EventAdapter: Wire to Batch Ingestion

[`EventAdapter`](../../rust/crates/runtime/src/sync_adapters.rs) follows this model: it does **not** serialize event batches through `SyncOrchestrator`; live ingestion uses the existing `IngestionSender`. The snippet below is the intended shape (and matches the implementation’s “no delta export” policy):

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

> **Note**: Use IVFFlat indexes for vector search (`CREATE INDEX ... USING ivfflat ... lists=N`). IVFFlat is the production-ready index type for approximate nearest neighbor queries.

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

### 8.1 Agent Registry

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

#### Implementation Gap (Current vs Target)

The current `AgentRecord` (`services/src/agents.rs:29-39`) has basic fields (`agent_id`, `name`, `agent_type: String`, `is_active: bool`, `agent_config: JSON`) but is **missing critical multi-agent fields**:

| Field | Target Design | Current Implementation | Status |
|-------|--------------|----------------------|--------|
| `agent_id` | UUID | ✅ `agent_id: String` | Done |
| `agent_type` | Enum (User/System/Orchestrator) | ⚠️ `agent_type: String` | Needs enum |
| `capabilities` | `Vec<String>` tool names | ❌ Missing | **Blocker**: Can't route tasks to capable agents |
| `status` | Enum (Active/Idle/Draining/Dead) | ⚠️ `is_active: bool` | Needs state machine |
| `last_heartbeat` | Epoch timestamp | ❌ Missing | **Blocker**: Can't detect dead agents |
| `lease_ttl_secs` | Default 60s | ❌ Missing | Needed for lease expiry |
| `max_concurrent_tasks` | Capacity declaration | ❌ Missing | Needed for load balancing |
| `current_task_count` | Current load | ❌ Missing | Needed for routing |

**Routes exist** (`router_builder.rs:130-139`): `/agents` CRUD endpoints work, but no `/agents/{id}/heartbeat` endpoint.

**Effort estimate**: ~3-4 days to add missing fields, heartbeat endpoint, and Dead-agent detection cron.

### 8.2 Coordination Patterns

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

### 8.3 Communication Model

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

### 9.1 The Problem

Current `TaskRecord` has `user_id` but no agent ownership. Two agents can read the same task and start working on it simultaneously, producing conflicting results.

#### Current TaskRecord vs Target

Current `TaskRecord` (`services/src/task_orchestrator.rs`) has:
- ✅ `task_id`, `user_id`, `status` (Pending/InProgress/Paused/Completed/Failed), `checkpoint`
- ❌ No `agent_id` — tasks aren't owned by specific agents
- ❌ No `lease_version` — no CAS operations for safe concurrent access
- ❌ No `expires_at` — no lease expiry
- ❌ No `/tasks/{id}/lease` endpoint in `router_builder.rs`

**Gap**: Task leasing is **entirely design-only**. The `TaskLease` struct, lease protocol, and all lease endpoints described below need to be implemented from scratch. This is the **single largest blocker** for multi-agent coordination.

### 9.2 Task Lease Model

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

### 9.5 Distributed Plan Execution

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

### 10.1 The Challenge

Current learning sync assumes **single writer per user per profile**. With multiple agents:
- Agent A observes entity "React" used with tool `read_file` (confidence: 0.8)
- Agent B observes entity "React" used with tool `grep` (confidence: 0.6)
- Both push deltas to cloud simultaneously

### 10.2 Merge Strategies (Already Designed, Need Implementation)

| Data Type | Merge Strategy | Rationale | Implementation Status |
|-----------|---------------|-----------|----------------------|
| **EntityGraph** | Union merge, observation-count-wins | More observations = higher confidence | ✅ `entity.rs:278-290` — `merge()` implemented, takes higher observation count |
| **PatternLibrary** | Union merge (combine patterns) | Different agents see different patterns | ✅ `pattern.rs` — `merge()` implemented, combines unique patterns |
| **Calibrator** | Weighted average by observation count | More data = more reliable threshold | ✅ `calibration.rs` — `merge()` implemented |
| **ToolQuality** | Weighted merge by invocation count | More invocations = better signal | ⚠️ Partial |
| **Preferences** | Last-writer-wins (timestamp) | User intent is singular | ❌ Not implemented |

**Critical limitation**: All merge functions are **2-way only** (local + remote). For true multi-agent convergence with 3+ agents, need either:
- **Version vectors** to detect which observation is newer (not just "bigger count")
- **Per-agent scoping** so agent A's learning about "React + read_file" doesn't overwrite agent B's equally-valid "React + grep" preference
- **3-way merge** using a common ancestor snapshot (current `merge()` has no ancestor parameter)

These merge functions are correct for the single-user/multiple-sessions case. For true multi-agent, they need extension.

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

### 10.4 Confidence Gate for Learned Context

Already implemented in `tool_selector.rs`:

```rust
const MIN_LEARNED_ENTITY_CONFIDENCE: f64 = 0.30;

// Entity hints with decayed_confidence < 0.30 are filtered out
// This prevents stale cross-agent observations from polluting selection
```

This gate becomes more important with multi-agent: observations from other agents may be less relevant to the current agent's context. The confidence decay mechanism naturally handles this — observations not reinforced by the current agent will decay below the gate threshold.

---

## 11. Token Efficiency at Scale

### 11.1 Current Token Budget

```
System prompt:     ~2,000 tokens (identity, constraints, capabilities)
Learned context:   ~250-290 tokens (max 3 entity + 2 pattern + 2 calibration + 2 tool hints)
Tool catalog:      ~1,500 tokens (50 tools, compact descriptions)
Conversation:      Variable (elastic zone)
Memory injection:  ~100-2,400 tokens (intent-driven loading)
────────────────────────────────────────────
Total overhead:    ~3,750-6,190 tokens (before conversation)
```

### 11.2 Optimization Strategies

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

### 11.3 Multi-Agent Token Efficiency

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

### 13.2 Dual Version Control: Git Worktree + Git4Data

Multi-agent isolation leverages **two parallel version control systems** — one for filesystem, one for data — both using the branch/merge/diff paradigm:

| Layer | Mechanism | Branch | Merge | Diff | Rollback |
|-------|-----------|--------|-------|------|----------|
| **Filesystem** | `git worktree` | Per-agent working directory | `git merge` | `git diff` | `git reset` |
| **Data** | Git4Data | Per-agent database branch | `DATA BRANCH MERGE` | `DATA BRANCH DIFF` | `RESTORE FROM SNAPSHOT` |

**Why worktree (not just branch)**: Multiple agents run **concurrently** on the same machine. `git branch` alone requires checkout to switch — agents would block each other. `git worktree` gives each agent its own working directory linked to the same repo, enabling true parallel filesystem access.

**Combined architecture**:
```
project_root/
├── .git/                              # shared git repo
├── worktrees/
│   ├── agent-A/                       # git worktree (filesystem branch)
│   │   └── (full project files)       #   Agent A modifies files here
│   └── agent-B/                       # git worktree (filesystem branch)
│       └── (full project files)       #   Agent B modifies files here
│
└── MatrixOne
    ├── agent_a_branch/                # Git4Data branch (data branch)
    │   └── learning, events, metrics  #   Agent A's isolated data
    ├── agent_b_branch/                # Git4Data branch (data branch)
    │   └── learning, events, metrics  #   Agent B's isolated data
    └── workspace (main)               # Merge target for both
```

**The lifecycle mirrors git perfectly**:
```
1. SETUP:   git worktree add worktrees/agent-A -b agent-a-work
            DATA BRANCH CREATE DATABASE agent_a_branch FROM workspace;

2. EXECUTE: Agent A works freely in worktrees/agent-A/ (filesystem)
            Agent A writes to agent_a_branch (data)
            Both completely isolated from Agent B.

3. REVIEW:  git diff main..agent-a-work                    (filesystem diff)
            DATA BRANCH DIFF agent_a_branch AGAINST workspace (data diff)

4. MERGE:   git merge agent-a-work                         (filesystem merge)
            DATA BRANCH MERGE agent_a_branch INTO workspace  (data merge)
            Cherry-pick semantics (coming soon) for selective data merge.

5. CLEANUP: git worktree remove worktrees/agent-A
            DROP DATABASE agent_a_branch;  -- or keep for audit
```

**Why this is powerful**: Git and Git4Data complement each other — git handles code versioning, Git4Data handles data versioning. Together, they give each agent a **complete isolated workspace** (files + data) with structured merge at both levels. No other agent framework has this dual version control capability.

For **cloud agents**: container-per-agent (filesystem isolation via clone) + Git4Data branch (data isolation). Same data-level patterns, different filesystem mechanism.

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
├── 50 built-in tools (edge_tools.rs)
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

> **Note**: Phase ordering begins with **crate restructuring** (Phase 0) because the current layer violations block all multi-client and multi-agent work. Phase 0 is purely mechanical (move code, no new features) and unblocks everything else.

### Phase 0: Crate Restructuring (Headless Runtime Enablement)

**Goal**: Move cloud-side logic out of `mo-agent` CLI into server-side crates. After this phase, `mo-agent` is a thin client that speaks the same protocol as Web and IDE.

| Task | Status | Effort | Notes / code |
|------|--------|--------|----------------|
| Move `sync_adapters` out of CLI | ✅ Done | — | **`runtime/src/sync_adapters.rs`** (not `services`: would create `runtime`↔`services` cycle). CLI uses `mo_agent_runtime::sync_adapters`. |
| Move `plan_decompose` out of CLI | ✅ Done | — | **`runtime/src/plan_decompose.rs`**; CLI `use mo_agent_runtime::plan_decompose`. |
| Extract active LLM row query + decrypt from `bridge_inprocess` | ✅ Done | — | **`services::resolve_active_llm_model`** (`models.rs`); bridge calls it. |
| Extract shared edge prompt context + tool schema prune to `runtime` | ✅ Done (slice 1) | Small | **`runtime/src/turn/edge_prompt_context.rs`** (`detect_workspace_context`, `detect_project_languages`, `make_args_preview`); **`runtime/src/turn/tool_schema_prune.rs`** (`prune_tool_schemas`) — used by CLI `chat_stream` + `bridge_inprocess` |
| Extract tool result semantics (error / resource-limit / dedup key) | ✅ Done (slice 2) | Small | **`runtime/src/turn/tool_result_semantics.rs`** — `is_tool_error`, `is_resource_limit_output`, `normalize_tool_arguments`, `tool_dedup_signature`; CLI `chat_stream` + `stream_render` |
| Extract cloud approval tool-name policy (sec. 5.5 gate list) | ✅ Done (slice 3) | Small | **`runtime/src/turn/cloud_approval_policy.rs`** — `cloud_gated_tool_kind` / execute+required lists; `cloud_tool_delivery`; CLI `PermissionManager::classify` + `is_dangerous` delegate here |
| Extract LLM tool argument hints (path / command / normalize) | ✅ Done (slice 4) | Small | **`runtime/src/turn/tool_argument_hints.rs`** — `normalize_llm_function_arguments`, `path_hint_from_args`, `permission_prompt_primary_detail`; `cloud_tool_delivery` path hint; CLI prompts + dangerous-pattern `cmd` alias |
| Reuse hints in `make_args_preview` | ✅ Done (slice 5) | Tiny | **`edge_prompt_context`**: file/bash/grep previews share `tool_argument_hints` |
| Canonical tool arg keys in hints layer | ✅ Done (slice 6) | Small | **`tool_argument_hints`**: only `path` and `command` (no `cmd`, `file_path`, `target_file`); cloud approval path + CLI prompts + journal previews aligned |
| Extract SSE `data:` JSON line parser | ✅ Done (slice 7) | Small | **`runtime/src/turn/sse_data_lines.rs`** — `drain_sse_data_lines`, `finish_sse_data_buffer`, `parse_sse_data_json_events`; `json_events_from_sse_event_block` (slice 39); `bridge_inprocess` stream + lifecycle contract tests |
| Extract SSE blank-line event blocks | ✅ Done (slice 8) | Small | **`runtime/src/turn/sse_blocks.rs`** — `drain_complete_sse_event_blocks` (`\n\n` / `\r\n\r\n`); **`SseBlankLineUtf8Buf`** (slice 39) shared by **`ChatTurnSseFramer`** + **`bridge_inprocess`** |
| Extract chat-turn factual / session heuristics | ✅ Done (slice 9) | Small | **`runtime/src/turn/chat_turn_heuristics.rs`** — `looks_like_factual_query`, `looks_like_live_query_with_context`, `should_force_factual_tool_retry`, `extract_repos_from_memory`, `is_session_not_found_error`; CLI `chat_stream` + `repl_turn` / `command_router` via `main` imports |
| Extract headless tool round assembly (cache list, stall-guard shape, edge output match) | ✅ Done (slice 10) | Small | **`runtime/src/turn/headless_tool_assembly.rs`** — `CACHEABLE_TOOLS`, `EdgeToolRoundRow`, `take_edge_output_for_tool_call`, `tool_calls_for_stall_guard`; CLI `chat_stream` + `EdgeToolRoundEntry` impl in `stream_render` |
| Extract boost-term → `DomainHint`, REPL history → OpenAI `messages`, stall schema filter | ✅ Done (slice 11) | Small | **`turn/boost_domain_hints.rs`** (`domain_hints_from_boost_terms`); **`turn/chat_history_openai.rs`** (`openai_messages_from_repl_history`); **`tool_schema_prune`** (`filter_tool_schemas_by_excluded_names`); CLI `chat_stream/sse_loop` |
| Extract `edge_profile` base + selector guidance for `/chat` payload | ✅ Done (slice 12) | Small | **`turn/chat_turn_edge_profile.rs`** (`read_git_branch_abbrev`, `memoria_env_for_edge_profile`, `build_base_edge_profile_value`, `detect_active_system_skills_in_message`); **`tool_registry/selection_edge_hints.rs`** (`top_unpinned_tool_names_from_report`, `apply_selector_hints_to_edge_profile`); CLI `sse_loop` |
| Extract `/chat` `explain` JSON + compaction-tier → `budget_pressure` | ✅ Done (slice 13) | Tiny | **`turn/chat_turn_explain_wire.rs`** (`chat_turn_explain_field_json`); **`prompts::CompactionTier::budget_pressure`** (`context.rs`); CLI `sse_loop` |
| Extract response-guard policy + pin invoked tool schemas | ✅ Done (slice 14) | Small | **`turn/response_guard.rs`** (`apply_response_guards`); **`tool_schema_prune::pin_invoked_tool_schemas`**; CLI `sse_loop` |
| Extract OpenAI assistant message with `tool_calls` (server vs edge round) | ✅ Done (slice 15) | Small | **`headless_tool_assembly`**: `openai_assistant_with_tool_calls_message`, `EdgeToolRoundRow::assistant_tool_call_id`; CLI `sse_loop` + `stream_render` |
| Extract memory-augmented boost-term merge (ranked memory → terms) | ✅ Done (slice 16) | Tiny | **`retrieval`**: `merge_boost_terms_unique`, `append_boost_terms_from_ranked_memory`; CLI `sse_loop` |
| Extract `/chat` base JSON payload (messages, session, model, explain, executor, capabilities, `edge_profile` base, active_skills) | ✅ Done (slice 17) | Small | **`turn/chat_turn_payload.rs`**: `ChatTurnBasePayloadInput`, `chat_turn_base_payload`, `merge_active_skills_into_edge_profile`; CLI `sse_loop` |
| Extract `/chat` dynamic fields + OpenAI tool / `tool_results` pair | ✅ Done (slice 18) | Small | **`chat_turn_payload`**: `set_payload_edge_tools`, `set_payload_tool_results_if_non_empty`, `merge_skill_instructions_into_edge_profile`; **`headless_tool_assembly`**: `openai_tool_roundtrip_values`; CLI `sse_loop` |
| Extract injected OpenAI `user` rows (factual retry, intent-drift correction) | ✅ Done (slice 19) | Tiny | **`chat_history_openai`**: `openai_user_content_message`; **`chat_turn_heuristics`**: `openai_factual_tool_retry_user_message`; CLI `sse_loop` |
| Extract TurnGuard verdict injections → `messages` | ✅ Done (slice 20) | Tiny | **`chat_history_openai`**: `append_openai_user_content_messages`; CLI `sse_loop` |
| Split `sse_loop.rs` → `sse_loop/mod.rs` + `sse_loop/run.rs` | ✅ Done (slice 21; coalesced slice 38) | Tiny | `stream_chat_sse` now in **`sse_loop/mod.rs`**; **`run.rs`** removed |
| Split skill instruction load + explain stderr from `sse_loop/run.rs` | ✅ Done (slice 22; coalesced 35 → 37) | Tiny | In **`agentic_loop_turn.rs`**; removed `skill_instructions_round` / `explain_sidecar` / `prepare_turn_request` files |
| Extract per-turn `/chat` payload prep (memory boost → selector → `edge_tools` → `record_plan`) | ✅ Done (slice 23; coalesced slice 37) | Small | **`agentic_loop_turn.rs`**: `PrepareTurnTelemetry` (pub to `agentic_sse_loop`), `prepare_chat_turn_payload` (private) |
| Extract headless tool round after SSE turn (assistant msg → per-tool edge results → OpenAI pairs) | ✅ Done (slice 24; coalesced slice 36) | Small | **`sse_loop/agentic_loop_turn.rs`** (private): `HeadlessToolRoundRequest`, `run_headless_tool_round`; removed `tool_round.rs` |
| Extract post-tool-turn policy (intent drift + TurnGuard verdict / checkpoints / retry) | ✅ Done (slice 25; coalesced 32 → **42**) | Small | **`runtime/turn/agentic_post_tool_policy.rs`**: `apply_agentic_post_tool_policy`, `AgenticPostToolPolicyRequest` / `Outcome` |
| Extract pre-tool stall preflight (signatures, `record_tool_calls`, name-stall) | ✅ Done (slice 26; coalesced 32 → **42**) | Tiny | **`runtime/turn/agentic_stall_preflight.rs`**: `apply_cli_agentic_stall_preflight`, `CliAgenticStallPreflightRequest` |
| Extract `TurnResult` ingest after `consume_turn_sse` (guards, usage, no-tool exit) | ✅ Done (slice 27; coalesced 32 → **41**) | Small | Logic in **`runtime/turn/agentic_turn_ingest.rs`** (`ingest_agentic_turn_stream`, slice 41); **`agentic_loop_turn`** builds snapshot + calls runtime |
| Extract per-iteration `/chat/turn` fetch (payload → POST → `consume_turn_sse`) | ✅ Done (slice 28; coalesced slice 32) | Small | **`sse_loop/agentic_loop_turn.rs`** (private): `ChatTurnSseFetchRequest`, `fetch_chat_turn_sse` |
| Extract post-loop CLI sidecars + `StreamResult` build | ✅ Done (slice 29; inlined slice 32) | Small | **`sse_loop/agentic_sse_loop.rs`** (private): `StreamLoopSidecarEprint`, `StreamResultBuild`, `eprint_stream_loop_sidecars`, `build_stream_result` |
| Extract one agentic SSE loop iteration (fetch through post-tool policy) | ✅ Done (slice 30) | Small | **`sse_loop/agentic_loop_turn.rs`**: `AgenticTurnRequest`, `AgenticLoopTurnExit`, `run_agentic_loop_iteration` |
| Consolidate agentic SSE session state + multi-turn driver + finalize | ✅ Done (slice 31) | Medium | **`sse_loop/agentic_sse_loop.rs`**: `AgenticSseLoopState`, `new` / `run_all_turns` / `into_stream_result`; entry in **`sse_loop/mod.rs`** (slice 38) |
| Coalesce per-iteration modules + drop `stream_result_finalize` file | ✅ Done (slice 32; ingest/post-tool/stall → **41–42**; headless → **43–47**) | Medium | **`agentic_loop_turn.rs`**: fetch, headless tool round, orchestration; ingest / stall preflight / post-tool policy in **`runtime/turn/agentic_*`**; **`headless_tool_*`** + **`hydrate_reflect`** + **`chat_turn_*`** helpers + **`skill_instructions_merge`**; **`agentic_sse_loop.rs`**: sidecars + `StreamResult` build |
| Extract `/chat/turn` SSE `data:` JSON dispatch (accumulator + edge pending + render hints) | ✅ Done (slice 33) | Medium | **`runtime/turn/chat_turn_sse_dispatch.rs`**: `ChatTurnSseAccum`, `ChatTurnEdgePending`, `dispatch_chat_turn_sse_event_block`; CLI `stream_render::TurnResult` derefs to accum + `ttft_ms` / edge maps |
| Extract `/chat/turn` SSE byte framing + TTFT alongside JSON dispatch | ✅ Done (slice 34) | Small | **`ChatTurnSseFramer`**: `push_lossy_bytes`, `take_trailing_dispatch_blob`; CLI `consume_turn_sse` calls runtime dispatch + `apply_sse_render_effects` |
| Coalesce explain stderr + skill-instruction round into `prepare_turn_request`; add full-body SSE parse | ✅ Done (slice 35; superseded 37) | Small | **`parse_chat_turn_sse_utf8_body`** in `chat_turn_sse_dispatch.rs`; explain/skill helpers now in **`agentic_loop_turn`** |
| Merge headless tool round file into `agentic_loop_turn` | ✅ Done (slice 36) | Medium | Inlines `run_headless_tool_round`; **`sse_loop/`** shrinks further in slice 37 |
| Merge `prepare_turn_request` into `agentic_loop_turn` | ✅ Done (slice 37) | Medium | Drops **`prepare_turn_request.rs`**; **`sse_loop/`** had `run.rs` until slice 38 |
| Inline `sse_loop/run.rs` into `sse_loop/mod.rs` | ✅ Done (slice 38) | Tiny | **`sse_loop/`** = `mod.rs` + `agentic_sse_loop.rs` + `agentic_loop_turn.rs` only |
| Align `bridge_inprocess` LLM SSE with blank-line framing + shared `data:` parse | ✅ Done (slice 39) | Small | **`SseBlankLineUtf8Buf`** in **`sse_blocks`** (shared with **`ChatTurnSseFramer`**); **`parse_sse_chunks`**: blank-line blocks → `json_events_from_sse_event_block`; tail → `drain_sse_data_lines` / `finish_sse_data_buffer` |
| Extract CLI agentic stall signatures + name-stall + verdict turn penalties | ✅ Done (slice 40) | Small | **`runtime::turn::stall`**: `round_tool_call_sig_and_names`, `detect_cli_tool_name_stall` (uses **`SERVER_STALL_WINDOW`**), `CLI_AGENTIC_VERDICT_REMAINING_PENALTY_*`; **`agentic_loop_turn`** calls in |
| Extract `/chat/turn` stream ingest (guards, usage, factual retry, no-tool exit) | ✅ Done (slice 41) | Small | **`runtime::turn::agentic_turn_ingest`**: `ingest_agentic_turn_stream`, `AgenticTurnStreamSnapshot`, `AgenticTurnIngestMut`; CLI wires `TurnResult` → snapshot + edge index fn |
| Move TurnGuard verdict audit row type to runtime (`VerdictEvent` alias in CLI) | ✅ Done (slice 42) | Tiny | **`runtime::turn::agentic_verdict_audit::AgenticVerdictAuditEvent`**; mo-agent `pub(crate) type VerdictEvent = …` |
| Extend headless tool assembly (round indices, flat parse, timeout names, unknown-tool message) | ✅ Done (slice 43) | Tiny | **`headless_tool_assembly`**: `HeadlessRoundToolIdx`, `headless_round_tool_indices`, `parse_flat_tool_call_event`, `headless_timeout_aborted_tool_names`, `unknown_local_tool_error_message`; CLI `run_headless_tool_round` |
| Extract reflect session hydrate + headless tool postprocess + journal row builders | ✅ Done (slice 44) | Small | **`hydrate_reflect`**: `reflect_hydration_rel_path`, `hydrate_reflect_placeholder_if_needed`; **`headless_tool_postprocess`**: `enrich_headless_tool_output_for_errors_and_limits`, `append_headless_result_quality_feedback`, `format_headless_tool_duration`; **`headless_tool_journal`**: duplicate/cache/unknown `ToolCallRecord` factories; **`SemanticDedup::append_near_duplicate_hint_if_any`**; removed `chat_stream/hydrate_reflect.rs` |
| Extract `/chat` budget pressure + headless step/cache/checkpoint + executed journal row + skill-name merge | ✅ Done (slice 45) | Small | **`chat_turn_budget_pressure::budget_pressure_for_chat_turn`**; **`headless_tool_assembly::idempotency_cache_hit_message`**; **`headless_tool_postprocess`**: `HeadlessStepDeadline`, `HeadlessCacheableRecordCtx`, `record_headless_cacheable_success_and_semantic_hint`, `try_write_light_headless_step_checkpoint`; **`headless_tool_journal::journal_record_executed_tool_call`**; **`chat_history_openai::merge_skill_names_track`** |
| Extract prepare-turn explain lines + selection context builder + headless opening / OpenAI pair helpers | ✅ Done (slice 46) | Small | **`prepare_turn_explain_text`**: `restricted_tools_explain_text`, `selector_guidance_explain_text`; **`chat_turn_selection_context::build_agentic_tool_selection_context`**; **`headless_tool_assembly`**: `HeadlessRoundOpening`, `begin_headless_tool_round_opening`, `HEADLESS_DUPLICATE_WITHIN_TURN_BODY`, `headless_openai_duplicate_within_turn_pair`, `headless_idempotency_hit_openai_pair`, `headless_unknown_local_tool_openai_pair` |
| Extract headless stderr lines + skill instruction merge + `/chat` HTTP error string + step `record_plan` helper | ✅ Done (slice 47) | Small | **`headless_tool_stderr_lines`** (cache/unknown/resource/ok/err/footer + UTF-8 preview); **`skill_instructions_merge::merge_skill_instruction_bodies_for_chat`**; **`chat_turn_api_error::chat_turn_http_error_user_message`**; **`chat_turn_step_plan::record_agentic_step_plan_after_payload_prep`** |
| Implement tool execution callback protocol (cloud → edge) | ✅ Core path | Medium | §5.5 `/tools/result`, `tool_request` SSE; `chat_stream` **does not** re-execute tools for that path. |
| Add `edge_executor_id` to chat turn protocol | ✅ | Small | Thin client + §5.5.2 light edge helpers. |
| Move `SyncOrchestrator` construction from `ReplState` to `AppState` | ✅ Done | — | **`MatrixCloudRuntime`** bundles `SharedPool` + `IngestionSender` + `SyncOrchestrator`; `ReplState` / `AppState` hold `Option<Arc<MatrixCloudRuntime>>` only (no separate orchestrator field) |
| Move `IngestionSender` from `ReplState` to server pipeline | ✅ Done | — | Same bundle as row above; journal flush via `enqueue_journal_events` |
| Remove `matrixone_pool` from `ReplState` (use server `shared_pool`) | ✅ Done | — | Superseded by `MatrixCloudRuntime::shared_pool()`; no `matrixone_pool` field on `ReplState` |
| Refactor `chat_stream/`: cognitive loop → `runtime`, rendering stays CLI | 🟡 In progress | Large | **Slices 1–47** + **`sse_loop/`** (2 child modules + `mod.rs`) + **`runtime/turn/chat_turn_sse_dispatch`**. Remaining: move multi-turn loop to server |

**Success criteria** (unchanged): `mo-agent` CLI can be deleted and replaced with a ~500-line thin client; **not yet met** — `chat_stream` + `ReplState` infra fields remain.

**Correction vs older drafts**: `sync_adapters` targets **`runtime`**, not `services`.

### Phase 1: Complete Single-Agent Foundation

**Status**: ✅ **Sync-domain adapters complete** (2026); Phase 0 mechanical extraction is **mostly complete** (see Phase 0 table — large `chat_stream` loop remains in CLI).

**Goal**: Operational `DomainAdapter` implementations for all sync domains. LearningAdapter lives in **`runtime::sync_adapters`**, sharing `services` sync traits.

| Task | Status | Effort | Dependency |
|------|--------|--------|------------|
| Wire EventAdapter to IngestionSender (`EventAdapter` in `runtime`, sender from CLI/server state) | ✅ Done | Small | [`EventAdapter::new(sender)`](../../rust/crates/runtime/src/sync_adapters.rs) + [`IngestionSender::disconnected()`](../../rust/crates/services/src/event_ingestion.rs) for tests |
| Implement TaskAdapter with lease protocol | ✅ Done | Medium | [`TaskAdapter`](../../rust/crates/runtime/src/sync_adapters.rs) + Phase 3 DB (`task_leases`, [`push_tasks_pack_held_mysql`](../../rust/crates/services/src/multi_agent.rs)); exports only leased+dirty tasks |
| Implement TemplateAdapter (read-only pull) | ✅ Done | Small | [`TemplateAdapter`](../../rust/crates/runtime/src/sync_adapters.rs) + [`StateSyncService::pull_plan_templates_pack`](../../rust/crates/services/src/state_sync.rs) + [`MatrixOneTransport`](../../rust/crates/runtime/src/sync_adapters.rs) `Templates` pull; [`PushTrigger::Never`](../../rust/crates/services/src/sync_engine.rs) for templates |
| Implement PreferenceAdapter (bidirectional) | ✅ Done | Small | [`PreferenceAdapter`](../../rust/crates/runtime/src/sync_adapters.rs) + `MatrixOneTransport` preferences push/pull via `user_preferences` |

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

**Status**: ✅ **Implemented** (server + HTTP + sync); optional hardening (git worktree isolation, broader integration tests in CI) remains.

**Goal**: Multiple agents can safely claim and execute tasks without conflicts.

**Implemented**: `edge_agent_registry` + `task_leases` ([`storage.rs`](../../rust/crates/services/src/storage.rs)), [`DatabaseEdgeRegistryService`](../../rust/crates/services/src/multi_agent.rs) / [`DatabaseTaskLeaseService`](../../rust/crates/services/src/multi_agent.rs), `POST /agents/edge` and `POST /agents/edge/heartbeat`, task lease routes under `/tasks/{task_id}/lease/*`, `agent_tasks.agent_id`, `StateSyncService::pull_tasks_pack` / `push_tasks_pack_held`, [`TaskAdapter`](../../rust/crates/runtime/src/sync_adapters.rs) + [`MatrixOneTransport`](../../rust/crates/runtime/src/sync_adapters.rs) for [`SyncDomain::Tasks`](../../rust/crates/services/src/sync_engine.rs). CLI chat uses `MO_EDGE_EXECUTOR_ID` (or `edge-{uuid}`) for `edge_executor_id` / `X-Mo-Edge-Id`; [`MatrixCloudRuntime`](../../rust/crates/runtime/src/matrix_cloud_runtime.rs) may use `MO_EDGE_AGENT_ID` for the same logical role in server-side wiring — keep **one** id per process consistent with lease claim bodies.

| Task | Status | Notes |
|------|--------|--------|
| `edge_agent_registry` + heartbeat | ✅ Done | Typed bodies in `mo-thin-client`; see [`edge_callback_handlers`](../../rust/crates/runtime/src/server/edge_callback_handlers.rs) |
| `task_leases` + transactional claim | ✅ Done | `SELECT … FOR UPDATE` on `agent_tasks` + `task_leases` in [`try_claim_lease`](../../rust/crates/services/src/multi_agent.rs) |
| `agent_id` on `TaskRecord` / `agent_tasks` | ✅ Done | Nullable `agent_id` column + lease-driven updates |
| Lease-aware TaskAdapter + held push | ✅ Done | [`TaskAdapter`](../../rust/crates/runtime/src/sync_adapters.rs), [`push_tasks_pack_held_mysql`](../../rust/crates/services/src/multi_agent.rs) |
| Git worktree isolation for multi-agent edge | ❌ Open | Still future work |
| Automated DB tests in default CI | ⚠️ Partial | Unit tests always on; MySQL scenarios: [`tests/multi_agent_integration.rs`](../../rust/crates/services/tests/multi_agent_integration.rs) (`--ignored`) |

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
| MatrixOne HTAP aggregation queries for entity/pattern convergence | Medium | Phase 3 |
| Weighted aggregation for Calibrator (SQL, not Rust) | Small | HTAP queries |
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
| Tool count | ~4 fixed | ~10 | LSP-based | Shell + browser | Pluggable | Pluggable | 50 (production) | 50+ |
| Code intelligence | Trained model | Tree-sitter + model | AST + LSP | Claude-backed shell | None | None | 10 tree-sitter tools | 10+ |
| Token efficiency | ~60% | ~40% (200K window) | ~95% (symbol) | ~50% | Per-thread | Full replay | ~90% (intent-driven) | ~95% (progressive) |
| Self-correction | Basic repair | None | LSP validation | Reflection loop | Node retry | Callback | StallGuard + ErrorRecovery | + Drift detection |
| Privacy (local-first) | ❌ Cloud only | ✅ | ✅ | ⚠️ Container | Configurable | Configurable | ✅ | ✅ |
| Offline support | ❌ | ⚠️ Needs API | ❌ | ❌ | Local model | Local model | ⚠️ Journal works | ✅ Queue + sync |

---

## Appendix B: Key File References

| Area | File | Key Lines | Layer |
|------|------|-----------|-------|
| Sync engine | `services/src/sync_engine.rs` | DomainAdapter trait, SyncOrchestrator, SyncEnvelope | services ✅ |
| Learning sync | `services/src/state_sync.rs` | StateSyncService, optimistic locking, gzip | services ✅ |
| Event ingestion | `services/src/event_ingestion.rs` | IngestionWorker, batch flush, idempotency | services ✅ |
| Session restore | `services/src/session_restore.rs` | HybridRestoreService, RestoredSession | services ✅ |
| Session journal | `services/src/session_journal.rs` | JournalWriter, append-only JSONL | services ✅ |
| Task orchestrator | `services/src/task_orchestrator.rs` | TaskRecord, TaskCheckpoint, SubtaskPlan | services ✅ |
| Phase 3 registry & leases | `services/src/multi_agent.rs` | `DatabaseEdgeRegistryService`, `DatabaseTaskLeaseService`, `push_tasks_pack_held_mysql`, `TaskLeaseHoldCache` | services ✅ |
| Sync adapters | `runtime/src/sync_adapters.rs` | LearningAdapter, EventAdapter (ingestion), TemplateAdapter (pull cache), PreferenceAdapter (bidirectional), TaskAdapter (lease-filtered tasks) | runtime ✅ |
| Active LLM resolution | `services/src/models.rs` | `resolve_active_llm_model`, `ResolvedActiveLlmModel` | services ✅ |
| Tool selector | `runtime/src/tool_selector.rs` | LearnedContext, FallbackSelector, confidence gate | runtime ✅ |
| Bridge (in-process) | `runtime/src/turn/bridge_inprocess.rs` | Prompt cache, streaming LLM; `sse_blocks` + `sse_data_lines` (`json_events_from_sse_event_block`, slice 39); `resolve_active_llm_model`; `tool_schema_prune` | runtime ✅ (SQL out of bridge) |
| Edge prompt context | `runtime/src/turn/edge_prompt_context.rs` | Workspace/lang detection; `make_args_preview` uses `tool_argument_hints` for path/command (CLI `chat_stream`) | runtime ✅ |
| Tool schema prune | `runtime/src/turn/tool_schema_prune.rs` | Tiered pruning + `filter_tool_schemas_by_excluded_names` (stall-restricted tools) | runtime ✅ |
| Boost → domain hints | `runtime/src/turn/boost_domain_hints.rs` | `domain_hints_from_boost_terms` → `DomainHint` for selector | runtime ✅ |
| REPL → OpenAI messages | `runtime/src/turn/chat_history_openai.rs` | `openai_messages_from_repl_history`, `openai_user_content_message`, `append_openai_user_content_messages` (TurnGuard injections) | runtime ✅ |
| Chat turn edge profile | `runtime/src/turn/chat_turn_edge_profile.rs` | Git branch, Memoria env, base `edge_profile` JSON, `active_skills` detection | runtime ✅ |
| Selector → edge hints | `runtime/src/tool_registry/selection_edge_hints.rs` | `apply_selector_hints_to_edge_profile`, `top_unpinned_tool_names_from_report` | runtime ✅ |
| Chat explain wire | `runtime/src/turn/chat_turn_explain_wire.rs` | `chat_turn_explain_field_json` → `false` / `true` / `"verbose"` | runtime ✅ |
| Chat turn base payload | `runtime/src/turn/chat_turn_payload.rs` | `chat_turn_base_payload`, `merge_active_skills_*`, `merge_skill_instructions_*`, `set_payload_edge_tools`, `set_payload_tool_results_if_non_empty` | runtime ✅ |
| Compaction → pressure | `runtime/src/prompts/context.rs` | `CompactionTier::budget_pressure` (0.0 / 0.3 / 0.6 / 0.9) | runtime ✅ |
| Tool result semantics | `runtime/src/turn/tool_result_semantics.rs` | `is_tool_error`, `is_resource_limit_output`, `tool_dedup_signature` (sec. 5.5 SSE vs `tool_call`) | runtime ✅ |
| Cloud approval policy | `runtime/src/turn/cloud_approval_policy.rs` | `CLOUD_APPROVAL_REQUIRED_TOOLS`, `CLOUD_APPROVAL_EXECUTE_TOOLS`, `cloud_gated_tool_kind` → CLI classify + cloud gate (sec. 5.5) | runtime ✅ |
| Tool argument hints | `runtime/src/turn/tool_argument_hints.rs` | Normalize LLM `arguments`; **`path` + `command` only** for hints (approval, CLI, previews) | runtime ✅ |
| SSE data JSON lines | `runtime/src/turn/sse_data_lines.rs` | Incremental `data:` line → JSON; per-block parse for blank-line SSE; `[DONE]`; EOF flush | runtime ✅ |
| SSE event blocks | `runtime/src/turn/sse_blocks.rs` | Blank-line delimited event text; **`SseBlankLineUtf8Buf`** for incremental HTTP chunks | runtime ✅ |
| `/chat/turn` SSE JSON dispatch | `runtime/src/turn/chat_turn_sse_dispatch.rs` | `ChatTurnSseAccum`, `ChatTurnEdgePending`, `ChatTurnSseFramer`, `dispatch_chat_turn_sse_event_block`, `parse_chat_turn_sse_utf8_body` | runtime ✅ |
| Chat turn heuristics | `runtime/src/turn/chat_turn_heuristics.rs` | Factual-query guard, `openai_factual_tool_retry_user_message`, session-not-found, repo extraction from memory text | runtime ✅ |
| Headless tool assembly | `runtime/src/turn/headless_tool_assembly.rs` | `CACHEABLE_TOOLS`, edge row → `tool_call` output match, `openai_assistant_with_tool_calls_message`, `openai_tool_roundtrip_values` | runtime ✅ |
| Bridge (HTTP) | `runtime/src/turn/bridge/mod.rs` | HttpChatTurnBridge, forwards to external service | runtime ✅ |
| Chat stream | `mo-agent/src/mo_agent/chat_stream/` (`sse_loop/mod.rs`, `agentic_sse_loop.rs`, `agentic_loop_turn.rs`) | Multi-turn loop orchestration + CLI rendering; imports runtime headless helpers | ⚠️ Core loop should move to runtime |
| Plan decompose | `runtime/src/plan_decompose.rs` | Long-horizon planning, subtask generation | runtime ✅ |
| Entity graph | `runtime/src/pipeline/entity.rs` | EntityKnowledge, decayed_confidence | runtime ✅ |
| Pattern library | `runtime/src/pipeline/pattern.rs` | ToolChainPattern, drift detection | runtime ✅ |
| Calibrator | `runtime/src/pipeline/calibration.rs` | ProgressiveCalibrator, 3-axis thresholds | runtime ✅ |
| Stall detection | `runtime/src/turn/stall.rs` | TurnGuard, intent drift, name stall | runtime ✅ |
| Error recovery | `runtime/src/turn/error_recovery.rs` | ErrorCategory, escalation thresholds | runtime ✅ |
| Code intelligence | `mo-agent/src/edge_tools/code_intel.rs` | 10 AST tools, tree-sitter, PARSER_CACHE | edge ✅ (stays) |
| Git (pure Rust) | `mo-agent/src/edge_tools/git_gix.rs` | 8 git tools via gix, no binary dependency | edge ✅ (stays) |
| App state | `runtime/src/app_state.rs` | AppState: 30+ service traits, turn writers, bridge | runtime ✅ |
| Server builder | `runtime/src/server/state_builder.rs` | Pipeline learning writer, bridge wiring | runtime ✅ |
| Router | `runtime/src/server/router_builder.rs` | 50+ HTTP routes, the thin client API surface | runtime ✅ |
| Edge §5.5 callbacks | `runtime/src/server/edge_callback_handlers.rs` | `/tools/result`, `/approval/respond`, `/agents/edge*` | runtime ✅ |
| Web proxy | `web/app/api/backend/[...path]/route.ts` | Server-side proxy, httpOnly cookie auth | web ✅ |
| Thin client (§5.5) | `mo-thin-client/src/{paths,protocol,client}.rs` | Shared HTTP+SSE; CLI and future clients | ✅ |
| Light edge (§5.5.2) | `mo-thin-client/src/edge.rs` | `edge_executor_id` helpers, capability preset, `X-Mo-Edge-Id` | ✅ |

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

*This document is the source of truth for multi-agent cloud runtime architecture. It supersedes aspirational descriptions in individual design docs where they conflict with the implementation-grounded analysis here. The design leverages MatrixOne's HTAP capabilities as the core competitive moat — not just as storage, but as the computational backbone for learning convergence, drift detection, and real-time observability. The v1.2 revision introduces the **headless cloud runtime** architecture: a three-tier split (thin clients + cloud runtime + edge executors) that enables multi-client support (CLI, Web, IDE) and is prerequisite to multi-agent coordination.*
