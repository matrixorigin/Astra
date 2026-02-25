# mo-agent-engine Architecture

> **Status**: Living Document — source of truth for all design decisions  
> **Last Updated**: 2026-02-21

---

## What We Are

An **Agentic Runtime** — not a framework, not a chatbot wrapper, not an OS.

- **Framework** (LangChain, CrewAI): gives you libraries. You assemble the pieces. No guarantees.
- **Coding Assistant** (Claude Code, Cursor): single-purpose tool. Runs locally, no platform services.
- **Agentic Runtime** (mo-agent): provides **execution environment + infrastructure guarantees**. Every agent on this platform automatically gets auditable decisions, versioned memory, safe experimentation, cost control, and trust verification. The agent developer writes a system prompt and picks skills. The runtime handles everything else.

Why "Runtime" and not "OS"? An OS manages hardware resources (processes, memory, filesystems). We don't. We manage **agent lifecycle resources**: context budget, memory governance, skill versioning, decision audit, cost tracking. The relationship between mo-agent and an agent is analogous to JVM and a Java program — the runtime provides managed execution with guarantees the raw environment doesn't offer.

Why "Agentic"? The runtime is not passive. It has agency of its own — it actively manages the agents running on it: implicit feedback mining → prompt auto-evolution → regression gate → activate. Memory self-curates (confidence decay, quarantine, compression). Skill selection learns from historical outcomes. The runtime is itself an agent that governs other agents.

## The Problem Space

Five problems block AI agents from production adoption:

