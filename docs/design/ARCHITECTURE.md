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
│                  PLATFORM CAPABILITIES (Kernel)              │
│  Memory │ Context │ Skills │ Sandbox │ Time Travel │        │
│  LLM Client │ Streaming │ Planning │ Trust Engine │         │
│  Evaluation │ Cost Control                                  │
├─────────────────────────────────────────────────────────────┤
│                    DATA LAYER (MatrixOne)                    │
│  Replaces: Vector DB · Search Engine · ETL · Package Registry│
│  Enables: Clone-Test-Merge · Hybrid Recall · Snapshot Audit  │
└─────────────────────────────────────────────────────────────┘
```

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

### 5. MatrixOne Eliminates Infrastructure Categories

The strategic insight is not "we use a good database." It's that MatrixOne's capabilities **collapse entire infrastructure categories into database operations**:

| Traditional Stack | What We Replace It With |
|---|---|
| Vector DB (Pinecone/Milvus) + sync jobs | Native VECTOR type + HNSW/IVF index — hybrid search in one query |
| Search engine (Elasticsearch) | Native FULLTEXT INDEX — combined with vector + SQL |
| Staging environment + CI/CD | CREATE CLONE → test → MERGE BRANCH or discard |
| Audit database + compliance tools | Snapshot binding — every decision traceable to exact data state |
| Package registry + distribution API | Publication → Subscription — skill marketplace as data sharing |
| ETL pipeline + data warehouse | Stage (S3) + External Table — bidirectional, zero ETL |
| Guardrail middleware | Python UDF — safety checks at the data layer |
| Real-time monitoring dashboard | Dynamic Table — auto-refreshing aggregates |
| Multi-tenant access control | Multi-Account — database-level isolation, not application-level |

Each elimination removes a system to deploy, monitor, pay for, and debug. See [data-versioning.md §6](data-versioning.md) for the concrete workflows this enables.

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
| Industry-wide: too many systems to integrate | MatrixOne eliminates 6+ infrastructure categories (vector DB, search, ETL, registry, guardrails, monitoring) |

## What This Is NOT

- **Not a chatbot.** Agents understand intent, select actions, learn from context, and reproduce decisions.
- **Not a framework.** You don't import our library. You deploy on our platform and get guarantees.
- **Not vendor-locked to one LLM.** Multi-provider routing with circuit breaker and fallback chains.
- **Not a demo.** 527 tests passing, production Docker support, structured logging, rate limiting.
