# mo-agent-engine Architecture

> **Status**: Living Document — source of truth for all design decisions  
> **Last Updated**: 2026-02-14

---

## What We Are

An **Agent Operating System** — not a framework, not a chatbot wrapper.

Frameworks (LangChain, CrewAI) give you libraries. An OS gives you **infrastructure guarantees**: every agent on this platform automatically gets auditable decisions, versioned memory, safe experimentation, and cost control. The agent developer writes a system prompt and picks skills. The platform handles everything else.

## The Problem Space

Five problems block AI agents from production adoption:

| # | Problem | Why It's Hard |
|---|---------|---------------|
| 1 | **Decisions are black boxes** | The data the agent saw has changed. The prompt was updated. The context window is gone. No way to reconstruct. |
| 2 | **Iteration is guesswork** | No regression testing for prompt/skill changes. Teams ship and pray. |
| 3 | **Memory is broken** | Agents forget across sessions. Knowledge updates silently invalidate past answers. No memory lifecycle. |
| 4 | **Experimentation is expensive** | Testing on production data requires full copies. Most teams skip it. |
| 5 | **Trust is unverifiable** | No confidence signals, no claim verification, no audit trail for compliance. |

## Core Thesis

```
Agent Decision = f(prompt@version, skill@version, context@snapshot, memory@state, llm_params)

Version the inputs → audit the outputs → learn from the gaps.
```

We don't compete on "smarter LLM." We compete on **trust infrastructure**: every decision auditable, every change testable, every data dependency versioned, every response carrying uncertainty signals.

## Architecture Layers

```
┌─────────────────────────────────────────────────────────────┐
│                      USER AGENTS (Apps)                     │
│  Code Review · CI Diagnosis · Data Analysis · Custom        │
│  Defined by: system_prompt + skill_set + model              │
├─────────────────────────────────────────────────────────────┤
│                    SYSTEM AGENTS (Daemons)                   │
│  Regression · Audit · Tuning · Eval                         │
│  Same execution model, elevated permissions, auto-triggered │
├─────────────────────────────────────────────────────────────┤
│                  PLATFORM SERVICES (Kernel)                  │
│  Memory │ Context │ Skills │ Planning │ Trust Engine │       │
│  LLM Client │ Streaming │ Evaluation │ Cost Control         │
├─────────────────────────────────────────────────────────────┤
│              PLATFORM STATE (MatrixOne instance)             │
│  Agent state: sessions, events, skills, decisions, snapshots│
│  Agent memory: episodic, semantic, procedural               │
│  Platform ops: SLO metrics, quality scores, audit trail     │
├─────────────────────────────────────────────────────────────┤
│          ENHANCED SERVICES (optional, MatrixOne-native)      │
│  Sandbox (clone) │ Pub/Sub (marketplace) │ Time Travel      │
│  Hybrid Search │ Branch/Diff/Merge │ Dynamic Table          │
│  Activated when user data is also on MatrixOne               │
│  Usage: pass db handle → service operates on user's DB      │
└─────────────────────────────────────────────────────────────┘
```

**Key distinction**: The platform manages agent state (sessions, events, memory, decisions) in its own MatrixOne instance. It does NOT assume ownership of the user's business data. Agents can operate on any data source — MySQL, PostgreSQL, S3, APIs, files.

The Enhanced Services layer activates when the user's data also lives on MatrixOne. In that case, services like Sandbox accept a `db: Session` handle and operate on the user's database directly — zero-copy clone, time-travel, hybrid search become available. These are opt-in capabilities, not platform requirements.

Adding a new User Agent = define `AgentProfile` (system_prompt + skills + model). Zero platform code.

## Design Documents

This is the index. Each document is the **single source of truth** for its domain.

