# Memory Overview

> **Status**: Core Design — shared concepts for all memory backends
> **Last Updated**: 2026-03-08
> **Scope**: Cognitive architecture, context engineering, protocol interfaces, governance model
> **Backends**: [Tabular](tabular-memory.md) (production) | [Graph](graph-memory.md) (planned)
> **Related**: [intent-driven-loading.md](intent-driven-loading.md), [backend-coexistence.md](backend-coexistence.md), [context-window-management.md](../context-window-management.md)

---

## Memory Flow Overview

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│                              MEMORY FLOW DIAGRAM                                │
└─────────────────────────────────────────────────────────────────────────────────┘

  User Input / Tool Result
         │
         ▼
  ┌─────────────┐
  │  PERCEIVE   │  Sensory buffer (in-memory)
  └──────┬──────┘
         │
         ▼
  ┌─────────────┐
  │   ENCODE    │  Event creation + metadata extraction
  └──────┬──────┘
         │
         ├──────────────────────────────────────────────────────────────┐
         │                                                              │
         ▼                                                              ▼
  ┌─────────────┐                                              ┌───────────────┐
  │    STORE    │  conversation_events (episodic)              │   OBSERVER    │
  │             │  memories table (semantic/profile/...)       │  (post-turn)  │
  └──────┬──────┘                                              └───────┬───────┘
         │                                                              │
         │                                                              ▼
         │                                                     ┌───────────────┐
         │                                                     │  SENSITIVITY  │
         │                                                     │    FILTER     │
         │                                                     │ (pre-persist) │
         │                                                     └───────┬───────┘
         │                                                              │
         │◄─────────────────────────────────────────────────────────────┘
         │
         ▼
  ┌─────────────┐
  │ CONSOLIDATE │  SessionSummarizer: incremental + full summaries
  └──────┬──────┘
         │
         ▼
  ┌─────────────┐     ┌─────────────┐     ┌─────────────┐
  │  RETRIEVE   │────▶│   SCORE &   │────▶│  ASSEMBLE   │
  │  (Hybrid)   │     │   SELECT    │     │   CONTEXT   │
  └─────────────┘     └─────────────┘     └──────┬──────┘
                                                  │
                                                  ▼
                                          ┌─────────────┐
                                          │     LLM     │
                                          │    CALL     │
                                          └──────┬──────┘
                                                  │
         ┌────────────────────────────────────────┘
         │
         ▼
  ┌─────────────┐
  │   UPDATE    │  Contradiction detection → supersede
  └──────┬──────┘
         │
         ▼
  ┌─────────────┐
  │   GOVERN    │  Decay, cleanup, quarantine, health
  └─────────────┘
  ┌─────────────┐     ┌─────────────┐     ┌─────────────┐
  │  RETRIEVE   │────▶│   SCORE &   │────▶│  ASSEMBLE   │
  │  (Hybrid)   │     │   SELECT    │     │   CONTEXT   │
  └─────────────┘     └─────────────┘     └──────┬──────┘
                                                  │
                                                  ▼
                                          ┌─────────────┐
                                          │     LLM     │
                                          │    CALL     │
                                          └──────┬──────┘
                                                  │
         ┌────────────────────────────────────────┘
         │
         ▼
  ┌─────────────┐
  │   UPDATE    │  Contradiction detection → supersede
  └──────┬──────┘
         │
         ▼
  ┌─────────────┐
  │   GOVERN    │  Decay, cleanup, quarantine, health
  └─────────────┘

  ─────────────────────────────────────────────────────────────────────────────────
  STORAGE LAYERS:
  
  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐
  │  Sensory    │  │   Working   │  │  Episodic   │  │  Semantic   │
  │  (memory)   │  │ (scratchpad)│  │  (events)   │  │ (memories)  │
  └─────────────┘  └─────────────┘  └─────────────┘  └─────────────┘
       1 turn         session         90 days          permanent
                                      + decay           + decay