| # | Problem | Why It's Hard |
|---|---------|---------------|
| 1 | **Decisions are black boxes** | The data the agent saw has changed. The prompt was updated. The context window is gone. No way to reconstruct. |
| 2 | **Iteration is guesswork** | No regression testing for prompt/skill changes. Teams ship and pray. | ✅ **SOLVED**: Prompt Auto-Evolution with regression gate. Implicit feedback mining → LLM diagnosis → auto-improve → gate → activate. |
| 3 | **Memory is broken** | Agents forget across sessions. Knowledge updates silently invalidate past answers. No memory lifecycle. | ✅ **SOLVED**: Episodic/semantic/procedural memory with automated governance (confidence decay, quarantine, compression). Distributed scheduling ensures multi-instance safety. |
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
│                         EDGE                                │
│  EdgeChatLoop │ Local Tools │ MCP Servers │ Permissions     │
│  Runs on user's machine. Drives agentic loop.               │
│  Executes tools locally. Syncs state to cloud.              │
├─────────────────────────────────────────────────────────────┤
│                      USER AGENTS (Apps)                     │
│  Code Review · CI Diagnosis · Data Analysis · Custom        │
│  Defined by: system_prompt + skill_set + model              │
├─────────────────────────────────────────────────────────────┤
│                    SYSTEM AGENTS (Daemons)                   │
│  Regression · Audit · Tuning · Eval                         │
│  Same execution model, elevated permissions, auto-triggered │
├─────────────────────────────────────────────────────────────┤
│                  PLATFORM SERVICES (Cloud)                   │
│  Memory │ Context │ Skills │ Planning │ Trust Engine │       │
│  LLM Client │ Streaming │ Evaluation │ Cost Control         │
├─────────────────────────────────────────────────────────────┤
│              PLATFORM STATE (Platform DB)                    │
│  Identity: users, roles, permissions                        │
│  Runtime: sessions, events, decisions, snapshots, audit     │
│  Catalog: skill_definitions, skill_permissions, models      │
│  Credentials: user_credentials (encrypted)                  │
├─────────────────────────────────────────────────────────────┤
│              SKILL DATA (sk_{skill}_{table} prefix)         │
│  sk_github_repos, sk_github_pr_cache                        │
│  sk_knowledge_entries, sk_knowledge_relations                │
│  Same DB, same Base — defined in skills/{name}/models.py    │
├─────────────────────────────────────────────────────────────┤
│          ENHANCED SERVICES (optional, MatrixOne-native)      │
│  Sandbox (clone) │ Pub/Sub (marketplace) │ Time Travel      │
│  Hybrid Search │ Branch/Diff/Merge │ Dynamic Table          │
│  Activated when user data is also on MatrixOne               │
│  Usage: pass db handle → service operates on user's DB      │
└─────────────────────────────────────────────────────────────┘
```

**Three execution layers**:

1. **Edge** — user's machine. Drives the agentic loop (EdgeChatLoop), executes local tools (file, shell, git, MCP). State syncs to cloud. See [Edge-Cloud Execution](edge-cloud-execution.md).
2. **Platform Services (Cloud)** — API server. Handles LLM calls (API key security, context assembly, memory injection, model routing, budget control, audit). Source of truth for all state. System agents run entirely here.
3. **Enhanced Services** — opt-in capabilities when running on MatrixOne (zero-copy clone, time-travel, hybrid search).

Skills are **stateful platform capabilities** with platform-defined schemas and typed API layers. Skill tables are defined in `skills/{name}/models.py` and created by `init_db()`. See [Skill-as-Package](skill-as-package.md).

Adding a new User Agent = define `AgentProfile` (system_prompt + skills + model). Zero platform code.

## Design Documents

This is the index. Each document is the **single source of truth** for its domain.

| Document | Scope |
|----------|-------|
| [Memory and Context](memory-and-context.md) | Cognitive architecture: episodic/semantic/procedural memory, context engineering, attention budget, compaction, memory lifecycle |
| [Trust and Safety](trust-and-safety.md) | Decision audit, hallucination firewall, uncertainty quantification, regression gate, observability, guardrails |
| [Skills and Tools](skills-and-tools.md) | Skill system, MCP compatibility, tool design, side-effect profiles, progressive disclosure, marketplace |
| [Skill-as-Package](skill-as-package.md) | Stateful skill architecture: platform-defined schema, install lifecycle, skill API layer, credential management, `sk_` table naming |
| [Unified Selector Pipeline](unified-selector-pipeline.md) | Skill selection: retrieve → audit → feedback pipeline |
| [Agents and Orchestration](agents-and-orchestration.md) | ChatLoop, PAOR planning, multi-agent delegation, streaming, sub-agent architecture |
| [Data Versioning](data-versioning.md) | Git for Data: time travel, sandbox, branching, cost-aware branching, training data pipeline |
| [Evaluation and Evolution](evaluation-and-evolution.md) | Quality scoring, replay gating, prompt auto-evolution, implicit feedback mining, self-improving agents, meta-learning closed loop |
| [Write Path Optimization](write-path-optimization.md) | Async event pipeline: fire-and-forget emit, background batch flush, embedding fully decoupled into `event_embeddings`, event tiering — 60x hot-path latency reduction |
| [Feedback Classification Model](feedback-classification-model.md) | Native feedback classifier: data pipeline, model training, deployment as platform skill, continuous learning |
| [Deployment Architecture](deployment-architecture.md) | Deployment topologies (single machine → K8s), edge-cloud split execution, `/chat/turn` protocol, execution backend abstraction, GPU scheduling, Ray integration |
| [Implementation Plan](implementation-plan.md) | Unified execution plan: write path optimization (A1-A5) + CLI edge-cloud architecture (B1-B5), acceptance criteria, risk register |

### Core Design (continued)

| Document | Scope |
|----------|-------|
| [Edge-Cloud Execution](edge-cloud-execution.md) | Edge-cloud split execution: `/chat/turn` protocol, skill classification (edge/cloud/hybrid), edge state model, security model, sync protocol |
| [Agent Introspection](agent-introspection.md) | Agent self-awareness: metacognition model, static/dynamic introspection, intent classification, cross-agent capability query, system prompt enrichment |
| [Prompt Lifecycle](prompt-lifecycle.md) | Prompt assembly pipeline, unified prompt path, edge-cloud tool merging, prompt versioning via time travel, prompt A/B testing via branching, self-model section |

### Supporting Documents (Implementation)

| Document | Scope |
|----------|-------|
| [Authentication](../implementation/authentication.md) | JWT, ownership-based authorization |
| [LLM Integration](../implementation/llm-integration.md) | Provider abstraction, auto-detection from DB tokens, routing, cost tracking |
| [GitHub Integration](../implementation/github-integration.md) | Repo operations, token management |
| [Deployment](../implementation/deployment.md) | Project structure, Docker, configuration |
| [Feedback Classifier Deployment](../implementation/feedback-classifier-deployment.md) | Training/inference skill isolation, ONNX export, batch processing, model artifacts |
| [Scope Configuration](../implementation/scope-configuration.md) | Scope-based config resolution |
| [CI](../implementation/ci.md) | GitHub Actions workflows |

## Key Design Decisions

### 1. Memory is a First-Class System, Not an Afterthought

Industry trend: Anthropic's context engineering, Letta/MemGPT's memory OS, EverMemOS's dual-layer architecture, Observational Memory's 95% LongMemEval score — all point to memory as **the** differentiator for production agents.

Our position: Memory is not "RAG bolted on later." It is a cognitive architecture with distinct layers (sensory → working → episodic → semantic → procedural), each with its own storage, retrieval, and lifecycle. See [Memory and Context](memory-and-context.md).

### 2. Context Engineering Over Prompt Engineering

Following Anthropic's insight: the question is not "how to write a better prompt" but "what configuration of context maximizes desired behavior." Context is a finite attention budget. Every token must earn its place.

Our implementation: task-aware budget allocation, just-in-time retrieval, compaction for long-horizon tasks, structured note-taking for cross-session persistence. See [Memory and Context](memory-and-context.md).

### 3. Skills Are Stateful Packages

Industry trend: Anthropic's Agent Skills (three-tier progressive loading), MCP as the tool protocol standard, ElizaOS plugin schemas. But no framework supports skill install lifecycle with platform-defined schemas.

Our position: Skills are **stateful platform capabilities** — the platform defines skill table schemas (deterministic, like any other model). All tables live in the same platform database, with skill business tables using `sk_{skill}_{table}` naming convention. Each skill defines its own tables in `skills/{name}/models.py`. Skills expose typed API layers for data access. This goes beyond ElizaOS (plugin-owned schema, fixed PG) and far beyond LangChain/CrewAI (stateless functions). See [Skill-as-Package](skill-as-package.md) and [Skills and Tools](skills-and-tools.md).

### 4. Trust Is Built Into the Platform, Not Bolted On

Industry trend: Decision lineage (Elixir Data), agentic observability (DataRobot), zero-trust agent architecture (Microsoft Foundry), AI guardrails as defense-in-depth.

Our position: Every decision binds to a data snapshot. Every response carries confidence signals. Every change passes a regression gate. This is not optional — it's platform infrastructure. See [Trust and Safety](trust-and-safety.md).

### 5. MatrixOne: Platform State + Optional Enhanced Services

MatrixOne serves two distinct roles:

**Role 1: Platform State Store (always)**

The platform's own state — user identity, sessions, events, skill catalog, skill business data, decisions, audit trail — all lives in a single MatrixOne instance managed by the platform operator. Skill business tables use `sk_{skill}_{table}` naming convention. This gives the platform native vector search, fulltext search, HTAP, and time-travel for all data.

**Role 2: Enhanced Services (opt-in, MatrixOne-native)**

When running on MatrixOne, the platform can offer enhanced services:

| Enhanced Service | What It Does | How It Works |
|---|---|---|
| Sandbox | Isolated experiment environment | `Sandbox(db=user_db)` → `CREATE CLONE` on user's DB |
| Time Travel | Query historical state | Snapshot binding on user's DB → exact past state |
| Hybrid Search | Vector + fulltext + SQL in one query | Only if user's tables have vector/fulltext indexes |
| Branch/Diff/Merge | Git-like data workflows | Branch user's tables → experiment → diff → merge |
| Skill Marketplace | Publish/subscribe skill definitions | `CREATE PUBLICATION` → cross-account sharing |
| Dynamic Table | Real-time derived views | Auto-refreshing aggregates on user's data |

The enhanced services are a value-add when running on MatrixOne, not a platform dependency. See [data-versioning.md §6](data-versioning.md) for the concrete workflows.

### 6. Event-Centric, Not State-Centric

All state flows through `conversation_events` with causal chain tracking. This enables replay, lineage, audit, and multi-agent coordination through a single mechanism. Events are the universal interface.

## Industry Alignment

| Industry Direction | Our Alignment |
|-------------------|---------------|
| Anthropic Agent Teams: parallel coordination, shared task board | Teams with clone-per-agent speculative execution — run 4 approaches, keep the best |
| Vercel/Anthropic Skills: composable, shareable agent capabilities | Skill-as-Package: stateful skills with schema + migrations + marketplace + RBAC |
| ElizaOS plugin schemas: plugins declare DB tables | Platform-defined schema + typed skill API + `sk_` naming — simpler and safer than plugin-owned schemas |
| RouteMoA: cost-quality model routing | Self-improving router that learns from historical quality/cost data |
| MemGPT/EverMemOS: cognitive memory architecture | Hybrid memory recall — vector + fulltext + quality in one query, self-curating |
| Braintrust/Maxim: agent evaluation, regression testing | Clone-test-merge — zero-risk evolution, regression gate as database operation |
| Microsoft zero-trust: auditable, verifiable agent decisions | Snapshot-as-ground-truth — every decision reconstructable at any future point |
| LangSmith/OpenTelemetry: async event pipeline, fire-and-forget tracing | Async EventPipeline: in-memory queue → background batch flush → bulk INSERT. Event tiering (critical/durable/ephemeral). See [Write Path Optimization](write-path-optimization.md) |
| Industry-wide: too many systems to integrate | Single platform DB with `sk_` prefix for skill data; enhanced services for MatrixOne users |

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
  ├── 1. ASYNC EVENT PIPELINE (fire-and-forget)
  │   Event writes never block the hot path.
  │   emit() enqueues in-memory (<1μs), returns immediately.
  │   Background task drains queue every 200ms, bulk INSERTs.
  │   Only 2 sync flush points per turn:
  │   (1) after user_query, for build_context to read;
  │   (2) after run status (completed/failed/cancelled), for cross-worker polling.
  │   See [Write Path Optimization](write-path-optimization.md).
  │
  │   Industry alignment: LangSmith SDK uses identical pattern —
  │   PriorityQueue + background thread + batch drain + operation merging.
  │
  ├── 2. EMBEDDING FULLY DECOUPLED
  │   Embeddings are NOT in the event write path at all.
  │   Events write to conversation_events with no embedding column.
  │   Async EmbeddingWorker generates embeddings into event_embeddings
  │   table (separate lifecycle, separate worker, separate DB session).
  │   Only user_query, llm_response, plan_created, knowledge_extracted
  │   get embeddings — stream events are never embedded.
  │   See [Write Path Optimization](write-path-optimization.md).
  │
  ├── 3. EVENT TIERING (critical / durable / ephemeral)
  │   Critical (user_query, llm_response) → conversation_events
  │   Durable (run_started, run_completed) → conversation_events
  │   Ephemeral (stream_text_delta, etc.) → run_events only
  │   No tier touches embeddings. Eliminates dual-write overhead for 60% of events.
  │   See [Write Path Optimization](write-path-optimization.md).
  │
  ├── 4. ASYNC SNAPSHOT WRITES
  │   Context snapshots are large (full prompt content).
  │   Write async — the LLM call doesn't need to wait.
  │   Snapshot ID assigned synchronously, content flushed async.
  │   If crash before flush: snapshot marked incomplete (audit still works,
  │   just with "snapshot content lost" flag).
  │
  ├── 5. PARTITION BY DEPLOYMENT SCOPE
  │   In multi-tenant deployments, each account is a separate
  │   database namespace (MatrixOne Multi-Account).
  │   Tenant A's write storm doesn't contend with Tenant B's reads.
  │   Agent code is identical — isolation is infrastructure-level.
  │   In single-tenant deployments, this is a no-op.
  │
  └── 6. APPEND-ONLY DESIGN
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
│    → Skip embedding for non-critical events (already default)│
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