| Document | Scope |
|----------|-------|
| [Memory and Context](memory-and-context.md) | Cognitive architecture: episodic/semantic/procedural memory, context engineering, attention budget, compaction, memory lifecycle |
| [Trust and Safety](trust-and-safety.md) | Decision audit, hallucination firewall, uncertainty quantification, regression gate, observability, guardrails |
| [Skills and Tools](skills-and-tools.md) | Skill system, MCP compatibility, tool design, side-effect profiles, progressive disclosure |
| [Agents and Orchestration](agents-and-orchestration.md) | ChatLoop, PAOR planning, multi-agent delegation, streaming, sub-agent architecture |
| [Data Versioning](data-versioning.md) | Git for Data: time travel, sandbox, branching, snapshot-scoped permissions, training data pipeline |
| [Evaluation and Evolution](evaluation-and-evolution.md) | Quality scoring, replay gating, prompt evolution, self-improving agents, meta-learning closed loop |

### Supporting Documents (Operational)

| Document | Scope |
|----------|-------|
| [LLM Integration](llm-integration.md) | Provider abstraction, routing, cost management, caching |
| [Authentication & Authorization](authentication-authorization.md) | JWT, ownership model, permissions |
| [Multi-tenancy](multi-tenancy-architecture.md) | Tenant isolation, data source flexibility |
| [Deployment](deployment-architecture-proposal.md) | Docker, CI/CD, monitoring |
| [Concurrency Model](concurrency-model.md) | Isolation guarantees, conflict resolution |

## Key Design Decisions

### 1. Memory is a First-Class System, Not an Afterthought

Industry trend: Anthropic's context engineering, Letta/MemGPT's memory OS, EverMemOS's dual-layer architecture, Observational Memory's 95% LongMemEval score — all point to memory as **the** differentiator for production agents.

Our position: Memory is not "RAG bolted on later." It is a cognitive architecture with distinct layers (sensory → working → episodic → semantic → procedural), each with its own storage, retrieval, and lifecycle. See [Memory and Context](memory-and-context.md).

### 2. Context Engineering Over Prompt Engineering

Following Anthropic's insight: the question is not "how to write a better prompt" but "what configuration of context maximizes desired behavior." Context is a finite attention budget. Every token must earn its place.

Our implementation: task-aware budget allocation, just-in-time retrieval, compaction for long-horizon tasks, structured note-taking for cross-session persistence. See [Memory and Context](memory-and-context.md).

### 3. Skills Are MCP-Compatible, Progressive-Disclosure Modules

Industry trend: Anthropic's Agent Skills (three-tier progressive loading), MCP as the tool protocol standard, Google's A2A for agent-to-agent communication.

Our position: Skills are versioned, declarative capabilities that load progressively (metadata → summary → full instructions). They expose MCP-compatible interfaces. External MCP servers can register as skill sources. See [Skills and Tools](skills-and-tools.md).

### 4. Trust Is Built Into the Platform, Not Bolted On

Industry trend: Decision lineage (Elixir Data), agentic observability (DataRobot), zero-trust agent architecture (Microsoft Foundry), AI guardrails as defense-in-depth.

Our position: Every decision binds to a data snapshot. Every response carries confidence signals. Every change passes a regression gate. This is not optional — it's platform infrastructure. See [Trust and Safety](trust-and-safety.md).

### 5. MatrixOne: Platform State + Optional Enhanced Services

MatrixOne serves two distinct roles:

**Role 1: Platform State Store (always)**

The platform's own state — agent sessions, events, memory, skills, decisions, audit trail — lives in a MatrixOne instance. This gives the platform native vector search (memory retrieval), fulltext search (event search), HTAP (event writes + quality analytics), and time-travel (decision audit) for its own operational data. No external vector DB or search engine needed for platform operations.

**Role 2: Enhanced Services for User Data (opt-in)**

When the user's business data also lives on MatrixOne, the platform can offer enhanced services that operate directly on the user's database. These services accept a `db` handle and use MatrixOne-native operations:

| Enhanced Service | What It Does | How It Works |
|---|---|---|
| Sandbox | Isolated experiment environment | `Sandbox(db=user_db)` → `CREATE CLONE` on user's DB |
| Time Travel | Query historical state | Snapshot binding on user's DB → exact past state |
| Hybrid Search | Vector + fulltext + SQL in one query | Only if user's tables have vector/fulltext indexes |
| Branch/Diff/Merge | Git-like data workflows | Branch user's tables → experiment → diff → merge |
| Skill Marketplace | Publish/subscribe skill definitions | `CREATE PUBLICATION` → cross-account sharing |
| Dynamic Table | Real-time derived views | Auto-refreshing aggregates on user's data |