```

---

## Why This Document Exists

Memory and context are the two most critical capabilities for production AI agents. Anthropic's context engineering research shows that intelligence is not the bottleneck — **context is**. Letta/MemGPT, EverMemOS, and Observational Memory demonstrate that agents with proper memory architecture dramatically outperform those without.

This document defines how astra-engine thinks about, stores, retrieves, and manages the information that agents use to make decisions.

### Memory Behaviors: Why They Matter

Before diving into architecture, it helps to think about memory the way humans
experience it — and why each behavior is critical for agents.

| Behavior | Human Analogy | Why Agents Need It | Our Mechanism | Industry |
|----------|--------------|-------------------|---------------|----------|
| **Short-term recall** | "What did you just say?" | Without it, agent forgets mid-conversation. Every turn is a cold start. | Sensory buffer (in-memory) + Working memory (`agent_scratchpad`) | All frameworks (context window) |
| **Long-term memory** | "Last month you said you prefer Go" | Without it, agent can't build relationship or accumulate knowledge across sessions. | `memories` table (semantic/profile/procedural) with confidence decay | Letta: core/archival memory. Zep, Mem0: user memory store |
| **Forgetting** | Outdated facts fade; you stop remembering old phone numbers | Without it, stale knowledge pollutes decisions. Agent confidently uses a deprecated API. | Confidence decay: `effective_confidence = initial × 0.5^(days/half_life)`. Below threshold → quarantine. | Letta: manual eviction. Most frameworks: ❌ no decay |
| **Recall** | "What was that restaurant name?" — retrieval from partial cue | Agent must find relevant memories from vague queries, not just exact match. | Hybrid retrieval: vector similarity + fulltext keyword + temporal recency + confidence weighting | Letta: embedding search. Standard RAG: vector-only |
| **Reflection** | "Looking back, those three incidents were all about the same bug" | Agent must synthesize patterns from individual experiences — not just store raw facts. | ✅ **Designed** — shared `ReflectionEngine` in `core/memory/reflection/`. Backend-agnostic: each backend provides candidates, engine does importance scoring → LLM synthesis → scene creation. See [graph-memory.md §4.3](graph-memory.md) for full design. | Generative Agents (Park et al.): reflection. Letta: ❌. Most: ❌ |
| **Contradiction resolution** | "Wait, you said X before but now Y — which is it?" | Without it, agent holds conflicting beliefs simultaneously. | Observer: L2_DISTANCE finds similar existing memories; if content differs → atomic supersede (deactivate old + insert new) | Letta: overwrite block. Most: ❌ silent conflict |
| **Memory tampering protection** | You can't secretly rewrite someone's memories | Agent memories must be auditable — no silent edits, no untracked deletions. | Immutable `source_event_ids` provenance. Supersede chain (never hard delete). PITR time-travel to verify any past state. `context_snapshot` records what agent saw. | Letta: git log. Most: ❌ no audit trail |
| **Retrospection** | "If I had known then what I know now..." | Debug bad decisions by replaying with corrected memory. | Sandbox branch → modify memories → replay session → compare outcomes. Zero-copy via MO `data branch`. | Letta: git branch. Most: ❌ |
| **Consolidation** | Sleeping on it — short-term → long-term overnight | Raw experiences must be distilled into durable knowledge, or memory grows unbounded. | Observer (per-turn extraction) → SessionSummarizer (periodic/close) → Governance (cleanup/quarantine) | EverMemOS: consolidation loop. Letta: ❌ manual. Most: ❌ |
| **Selective attention** | You remember what matters, not every detail | Agent can't stuff everything into context window. Must select the most relevant subset. | TieredLoader: L0 profile (always) + L1 retrieval (query-relevant). PromptAssembler enforces token budget. | Letta: core vs archival split. MemGPT: page in/out |

**Key insight**: Most agent frameworks implement only 2-3 of these behaviors
(short-term recall + long-term storage + basic recall). The gap between "has
memory" and "has a memory *system*" is the difference between a chatbot that
remembers your name and an agent that can detect its own knowledge is outdated,
resolve contradictions, explain why it made a past decision, and improve over time.

---

## 1. The Cognitive Architecture

Inspired by cognitive science and aligned with the latest industry research (Generative Agents, MemGPT, EverMemOS), we model agent memory as a **layered cognitive system**:

```
┌─────────────────────────────────────────────────────────────┐
│  SENSORY BUFFER                                             │
│  Raw input: user message, tool results, streaming chunks    │
│  Lifetime: single inference turn                            │
│  Storage: in-memory only                                    │
├─────────────────────────────────────────────────────────────┤
│  WORKING MEMORY (Scratchpad)                                │
│  Active reasoning state: current plan, intermediate results │
│  Lifetime: single task / causal chain                       │
│  Storage: agent_scratchpad table                            │
├─────────────────────────────────────────────────────────────┤
│  EPISODIC MEMORY                                            │
│  Past experiences: "what happened"                          │
│  User asked X, agent did Y, outcome was Z                   │
│  Lifetime: session → cross-session (with decay)             │
│  Storage: conversation_events (sole source of truth)        │
│  Retrieval: HybridRetriever via event_embeddings JOIN       │
│  Cross-session: session summaries (type=semantic)           │
├─────────────────────────────────────────────────────────────┤
│  SEMANTIC MEMORY                                            │
│  Extracted knowledge: "what is true"                        │
│  User prefers X, codebase uses pattern Y, API Z is flaky   │
│  Lifetime: long-term, evolving                              │
│  Storage: memories table (type=semantic) +                  │
│           sk_knowledge_entries (knowledge skill)            │
├─────────────────────────────────────────────────────────────┤
│  PROCEDURAL MEMORY                                          │
│  Learned behaviors: "how to do things"                      │
│  Skill selection patterns, prompt improvements, tool chains │
│  Lifetime: permanent, versioned                             │
│  Storage: memories (type=procedural) + skills_registry +    │
│           prompt_templates                                  │
│  Note: skill_selection_learnings are Skill Selector internal│
│        state, bridged via procedural_memory.py for type     │
│        unification but NOT injected into MemoryRetriever    │
└─────────────────────────────────────────────────────────────┘
```

### Layer → Storage Mapping

| Layer | Table(s) | Retriever | Index | Lifecycle |
|-------|----------|-----------|-------|-----------|
| Sensory Buffer | (in-memory only) | — | — | Discarded after inference turn |
| Working Memory | `agent_scratchpad` | Direct query by `session_id` | B-tree on `session_id`, `user_id` | Task/chain scoped; archived on completion |
| Episodic | `conversation_events` + `event_embeddings` | HybridRetriever | IVF-flat vector + fulltext on `content` | Append-only; cross-session via session summaries |
| Semantic | `memories` (type=semantic) + `sk_knowledge_entries` | MemoryRetriever | IVF-flat vector + fulltext on `content` | Confidence decay (query-time, per trust tier) |
| Procedural | `memories` (type=procedural) | MemoryRetriever | Same as semantic | Versioned; permanent |
| Profile | `memories` (type=profile) | MemoryRetriever (L0 cache via ProfileManager) | Same as semantic | Synthesized from semantic; cached |
| Tool Result | `memories` (type=tool_result) | MemoryRetriever | Same as semantic | Session-scoped; 7-day decay |

> **Architecture Decision**: Episodic memory lives exclusively in
> `conversation_events`, NOT in the `memories` table. The `memories` table stores
> only profile, semantic, procedural, working, and tool_result types.
> `conversation_events` already has causal chains, event types, full metadata, and
> async embeddings. Cross-session episodic recall is handled by session summaries
> (type=semantic, generated by SessionSummarizer) and by HybridRetriever searching
> `conversation_events` directly.

### Why Five Layers, Not Three

The common "short-term / long-term / RAG" model conflates fundamentally different types of information:

- **Episodic** ("last Tuesday you asked me to refactor auth") is different from **Semantic** ("this codebase uses dependency injection"). They have different retrieval patterns, different decay rates, and different update mechanisms.
- **Procedural** ("when the user asks about CI, check logs first") is learned behavior that should persist across all sessions and improve over time. It's not "memory" in the traditional sense — it's **skill**.
- **Working memory** is not just "recent conversation." It's the agent's active reasoning state — the current plan, hypotheses being tested, intermediate results. It must be explicitly managed, not just a sliding window.

### Memory Ownership, Isolation, and Privacy

#### Ownership Model

Memory has three scoping dimensions: **user**, **session**, and **agent**.

```
User (alice)
├── Cross-session memories (session_id = NULL)
│   ├── profile: "prefers Go, senior engineer"
│   ├── semantic: "project uses DI pattern"
│   └── procedural: "check logs before debugging"
├── Session A (session_id = "sess_01")
│   ├── working: current plan, hypotheses
│   ├── tool_result: grep output from this session
│   └── scratchpad: intermediate notes
└── Session B (session_id = "sess_02")
    └── (isolated from Session A's working state)
```

| Dimension | Key Column | Isolation Rule |
|-----------|-----------|----------------|
| **User** | `user_id` (mandatory, every query) | Hard boundary. User A never sees User B's memories. No exceptions. |
| **Session** | `session_id` (nullable) | Soft boundary. `session_id = NULL` means cross-session (profile, semantic, procedural). Session-scoped types (working, tool_result) are only visible within that session. Retriever's `include_cross_session` flag controls whether cross-session memories are included. |
| **Agent** | (not stored) | No isolation. All agents serving the same user share the same memory pool. Intentional — see below. |

#### Why No Agent-Level Isolation

Memories are **per-user, not per-agent**. When a Code Agent learns "this user
prefers Go over Python," the CI Agent should know that too. User knowledge is a
shared asset — fragmenting it by agent creates inconsistency.

Agents coordinate through the event blackboard (`conversation_events`), not
through shared memory state. See
[agents-and-orchestration.md §4](agents-and-orchestration.md#4-multi-agent-collaboration).

**Agent self-editing**: Memory mutation happens implicitly through the Observer
pipeline (extract → contradict → persist). Since memories are user-scoped, any
agent's Observer can supersede any memory for that user. There is no explicit
"self-edit" API.

#### Session Isolation Semantics

| Memory Type | `session_id` | Visibility | Rationale |
|-------------|-------------|------------|-----------|
| profile | NULL | All sessions | User identity is global |
| semantic | NULL | All sessions | Learned knowledge persists |
| procedural | NULL | All sessions | Behavioral patterns persist |
| working | Set | This session only | Active reasoning state is task-specific |
| tool_result | Set | This session only | Raw tool outputs are ephemeral |

The retriever enforces this via SQL:
- `include_cross_session=True` (default): `WHERE (session_id = :sid OR session_id IS NULL)` — sees session-local + global
- `include_cross_session=False`: `WHERE session_id = :sid` — sees only session-local

#### Sensitive Information Handling

Memory is a long-lived store — sensitive information requires explicit treatment:

| Concern | Mechanism |
|---------|-----------|
| **PII in memories** | Sensitivity filter detects and redacts PII before persistence |
| **Credential leakage** | Sensitivity filter discards credential-containing content; `tool_result` type has 7-day decay + session scope as defense-in-depth |
| **Cross-session leakage** | Session-scoped types (working, tool_result) have `session_id` set; cross-session types (profile, semantic, procedural) have `session_id=NULL`. Retriever enforces via SQL. |
| **Memory deletion (right to forget)** | `is_active = 0` soft-deletes exclude from retrieval. True erasure via PITR retention expiry (configurable) |
| **Audit trail vs privacy** | `context_snapshot` stores memory_ids, not content. Content looked up at audit time (respects current `is_active` state) |

#### Sensitivity Filter (Pre-Persist Hook)

The Observer pipeline includes a **sensitivity filter** that runs after LLM extraction
and before persistence. This is a mandatory gate — no memory bypasses classification.

```
Observer.extract_candidates()
    ↓
┌─────────────────────────────────────────────────────────────┐
│  SENSITIVITY FILTER (pre-persist hook)                      │
│                                                             │
│  For each candidate memory:                                 │
│    1. Classify: check_sensitivity(content)                  │
│       → regex patterns for email, phone, SSN, credit card,  │
│         AWS keys, private keys, bearer tokens, passwords    │
│                                                             │
│    2. Apply policy:                                         │
│       if any pattern matches:                               │
│         → BLOCK (entire memory rejected, never persisted)   │
│         → Structured audit log with content_hash            │
│       else:                                                 │
│         → PASS (memory proceeds to persistence)             │
│                                                             │
│  Design decision: block-only, no redaction. Safer than      │
│  partial redaction which risks incomplete PII removal.      │
└─────────────────────────────────────────────────────────────┘
    ↓
Observer.persist_with_contradiction_check()
```

**Classification approach**: Regex patterns only (fast, deterministic).
Small classifier model and LLM-based classification are design targets for
high-security deployments.

#### Why This Matters

1. **Trust**: Users must trust that their data stays within their boundary. `user_id` filtering is mandatory in every SQL query — there is no code path that reads memories without it.
2. **Compliance**: GDPR right-to-erasure maps to soft-delete + PITR expiry. The supersede chain provides full audit trail of what was known and when.
3. **Multi-tenancy**: The `user_id` boundary is the foundation for multi-tenant deployment. No shared memory pool across users, no cross-contamination risk.
4. **Agent safety**: Without session isolation, a compromised or hallucinating agent in one session could pollute working memory for another concurrent session. Session-scoped types prevent this.

### Memory Lifecycle

Every piece of information follows a lifecycle:

```
Perceive → Encode → Store → Consolidate → Retrieve → Update → Decay/Archive
```

| Phase | What Happens | Mechanism |
|-------|-------------|-----------|
| **Perceive** | Raw input enters sensory buffer | HTTP request, tool result, stream chunk |
| **Encode** | Extract structured information | Event creation with metadata, entity extraction |
| **Store** | Persist to appropriate layer | MatrixOne (events, knowledge); embeddings async |
| **Consolidate** | Promote, summarize, connect | Post-chain hooks: summarization, knowledge extraction, entity linking |
| **Retrieve** | Find relevant memories for current task | Hybrid search: causal chain + semantic + temporal + entity overlap |
| **Update** | Revise beliefs based on new evidence | Knowledge entry versioning, confidence decay |
| **Decay/Archive** | Remove or compress stale information | Intelligent decay based on recency × relevance × utility |

### Memory Lifecycle Governance

Decay, trust, and cleanup are not ad-hoc — they're a formal governance model with explicit policies, automated enforcement, and audit trail.

#### Retention Policy by Memory Type

| Memory Type | Default TTL | Decay Behavior | Deletion | Status |
|---|---|---|---|---|
| **Sensory** (raw stream chunks) | 1 hour | Auto-purge after consolidation into events | Hard delete (no audit need) | 🔵 Design Target |
| **Working** (active plan state) | Session lifetime | ✅ Archived by `run_hourly()` after 2h inactivity | Soft delete (queryable via time-travel) | ✅ Implemented |
| **Semantic** (knowledge entries) | No TTL (explicit lifecycle) | ✅ Query-time confidence decay with per-tier half-life | ✅ Quarantine by `run_daily()` when effective_confidence < 0.3 | ✅ Implemented |
| **Procedural** (skills, prompt templates) | No TTL (versioned) | Never auto-decay | Deprecate → version tombstone | ✅ Implemented |
| **Tool Result** | 24 hours | ✅ TTL-based cleanup by `run_hourly()` | Hard delete | ✅ Implemented |

#### Automated Confidence Decay

Knowledge entries lose confidence over time unless revalidated:

```
effective_confidence(t) = initial_confidence × decay_factor^(days_since_validation / half_life)

where:
  decay_factor = 0.5  (halves every half_life period)
  half_life = per trust tier: T1=365d, T2=180d, T3=60d, T4=30d
```

Confidence decay is **query-time only**. The `memories` table stores
`initial_confidence` (immutable after write). `effective_confidence` is computed
in every retrieval query. Governance does NOT mutate the confidence column.
This is stateless and idempotent.

Retriever computes in SQL: `initial_confidence * EXP(-TIMESTAMPDIFF(DAY, observed_at, NOW()) / :half_life)`

When effective confidence drops below retrieval threshold (default 0.3):
- Entry excluded from retrieval results
- Queued for revalidation (automated or human)
- If revalidated: confidence reset to validated level, timer restarts
- If not revalidated within grace period: quarantined

#### Source Trust Tiers

Not all information sources are equally reliable. Trust tier determines initial confidence and decay rate:

| Trust Tier | Sources | Initial Confidence | Half-Life | Verification |
|---|---|---|---|---|
| **T1: Verified** | Official docs, verified APIs, system-generated | 0.95 | 365 days | Auto-verified against source URL/API |
| **T2: Curated** | Human-reviewed, team knowledge bases | 0.85 | 180 days | Periodic human review cycle |
| **T3: Inferred** | Agent-extracted from conversations, LLM-generated summaries | 0.65 | 60 days | Cross-reference against T1/T2 sources |
| **T4: Unverified** | Raw user input, unvalidated claims | 0.40 | 30 days | Must be promoted to T3+ or decays to quarantine |

#### Governance Cycles

```
┌─────────────────────────────────────────────────────────┐
│  MEMORY GOVERNANCE ENGINE (GovernanceScheduler)         │
│                                                         │
│  run_hourly():                                          │
│    - Cleanup expired TOOL_RESULT memories (TTL-based)   │
│    - Archive stale WORKING memories (>2h inactive)      │
│                                                         │
│  run_daily(user_id):                                    │
│    - Delete inactive low-confidence memories (stale)    │
│    - Quarantine: deactivate memories where              │
│      effective_confidence < threshold (per trust tier)   │
│    - Pollution detection (supersede ratio check)        │
│                                                         │
│  run_weekly():                                          │
│    - Cleanup orphan sandbox branches                    │
│    - Cleanup old milestone snapshots (keep last N)      │
│                                                         │
│  Confidence decay is query-time only — governance       │
│  never mutates the initial_confidence column.           │
│                                                         │
│  Trust tiers (T1-T4) determine per-tier half-life:      │
│    T1=365d, T2=180d, T3=60d, T4=30d                    │
└─────────────────────────────────────────────────────────┘
```

#### Distributed Scheduling (Multi-Instance Deployment)

For production deployments with N replicas, governance tasks must run exactly once per cycle:

- `MemoryGovernanceScheduler` — façade, wires task runner + backend
- `SchedulerBackend` (abstract) — pluggable: AsyncIO (dev), Celery, Temporal, K8s CronJob
- `GovernanceTaskRunner` — executes tasks with distributed locking

Distributed lock via `distributed_locks` table (lock_name PK, instance_id, expires_at). INSERT-based acquisition with expiry-based failover.

---

## 2. Context Engineering

### The Core Principle

Following Anthropic's insight: context engineering is about finding the **smallest possible set of high-signal tokens** that maximize the likelihood of desired behavior. Context is a finite attention budget with diminishing marginal returns.

### Context Assembly Pipeline

```
User Request
    │
    ▼
┌─────────────────────────────────────────────────────────────┐
│  1. CLASSIFY: What kind of task is this?                    │
│     code_review | planning | debugging | general | ...      │
│     → Determines budget allocation strategy                 │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│  2. BUDGET: Allocate attention budget by task type           │
│     Total: model_context_limit - response_reserve            │
│     ┌──────────────────────────────────────────────────┐    │
│     │ code_review:  code 50% | history 20% | docs 20% │    │
│     │ debugging:    logs 40% | code 30% | history 20% │    │
│     │ planning:     history 50% | code 20% | docs 20% │    │
│     └──────────────────────────────────────────────────┘    │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│  3. RETRIEVE: Pull candidates from each memory layer         │
│     Working: current causal chain events                     │
│     Episodic: relevant past experiences (hybrid search)      │
│     Semantic: relevant knowledge entries                     │
│     Procedural: skill definitions, learned patterns          │
│     External: just-in-time tool calls (file reads, API)      │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│  4. SCORE & SELECT: Multi-signal relevance ranking           │
│     semantic_similarity × 0.35                               │
│     causal_proximity   × 0.25                                │
│     temporal_recency   × 0.20                                │
│     entity_overlap     × 0.10                                │
│     user_reference     × 0.10                                │
│     → Select top-K within each budget slot                   │
└────────────────────────┬────────────────────────────────────┘
```

### Prompt Layout (Cache-Optimized)

Cache-friendly layout — stable prefix maximizes prompt caching, dynamic suffix changes per turn:

```
[STABLE]  §1 Role & capabilities        ← from DB prompt_templates (cacheable)
[STABLE]  §2 Constraints & format rules  ← hardcoded behavioral rules
[DYNAMIC] §2.5 Few-shot examples         ← from high-rated feedback
[DYNAMIC] §3 Observations + prior ctx    ← cross-session continuity, observer
[DYNAMIC] §4 Working memory / scratchpad ← per-session active notes
[DYNAMIC] §5 Conversation history        ← budget-capped
```

### Context Budget Allocation

`ContextBudgetManager` allocates the available context window (model limit − output reserve) across 7 sources. Ratios vary by conversation stage:

| Source | Query | Analysis | Generation | Planning |
|--------|-------|----------|------------|----------|
| System prompt | 10% | 8% | 10% | 10% |
| History | 25% | 15% | 20% | **30%** |
| Tool output | 25% | **35%** | 20% | 20% |
| Memory L0 (profile) | 5% | 5% | 5% | 5% |
| Memory L1 (retrieval) | 15% | 12% | 10% | 15% |
| Code context | 10% | 15% | **25%** | 10% |
| Documentation | 10% | 10% | 10% | 10% |

Example: 128K context, 4K output reserve → 124K available. In `analysis` stage:
system 10K, history 19K, tool output **43K**, memory 21K, code 19K, docs 12K.

Bold values show the dominant allocation per stage — analysis prioritizes tool
output (debugging needs logs), generation prioritizes code context, planning
prioritizes history (needs full conversation arc).

**Dynamic reallocation**: Sources are filled sequentially (system prompt → history
→ tool output → memory → ...). When a source uses less than its allocation, the
remainder is redistributed to subsequent sources via `allocate_remaining()`:

```python
# ContextBudgetManager tracks usage incrementally:
mgr = ContextBudgetManager(max_context_tokens=128000, reserve_for_output=4000)
# available = 124K

alloc = mgr.allocate(stage="analysis")
# alloc.system_prompt = 9920, alloc.tool_output = 43400, ...

# System prompt only used 3K of its 9.9K allocation:
mgr.record_usage("system_prompt", 3000)

# Now allocate_remaining() distributes the remaining 121K (not 114K):
realloc = mgr.allocate_remaining(stage="analysis")
# realloc.tool_output = 42350  (35% of 121K — got bigger because system was small)
```

This ensures no budget is wasted — a short system prompt means more room for
tool output and memory. The `TurnBudgetTracker` adds a second layer for tool
outputs specifically: if cumulative tool output exceeds 50% of its allocation
and the next output is large (>2K tokens), it force-summarizes via the Tool
Context Engine.

### Just-in-Time Retrieval

Following Anthropic's Claude Code pattern: instead of pre-loading everything, maintain **lightweight references** (file paths, query templates, API endpoints) and let the agent pull data on demand via tools. This mirrors human cognition: we don't memorize entire codebases — we know where to look.

### Compaction for Long-Horizon Tasks

When context approaches the window limit:

1. **Tool result clearing**: Remove raw tool outputs deep in history
2. **Conversation compaction**: Summarize old turns, preserve recent ones and key decisions
3. **Structured note-taking**: Agent writes notes to persistent storage (working memory → episodic/semantic promotion)

**Retrieval-based history (added 2026-03-05)**: For Turn 3+, the messages array sent to LLM uses recent 2 turns (full) + relevant old turns retrieved via `HybridRetriever` from `agent_events`. This keeps prompt tokens constant (~5000-7000) regardless of session length. The full history remains in `_session_cache` for persistence and recovery. See `context-window-management.md` §2 "Retrieval-Based History" for details.

### Cross-Session Continuity

When a user returns after hours/days:

1. Load **session summary** (episodic: what happened last time)
2. Load **user knowledge** (semantic: preferences, patterns, expertise level)
3. Load **active plans** (working: any unfinished goals)
4. Agent reads its own notes and continues

> **Session summary generation**: Session summaries bridge episodic and cross-session
> recall. Raw events stay in `conversation_events` (session-scoped), but distilled
> summaries become permanent knowledge (`type=semantic`, `subtype=session_summary`,
> `session_id=NULL`).

#### Session Summary Generation Timing

Summaries are generated at multiple points to handle both short and long sessions:

| Trigger | Condition | Summary Type | Rationale |
|---------|-----------|--------------|-----------|
| **Session close** | `POST /sessions/{id}/close` | Full session summary | Natural boundary; user explicitly ends |
| **Turn threshold** | Every N turns (default: 50) | Incremental summary | Long sessions (days without close) need periodic consolidation |
| **Time threshold** | Every T hours (default: 2h) | Incremental summary | Backup for sessions with sparse but long-running activity |
| **Context overflow** | History exceeds budget | Compaction summary | Emergency consolidation to free context space |

**Incremental vs Full summaries**:

```
Session with 200 turns over 8 hours:
  Turn 50  → incremental_summary_1 (covers turns 1-50)
  Turn 100 → incremental_summary_2 (covers turns 51-100)
  2h mark  → (skipped, turn threshold already fired)
  Turn 150 → incremental_summary_3 (covers turns 101-150)
  Close    → full_summary (synthesizes all incrementals + turns 151-200)
```

**Storage**:
- Incremental: `type=semantic`, `subtype=session_incremental`, `session_id=current`
- Full: `type=semantic`, `subtype=session_summary`, `session_id=NULL` (cross-session)

Incremental summaries are session-scoped (only visible within that session) until
the full summary is generated, which supersedes them and becomes cross-session.

**Implementation**: `SessionSummarizer` is called by:
1. `SessionManager.close_session()` — full summary
2. `TurnHooks.post_turn()` — checks turn/time thresholds, generates incremental
3. `ContextBudgetManager.on_overflow()` — emergency compaction summary

---

## 9. Differentiators

### Positioning

This architecture is not a single breakthrough invention — it is an **engineering
synthesis** that combines ideas from cognitive science (layered memory model),
Generative Agents (reflection), Letta/MemGPT (structured memory management),
EverMemOS (consolidation loops), and Anthropic (context engineering). The
originality lies in the combination: ownership model + retriever separation +
snapshot audit + tool context integration + MO-native versioning, working together
as a coherent system.

### Capability Comparison

| Capability | Standard RAG | MemGPT/Letta | Us | Notes |
|-----------|-------------|-------------|-----|-------|
| Episodic memory | ❌ | ✅ | ✅ + causal chains + time-travel | |
| Semantic memory | Flat chunks | Editable core blocks | Versioned knowledge entries with provenance | |
| Procedural memory | ❌ | ❌ | ✅ Skill learnings, prompt evolution | |
| Memory versioning | ❌ | ✅ Git-based context repos | ✅ MO-native: PITR, snapshot, zero-copy branch | Row-level vs file-level |
| Memory rollback | ❌ | ✅ `git revert` | ✅ `restore from pitr` (row-level, sub-second) | |
| Agent self-edit memory | ❌ | ✅ Agent commits to git repo | ✅ Observer auto-extracts + supersedes | Letta: explicit; Us: implicit + explicit |
| Multi-agent shared memory | ❌ | ✅ Shared context repos | ✅ Per-user memory pool (all agents share by `user_id`) | Different model: Letta shares repos; we share by user |
| Memory audit | ❌ | Git log (commit-level) | ✅ Every retrieval in context_snapshot + provenance | |
| Memory experimentation | ❌ | Git branch (full copy) | ✅ Zero-copy branch → sandbox replay → merge | MO branch = no storage overhead |
| Automated decay | ❌ | ❌ Manual eviction | ✅ Query-time `effective_confidence` | |
| Automated consolidation | ❌ | ❌ Manual | ✅ Observer → SessionSummarizer → Governance pipeline | |
| Contradiction detection | ❌ | Overwrite block | ✅ DB-side L2_DISTANCE → atomic supersede | |
| Cross-session continuity | Vector search only | Archival search | Session summaries (auto-generated by SessionSummarizer) + knowledge entries | |
| Vector + Fulltext + SQL | 3 separate systems | External vector DB | MO native (fulltext only in WHERE — 3-phase workaround) | |
| Vector time-travel | ❌ | ❌ | ✅ Snapshot restores vector indexes too | |
| Tool context integration | ❌ | ❌ | ✅ TOOL_RESULT type + rule-based summary + memory_read | |

### Letta Context Repositories: Respect and Differentiation

Letta's Context Repositories (Feb 2026) introduced a compelling model: agents
use git to actively manage their own memory — commit, branch, revert, share repos
across agents. This is a genuine innovation in agent self-management.

**What Letta does well that we should acknowledge**:
- Agents have an explicit mental model of "saving" their memory state
- Git semantics are intuitive and well-understood
- Shared repos give multi-agent memory sharing with familiar access control
- Agent-initiated versioning puts the agent in control

**What we can do that Letta can do**:

| Letta Capability | Our Equivalent |
|-----------------|----------------|
| Agent commits memory | Observer auto-extracts; agent can also explicitly write via memory API |
| Agent branches memory | `data branch create table memories` — zero-copy |
| Agent reverts memory | `restore from pitr` — any timestamp, row-level |
| Shared memory repos | Per-user memory pool — all agents share automatically |
| Git log for audit | `context_snapshot` + `source_event_ids` + supersede chain |

**What we can do that Letta cannot**:

| Capability | Why Letta Can't | Our Mechanism |
|-----------|----------------|---------------|
| **Row-level rollback** | Git reverts entire files | MO PITR operates on individual rows |
| **Transactional consistency** | Git commits are manual checkpoints | MO PITR respects MVCC — captures in-flight state |
| **Zero-cost branching** | Git copies working tree | MO `data branch` is copy-on-write at storage layer |
| **Vector index time-travel** | Git has no concept of vector indexes | MO snapshot restores IVF-flat atomically |
| **Automated governance** | Agent must manually manage lifecycle | Observer + SessionSummarizer + Governance run automatically |
| **Automated contradiction detection** | Agent must notice conflicts | DB-side L2_DISTANCE finds contradictions on every write |
| **Query-time decay** | No decay model | `effective_confidence` computed in every retrieval |
| **Retrieval audit** | Git log shows writes, not reads | `context_snapshot` records exactly what was retrieved and scored |
| **Sandbox replay** | Can branch, but no replay infrastructure | Branch → modify → replay session → compare outcomes |
| **Tool output management** | No tool context integration | TOOL_RESULT type + rule-based summary + memory_read |

**The fundamental difference**: Letta's model is **agent-driven** (agent decides
when to save/load). Ours is **system-driven** (governance runs automatically) with
agent override capability. For audit and compliance use cases, system-driven is
strictly superior — you can't rely on the agent to maintain its own audit trail.
For creative/exploratory use cases, Letta's explicit control may feel more natural.

### The Audit Advantage

Because every memory retrieval is recorded in `context_snapshot`, we can answer:

- "Why did the agent forget about our auth discussion?" → Check which events were retrieved/excluded
- "When did the agent learn this wrong fact?" → Trace provenance to source events
- "Would the agent have made a different decision with better memory?" → Replay in sandbox

---

## 11. Module Independence & Interface Design

### Current State

The memory module is **architecturally isolated** — it has only two external dependencies:

| Dependency | Purpose | Files |
|---|---|---|
| `core.db_consumer` (DbFactory/DbConsumer) | Database access abstraction | 9 |
| `api.models.memory` (MemoryRecord) | SQLAlchemy ORM model | 3 |

It does **not** depend on core.context, core.events, core.llm, core.agent, core.skills, or core.embedding. The dependency graph is strictly one-directional: other modules depend on memory, never the reverse.

### Problem: Consumers Bypass Abstraction

Despite clean internal architecture, external consumers import internal classes directly:

```
core/context/prompt_assembler.py  → imports TieredMemoryLoader
core/agent/chat_loop.py           → imports MemoryStore, MemoryRetriever
api/routers/chat.py               → imports TypedObserver
api/routers/sessions.py           → imports MemoryStore, SessionSummarizer
core/context/scheduler.py         → imports GovernanceScheduler
skills/knowledge/                 → imports TypedObserver, trust_tier_defaults
```

This treats internal implementation as public API — any refactoring of memory internals breaks consumers.

### Problem: TieredMemoryLoader Is a Consumer, Not a Provider

`TieredMemoryLoader` decides "how much memory to load into the prompt based on memory_mode". This is **context assembly logic** (a consumer concern), not memory's core capability (store/retrieve/govern). It currently lives in `core/memory/` but belongs in `core/context/`.

### Target: Protocol-Based Interface

```
┌──────────────────────────────────────────────────────┐
│                Memory Module (independent)            │
│                                                      │
│  Public Interface (Protocol):                        │
│    MemoryReader  — retrieve(), get_profile()         │
│    MemoryWriter  — store(), observe_turn()           │
│    MemoryAdmin   — run_governance(), health_check()  │
│                                                      │
│  Facade:                                             │
│    MemoryService — single entry point for consumers  │
│                                                      │
│  Internal (not exposed):                             │
│    MemoryStore, MemoryRetriever, ProfileManager      │
│    GovernanceScheduler, TypedObserver, etc.           │
└──────────────────────┬───────────────────────────────┘
                       │ Protocol interface only
        ┌──────────────┼──────────────┐
        ▼              ▼              ▼
   Context Module   Agent Module   API Layer
   (prompt assembly) (chat loop)   (routers)
```

### Interface Definition

```python
# core/memory/interfaces.py — canonical signatures (see source file for full docstrings)

class MemoryReader(Protocol):
    def retrieve(self, user_id: str, query: str, *,
                 session_id: str = "",
                 query_embedding: list[float] | None = None,
                 memory_types: list[MemoryType] | None = None,
                 top_k: int = 10,
                 task_hint: str | None = None,
                 weights: RetrievalWeights | None = None,
                 include_cross_session: bool = True,
                 ) -> list[Memory]: ...
    def get_profile(self, user_id: str) -> str | None: ...

class MemoryWriter(Protocol):
    def store(self, user_id: str, content: str, *,
              memory_type: MemoryType,
              source_event_ids: list[str] | None = None,
              initial_confidence: float = 0.75,
              trust_tier: TrustTier | None = None,
              session_id: str | None = None,
              ) -> Memory: ...
    def observe_turn(self, user_id: str, messages: list[dict[str, Any]], *,
                     source_event_ids: list[str] | None = None,
                     ) -> list[Memory]: ...

class MemoryAdmin(Protocol):
    def run_governance(self, user_id: str) -> GovernanceReport: ...
    def health_check(self, user_id: str) -> HealthReport: ...
```

### Migration: TieredMemoryLoader → Context Module

TieredMemoryLoader moves to `core/context/` and consumes memory through `MemoryReader`:

```
Router → context_plan.memory_mode → TieredMemoryLoader (in core/context/)
    → calls MemoryService.reader.retrieve() / .get_profile()
    → assembles prompt section
```

Memory module has no knowledge of memory_mode, router, or prompt assembly. Intent-driven memory loading (see [intent-driven-loading.md](intent-driven-loading.md)) becomes a **context-layer consumption strategy**, not a memory-internal change.

### Migration Plan

| Step | Change | Risk |
|---|---|---|
| 1 | Add `core/memory/interfaces.py` with Protocol definitions | None — additive |
| 2 | Add `MemoryService` facade implementing the Protocols | None — additive |
| 3 | Move `TieredMemoryLoader` to `core/context/tiered_loader.py` | Medium — update imports |
| 4 | Migrate consumers to use `MemoryService` instead of direct imports | Medium — incremental |
| 5 | Mark internal classes as `_internal` or remove from `__init__.py` | Low — after step 4 |

Step 1-2 are prerequisites for intent-driven memory loading (Phase 2 in [intent-driven-loading.md](intent-driven-loading.md)).

---

## References

- [Anthropic: Effective Context Engineering for AI Agents](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
- [Anthropic: Equipping Agents with Agent Skills](https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills)
- [Memory Systems from Cognitive Neuroscience to Autonomous Agents](https://arxiviq.substack.com/p/ai-meets-brain-memory-systems-from)
- [Skywork: Why AI Agent Memory Systems Matter](https://skywork.ai/blog/ai-agent/why-ai-agent-memory-systems/)
- [EverMemOS: Dual-Layer Memory Architecture](https://www.bastillepost.com/global/article/5583424)
- [OpenAI: State Management with Long-Term Memory Notes](https://developers.openai.com/cookbook/examples/agents_sdk/context_personalization/)

Content was rephrased for compliance with licensing restrictions.