The platform does NOT require users to put their data in MatrixOne. Agents can operate on any data source. Enhanced services are a value-add for MatrixOne users, not a platform dependency. See [data-versioning.md §6](data-versioning.md) for the concrete workflows.

### 6. Event-Centric, Not State-Centric

All state flows through `conversation_events` with causal chain tracking. This enables replay, lineage, audit, and multi-agent coordination through a single mechanism. Events are the universal interface.

## Industry Alignment

| Industry Direction | Our Alignment |
|-------------------|---------------|
| Anthropic Agent Teams: parallel coordination, shared task board | Teams with clone-per-agent speculative execution — run 4 approaches, keep the best |
| Vercel/Anthropic Skills: composable, shareable agent capabilities | Skill Marketplace via Publication — distribution without infrastructure |
| RouteMoA: cost-quality model routing | Self-improving router that learns from historical quality/cost data |
| MemGPT/EverMemOS: cognitive memory architecture | Hybrid memory recall — vector + fulltext + quality in one query, self-curating |
| Braintrust/Maxim: agent evaluation, regression testing | Clone-test-merge — zero-risk evolution, regression gate as database operation |
| Microsoft zero-trust: auditable, verifiable agent decisions | Snapshot-as-ground-truth — every decision reconstructable at any future point |
| Industry-wide: too many systems to integrate | Platform state on single MatrixOne instance (vector DB, search, analytics consolidated); enhanced services for MatrixOne users |

## What This Is NOT

- **Not a chatbot.** Agents understand intent, select actions, learn from context, and reproduce decisions.
- **Not a framework.** You don't import our library. You deploy on our platform and get guarantees.
- **Not vendor-locked to one LLM.** Multi-provider routing with circuit breaker and fallback chains.
- **Not a demo.** 527 tests passing, production Docker support, structured logging, rate limiting.

---

## Data Flow Architecture: Throughput Under Multi-Agent Load

### The Throughput Problem

A single agent turn: 1 event read (context assembly) + 1 snapshot write + 1 LLM call + 1 event write. Manageable.

A 4-agent team doing 10 turns each: 40 context assemblies + 40 snapshot writes + 40 LLM calls + 80+ event writes (including delegation, task claims, results). All hitting the same `conversation_events` table, many within the same causal chain, many concurrent.

Scale to 100 concurrent teams: **thousands of event writes/sec, thousands of hybrid search queries/sec, hundreds of snapshot writes/sec** — all with causal ordering constraints.

This section describes how the data flow is designed to handle this without becoming the bottleneck.

### End-to-End Data Flow

```
User Request
  │
  ▼
┌─────────────────────────────────────────────────────────────┐
│  INGRESS                                                    │
│  Rate limit → Auth → Route to agent                         │
└──────────────────────────┬──────────────────────────────────┘
                           │
  ┌────────────────────────▼────────────────────────────────┐
  │  AGENT CHATLOOP                                         │
  │                                                         │
  │  ┌─── Context Assembly (READ path) ──────────────────┐  │
  │  │  1. Fetch causal chain events (indexed query)     │  │
  │  │  2. Hybrid memory search (vector+fulltext+SQL)    │  │
  │  │  3. Load skill definitions (cached)               │  │
  │  │  4. Assemble prompt (token budget allocation)     │  │
  │  └───────────────────────────────────────────────────┘  │
  │                         │                               │
  │                         ▼                               │
  │  ┌─── Snapshot Write ────────────────────────────────┐  │
  │  │  Record exact context BEFORE LLM call             │  │
  │  │  (async — doesn't block LLM call)                 │  │
  │  └───────────────────────────────────────────────────┘  │
  │                         │                               │
  │                         ▼                               │
  │  ┌─── LLM Call (EXTERNAL, async) ───────────────────┐  │
  │  │  Streaming response via provider                  │  │
  │  └───────────────────────────────────────────────────┘  │
  │                         │                               │
  │                         ▼                               │
  │  ┌─── Event Write (WRITE path) ─────────────────────┐  │
  │  │  Persist response event + metadata                │  │
  │  │  (batched if multi-tool-call turn)                │  │
  │  └───────────────────────────────────────────────────┘  │
  │                         │                               │
  │                         ▼                               │
  │  ┌─── Post-Chain Hooks (ASYNC, non-blocking) ───────┐  │
  │  │  Quality scoring (Python UDF, in-DB)              │  │
  │  │  Knowledge extraction → semantic memory           │  │
  │  │  Memory consolidation                             │  │
  │  └───────────────────────────────────────────────────┘  │
  └─────────────────────────────────────────────────────────┘
```

### Optimization Strategy by Path

#### Write Path: Event Ingestion

The hottest path. Every agent action produces at least one event. Under multi-agent load, this is thousands of inserts/sec into `conversation_events`.

```
Optimizations:
  │
  ├── 1. WRITE BATCHING
  │   A single agent turn often produces multiple events
  │   (tool_call + tool_result + llm_response).
  │   Batch into single transaction instead of 3 round-trips.
  │
  │   Single-agent turn: 1 transaction with 2-5 events
  │   Team turn: 1 transaction per agent (not per event)
  │
  ├── 2. ASYNC SNAPSHOT WRITES
  │   Context snapshots are large (full prompt content).
  │   Write async — the LLM call doesn't need to wait.
  │   Snapshot ID assigned synchronously, content flushed async.
  │   If crash before flush: snapshot marked incomplete (audit still works,
  │   just with "snapshot content lost" flag).
  │
  ├── 3. PARTITION BY DEPLOYMENT SCOPE
  │   In multi-tenant deployments, each account is a separate
  │   database namespace (MatrixOne Multi-Account).
  │   Tenant A's write storm doesn't contend with Tenant B's reads.
  │   Agent code is identical — isolation is infrastructure-level.
  │   In single-tenant deployments, this is a no-op.
  │
  └── 4. APPEND-ONLY DESIGN
      Events are never updated, only appended.
      No row-level locks, no update contention.
      MatrixOne's HTAP engine optimizes for append workloads.
```

#### Read Path: Context Assembly

The latency-critical path. Every LLM call requires assembling context from events + memory + skills. Under multi-agent load, N agents all reading simultaneously.

```
Optimizations:
  │
  ├── 1. CAUSAL CHAIN INDEX
  │   Most context assembly queries filter by causal_chain_id.
  │   Composite index: (causal_chain_id, created_at) covers 90% of reads.
  │   A 4-agent team has 4 causal chains — reads are naturally partitioned.
  │
  ├── 2. SKILL DEFINITION CACHE
  │   Skill definitions change rarely (versioned, explicit updates).
  │   Cache in application memory with TTL.
  │   Cache invalidation: version bump → invalidate.
  │   Eliminates ~30% of context assembly DB reads.
  │
  ├── 3. INCREMENTAL CONTEXT ASSEMBLY
  │   Don't rebuild full context from scratch every turn.
  │   Cache previous turn's context, append new events since last assembly.
  │   Only re-run hybrid search if query semantics changed significantly.
  │
  │   Turn N context = Turn N-1 context
  │                    + new events since Turn N-1
  │                    + re-ranked memory (if query changed)
  │                    - evicted tokens (budget overflow)
  │
  ├── 4. HTAP SEPARATION
  │   MatrixOne's HTAP engine routes:
  │   - Point reads (event by ID, chain by ID) → TP engine (row-store)
  │   - Analytical reads (hybrid search, aggregations) → AP engine (column-store)
  │   No application-level routing needed — the engine decides.
  │
  └── 5. READ-YOUR-OWN-WRITES GUARANTEE
      Agent writes event → immediately reads it back in next context assembly.
      MatrixOne's snapshot isolation guarantees this within a session.
      Cross-agent reads (blackboard) have slight delay — acceptable for
      event-based coordination (agents poll, not push).
```

#### Analytics Path: Quality Scoring, SLO Monitoring, Drift Detection

Background workload. Must not interfere with the interactive read/write paths.

```
Optimizations:
  │
  ├── 1. DYNAMIC TABLES (not scheduled jobs)
  │   Quality dashboards, SLO monitoring, pollution detection
  │   all defined as Dynamic Tables.
  │   MatrixOne refreshes them incrementally as source data changes.
  │   No cron jobs, no ETL, no separate analytics DB.
  │
  ├── 2. PYTHON UDF (in-DB compute)
  │   Quality scoring, PII detection, knowledge extraction
  │   run as UDFs inside the database.
  │   Data doesn't leave the engine → no serialization overhead,
  │   no network hop to an external scoring service.
  │
  └── 3. AP ENGINE ISOLATION
      Analytical queries (aggregations, scans) run on the AP engine.
      TP engine (serving interactive reads/writes) is not affected.
      This is MatrixOne's HTAP architecture — not a design choice,
      it's a database guarantee.
```

### Multi-Agent Specific: Team Data Flow

```
Team of 4 agents, 1 lead + 3 workers
  │
  ▼
Lead creates 3 task events (1 batched transaction)
  │
  ├── Worker A polls blackboard → claims task → works in own causal chain
  ├── Worker B polls blackboard → claims task → works in own causal chain
  └── Worker C polls blackboard → claims task → works in own causal chain
      │
      │  Each worker's reads/writes are isolated to their own chain.
      │  No cross-worker contention on the write path.
      │  Blackboard reads (polling for new tasks) are lightweight index scans.
      │
      ▼
Workers complete → write result events
  │
  ▼
Lead polls for completion events → assembles results → synthesizes
  │
  ▼
Total DB operations for a 3-task team round:
  - Writes: 3 (task creation) + 3 (claims) + ~30 (worker events) + 3 (results) + ~5 (lead) = ~44 events
  - Reads: ~12 context assemblies (3 workers × ~3 turns + lead × 3)
  - Hybrid searches: ~12 (one per context assembly, cached incrementally)
  - Snapshot writes: ~12 (async, non-blocking)
```

### Backpressure and Graceful Degradation

When the system approaches capacity:

```
┌─────────────────────────────────────────────────────────────┐
│  BACKPRESSURE SIGNALS                                       │
│                                                             │
│  DB write latency > 100ms (p95)                             │
│    → Increase write batch size (trade latency for throughput)│
│    → Shed P3 (speculative) agent tasks                      │
│                                                             │
│  Context assembly latency > 500ms (p95)                     │
│    → Reduce hybrid search scope (shorter time window)       │
│    → Use cached context more aggressively                   │
│    → Skip memory search for simple follow-up turns          │
│                                                             │
│  LLM provider rate limit approaching                        │
│    → Downgrade non-critical agents to cheaper model         │
│    → Queue P2/P3 tasks                                      │
│    → Merge redundant LLM calls (same context = same result) │
│                                                             │
│  Memory pressure (too many concurrent clones)               │
│    → Reject new sandbox creation                            │
│    → Force-expire idle clones (>30min no activity)          │
│    → Fall back to snapshot-based isolation (read-only)      │
│      instead of clone-based (read-write)                    │
└─────────────────────────────────────────────────────────────┘
```

### Why MatrixOne Makes This Tractable

For the **platform's own data flow** (agent state, events, memory, decisions), a traditional stack would require:
- PostgreSQL for events (TP) + ClickHouse for analytics (AP) + sync between them
- Pinecone for memory vector search + sync from PostgreSQL
- Elasticsearch for event fulltext search + sync from PostgreSQL
- Redis for caching + invalidation logic

That's 4 systems with 3 sync channels — each a consistency bug waiting to happen.

MatrixOne collapses this to **one system** with native HTAP, native vector+fulltext, and native async (Dynamic Tables). The data flow optimization is simpler because there's only one data flow to optimize.

For **user business data**, the platform is agnostic — agents can operate on any data source. But when user data is also on MatrixOne, the Enhanced Services (Sandbox, Time Travel, Branch/Diff/Merge) operate on the user's DB via a passed `db` handle, extending the same single-system benefits to the user's data without requiring migration.
