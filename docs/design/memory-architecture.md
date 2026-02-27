# Memory Architecture

> **Status**: Core Design — single source of truth for memory architecture  
> **Last Updated**: 2026-02-27  
> **Scope**: Conceptual architecture, design decisions, and design-level specifications  
> **Implementation**: See [memory-system-status.md](../implementation/memory-system-status.md) for engineering status, module mapping, and known issues

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
  │ CONSOLIDATE │  Reflector: cluster → promote → session summaries
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

This document defines how mo-agent-engine thinks about, stores, retrieves, and manages the information that agents use to make decisions.

### Memory Behaviors: Why They Matter

Before diving into architecture, it helps to think about memory the way humans
experience it — and why each behavior is critical for agents.

| Behavior | Human Analogy | Why Agents Need It | Our Mechanism | Industry |
|----------|--------------|-------------------|---------------|----------|
| **Short-term recall** | "What did you just say?" | Without it, agent forgets mid-conversation. Every turn is a cold start. | Sensory buffer (in-memory) + Working memory (`agent_scratchpad`) | All frameworks (context window) |
| **Long-term memory** | "Last month you said you prefer Go" | Without it, agent can't build relationship or accumulate knowledge across sessions. | `memories` table (semantic/profile/procedural) with confidence decay | Letta: core/archival memory. Zep, Mem0: user memory store |
| **Forgetting** | Outdated facts fade; you stop remembering old phone numbers | Without it, stale knowledge pollutes decisions. Agent confidently uses a deprecated API. | Confidence decay: `effective_confidence = initial × 0.5^(days/half_life)`. Below threshold → quarantine. | Letta: manual eviction. Most frameworks: ❌ no decay |
| **Recall** | "What was that restaurant name?" — retrieval from partial cue | Agent must find relevant memories from vague queries, not just exact match. | Hybrid retrieval: vector similarity + fulltext keyword + temporal recency + confidence weighting | Letta: embedding search. Standard RAG: vector-only |
| **Reflection** | "Looking back, those three incidents were all about the same bug" | Agent must synthesize patterns from individual experiences — not just store raw facts. | Reflector: clusters similar episodic events → promotes to semantic memory via LLM condensation | Generative Agents (Park et al.): reflection. Letta: ❌. Most: ❌ |
| **Contradiction resolution** | "Wait, you said X before but now Y — which is it?" | Without it, agent holds conflicting beliefs simultaneously. | Observer: L2_DISTANCE finds similar existing memories; if content differs → atomic supersede (deactivate old + insert new) | Letta: overwrite block. Most: ❌ silent conflict |
| **Memory tampering protection** | You can't secretly rewrite someone's memories | Agent memories must be auditable — no silent edits, no untracked deletions. | Immutable `source_event_ids` provenance. Supersede chain (never hard delete). PITR time-travel to verify any past state. `context_snapshot` records what agent saw. | Letta: git log. Most: ❌ no audit trail |
| **Retrospection** | "If I had known then what I know now..." | Debug bad decisions by replaying with corrected memory. | Sandbox branch → modify memories → replay session → compare outcomes. Zero-copy via MO `data branch`. | Letta: git branch. Most: ❌ |
| **Consolidation** | Sleeping on it — short-term → long-term overnight | Raw experiences must be distilled into durable knowledge, or memory grows unbounded. | Observer (per-turn extraction) → Reflector (periodic clustering/promotion) → Governance (decay/cleanup) | EverMemOS: consolidation loop. Letta: ❌ manual. Most: ❌ |
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
│  Storage: skills_registry + prompt_templates + learnings    │
└─────────────────────────────────────────────────────────────┘
```

### Layer → Storage Mapping

| Layer | Table(s) | Retriever | Index | Lifecycle |
|-------|----------|-----------|-------|-----------|
| Sensory Buffer | (in-memory only) | — | — | Discarded after inference turn |
| Working Memory | `agent_scratchpad` | Direct query by `session_id` | B-tree on `session_id`, `user_id` | Task/chain scoped; archived on completion |
| Episodic | `conversation_events` + `event_embeddings` | HybridRetriever | IVF-flat vector + fulltext on `content` | Append-only; cross-session via session summaries |
| Semantic | `memories` (type=semantic) + `sk_knowledge_entries` | MemoryRetriever | IVF-flat vector + fulltext on `content` | Confidence decay; Reflector promotes from events |
| Procedural | `memories` (type=procedural) + `skills_registry` | MemoryRetriever | Same as semantic | Versioned; permanent |
| Profile | `memories` (type=profile) | MemoryRetriever (L0 cache via ProfileManager) | Same as semantic | Synthesized from semantic; cached |
| Tool Result | `memories` (type=tool_result) | MemoryRetriever | Same as semantic | Session-scoped; 7-day decay |

> **Architecture Decision**: Episodic memory lives exclusively in
> `conversation_events`, NOT in the `memories` table. The `memories` table stores
> only profile, semantic, procedural, working, and tool_result types.
> `conversation_events` already has causal chains, event types, full metadata, and
> async embeddings. Cross-session episodic recall is handled by session summaries
> (type=semantic) and by HybridRetriever searching `conversation_events` directly.
> The Reflector promotes patterns from events to semantic memories, skipping the
> episodic intermediate state.

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
| **Cross-session leakage** | Sensitivity filter forces `session_id` on session-specific content, preventing cross-session promotion by Reflector |
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
│    1. Classify: sensitivity_classifier(content)             │
│       → returns {has_pii, is_credential, is_session_only}   │
│                                                             │
│    2. Apply policy:                                         │
│       if is_credential:                                     │
│         → DISCARD (never enters memories table)             │
│       if has_pii:                                           │
│         → content = redact_pii(content)                     │
│       if is_session_only:                                   │
│         → force session_id = current_session_id             │
│           (prevents cross-session promotion by Reflector)   │
│                                                             │
│  All actions logged to governance_events (auditable)        │
└─────────────────────────────────────────────────────────────┘
    ↓
Observer.persist_with_contradiction_check()
```

**Classification approach** (configurable, defense-in-depth):

| Method | Speed | Accuracy | Use Case |
|--------|-------|----------|----------|
| Regex patterns | <1ms | Medium | Known formats: API keys, SSNs, emails, credit cards |
| Small classifier model | ~10ms | High | PII detection, sensitivity scoring |
| LLM (same call as extraction) | 0ms marginal | Highest | Add `sensitivity` field to extraction prompt |

Default: regex first (fast reject), then small model for ambiguous cases.
LLM-based classification is opt-in for high-security deployments.

**Redaction strategy**:
- Replace with type placeholder: `<EMAIL>`, `<API_KEY>`, `<SSN>`
- Preserve semantic meaning: "user's email is <EMAIL>" still useful for recall
- Original content never written to `memories` — redaction is irreversible

**Session-only enforcement**:
- Memories marked `is_session_only` get `session_id = current_session_id`
- Reflector skips these during episodic→semantic promotion
- Examples: "user is debugging a production incident right now", temporary credentials context

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

| Memory Type | Default TTL | Decay Behavior | Deletion |
|---|---|---|---|
| **Sensory** (raw stream chunks) | 1 hour | Auto-purge after consolidation into events | Hard delete (no audit need) |
| **Working** (active plan state) | Session lifetime | Archived on session close | Soft delete (queryable via time-travel) |
| **Episodic** (session summaries, events) | 90 days active | Compress: full events → summary after TTL | Never hard delete (audit requirement) |
| **Semantic** (knowledge entries) | No TTL (explicit lifecycle) | Confidence decay over time (see below) | Quarantine → archive (never hard delete) |
| **Procedural** (skills, prompt templates) | No TTL (versioned) | Never auto-decay | Deprecate → version tombstone |

#### Automated Confidence Decay

Knowledge entries lose confidence over time unless revalidated:

```
effective_confidence(t) = initial_confidence × decay_factor^(days_since_validation / half_life)

where:
  decay_factor = 0.5  (halves every half_life period)
  half_life = 30 days (single value for now; trust tiers deferred)
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
│  MEMORY GOVERNANCE ENGINE (runs continuously)           │
│                                                         │
│  Every hour:                                            │
│    - Purge expired sensory buffer entries                │
│    - Archive closed working memory                      │
│                                                         │
│  Every day:                                             │
│    - Quarantine entries below confidence threshold       │
│    - Compress episodic events past TTL → summaries      │
│    - Flag T4 entries approaching decay deadline          │
│                                                         │
│  Every week:                                            │
│    - T1 auto-verification: re-fetch source URLs         │
│    - Contradiction scan: semantically similar entries    │
│      with conflicting claims                            │
│    - Generate memory health report per user             │
│                                                         │
│  All actions logged as governance_events (auditable)    │
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

**Implementation**: `Reflector.generate_session_summary()` is called by:
1. `SessionManager.close_session()` — full summary
2. `TurnHooks.post_turn()` — checks turn/time thresholds, generates incremental
3. `ContextBudgetManager.on_overflow()` — emergency compaction summary

---

## 3. Memory Storage Design

### Episodic Memory: conversation_events

The existing event system IS episodic memory. Every interaction is an atomic event with causal chain tracking.

```sql
conversation_events:
  event_id, user_id, session_id, agent_id,
  event_type, content, metadata,
  parent_event_id, causal_chain_id,
  context_snapshot, token_usage,
  llm_model_used, llm_params,
  quality_score, confidence_score,
  created_at
```

Retrieval: by session (recent), by causal chain (thread), by user (cross-session), by semantic similarity (via `event_embeddings` JOIN).

### Semantic Memory: knowledge_entries

> `sk_knowledge_entries` and `sk_knowledge_relations` are part of the **knowledge skill**. See [skill-as-package.md](skill-as-package.md).

```sql
CREATE TABLE knowledge_entries (
  entry_id        VARCHAR(64) PRIMARY KEY,
  user_id         VARCHAR(64) NOT NULL,
  agent_id        VARCHAR(64),
  category        VARCHAR(50) NOT NULL,  -- 'user_preference' | 'codebase_pattern' | 'domain_fact' | 'tool_behavior' | 'entity'
  key             VARCHAR(255) NOT NULL,
  value           TEXT NOT NULL,
  extraction_method VARCHAR(50),
  confidence      DECIMAL(3,2) DEFAULT 1.0,
  last_accessed_at TIMESTAMP,
  access_count    INT DEFAULT 0,
  version         INT DEFAULT 1,
  superseded_by   VARCHAR(64),
  embedding       VECF64(1536),
  created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE knowledge_entry_sources (
  entry_id  VARCHAR(64) NOT NULL,
  event_id  VARCHAR(64) NOT NULL,
  PRIMARY KEY (entry_id, event_id)
);
```

### Procedural Memory: skills_registry + prompt_templates + selector_learnings

Procedural memory is **how the agent has learned to behave**: versioned skill definitions, versioned system prompts, and patterns learned from skill selection failures.

### Working Memory: Structured Notes

```sql
CREATE TABLE agent_scratchpad (
  note_id         VARCHAR(64) PRIMARY KEY,
  session_id      VARCHAR(64) NOT NULL,
  user_id         VARCHAR(64) NOT NULL,
  agent_id        VARCHAR(64),
  note_type       VARCHAR(50) NOT NULL,  -- 'plan' | 'hypothesis' | 'finding' | 'todo' | 'decision'
  content         TEXT NOT NULL,
  status          VARCHAR(20) DEFAULT 'active',
  related_event_ids JSON,
  related_note_ids  JSON,
  created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
```

### Unified memories Table

The design sections above describe a multi-table schema for pedagogical clarity.
The actual system uses a **unified `memories` table** with a `memory_type` enum
to distinguish all memory layers. This reduces JOIN complexity and lets all memory
types share the same CRUD, vector index, fulltext index, and governance lifecycle.

```sql
CREATE TABLE memories (
  memory_id        VARCHAR(64) PRIMARY KEY,
  user_id          VARCHAR(64) NOT NULL,
  session_id       VARCHAR(64),           -- NULL = cross-session
  memory_type      VARCHAR(20) NOT NULL,   -- profile/semantic/procedural/working/tool_result
  content          TEXT NOT NULL,
  initial_confidence FLOAT DEFAULT 0.75,
  embedding        VECF32(1536),
  source_event_ids JSON DEFAULT '[]',
  superseded_by    VARCHAR(64),
  is_active        SMALLINT DEFAULT 1,
  observed_at      DATETIME NOT NULL,
  created_at       DATETIME DEFAULT NOW(),
  updated_at       DATETIME DEFAULT NOW()
);
```

> **Note on `episodic`**: The `MemoryType.EPISODIC` enum value exists for backward
> compatibility, but no new episodic rows are written to `memories`. Episodic
> memory is served exclusively from `conversation_events` via HybridRetriever.

---

## 4. Retrieval Architecture

### Hybrid Search

| Memory Layer | Primary Retrieval | Secondary Retrieval |
|-------------|-------------------|---------------------|
| Working | Causal chain (exact) | Recency |
| Episodic | Semantic similarity | Temporal + causal proximity |
| Semantic | Key lookup + semantic search | Category filter + confidence ranking |
| Procedural | Skill matching (rule + LLM) | Historical success rate |

### MatrixOne-Native Retrieval (No External Vector DB)

We do NOT use an external vector database. MatrixOne natively supports VECTOR type, IVF-flat indexes, fulltext search, and hybrid search. All memory retrieval happens in SQL.

### Two Retriever Architecture (Intentional Separation)

The system uses two retrievers with **explicitly separated responsibilities**.

| Retriever | Data Source | Responsibility |
|-----------|-------------|----------------|
| MemoryRetriever | `memories` table | **Knowledge retrieval**: profile, semantic, procedural, tool_result |
| HybridRetriever | `conversation_events` + `event_embeddings` + `sk_knowledge_entries` | **Episodic retrieval**: what happened, causal chains, raw history |

**Boundary rule**: MemoryRetriever never touches `conversation_events`.
HybridRetriever never touches `memories`. If information needs to be found by
both, it belongs in only one store — decide which at write time.

**MemoryRetriever** — 3-phase hybrid retrieval:

```
Phase 1 (SQL): Keyword filter (MATCH in WHERE) + temporal/confidence scoring
Phase 2 (SQL): L2_DISTANCE vector nearest-neighbor search (when embedding provided)
Phase 3 (App): Merge + re-rank by weighted 4-dim score:
               vector_sim × w_vec + keyword_match × w_kw + temporal × w_time + confidence × w_conf
```

MO fulltext limitation: `MATCH() AGAINST()` can only be used in `WHERE` (boolean filter), not in `SELECT` (arithmetic scoring). Keyword is a binary signal; 4-dim merge happens application-side.

### Reference SQL

```sql
-- ASPIRATIONAL: single-query hybrid scoring (not currently possible in MO)
SELECT e.event_id, e.content,
  (0.35 * l2_distance(emb.embedding, @query_vec) +
   0.25 * MATCH(e.content) AGAINST(@query_text IN NATURAL LANGUAGE MODE) +
   0.20 * EXP(-TIMESTAMPDIFF(HOUR, e.created_at, NOW()) / 24.0)
  ) AS relevance
FROM conversation_events e
JOIN event_embeddings emb ON e.event_id = emb.event_id
WHERE e.user_id = @user_id
  AND e.created_at > NOW() - INTERVAL 30 DAY
ORDER BY relevance DESC LIMIT @top_k;

-- Semantic memory retrieval: vector + fulltext + confidence
SELECT entry_id, key, value, confidence,
  l2_distance(embedding, @query_vec) AS vec_score,
  MATCH(value) AGAINST(@query_text IN BOOLEAN MODE) AS ft_score
FROM knowledge_entries
WHERE user_id = @user_id AND confidence > 0.3
ORDER BY (0.5 * vec_score + 0.3 * ft_score + 0.2 * confidence) DESC
LIMIT @top_k;
```

### HybridRetriever Scoring Formula

The actual implementation uses a 3-phase approach with explicit score normalization:

```python
# Phase 1: Keyword + temporal/confidence (SQL)
# Phase 2: Vector nearest-neighbor (SQL)
# Phase 3: Merge + re-rank (Python)

def compute_final_score(candidate, query_embedding, weights):
    """
    4-dimensional scoring with normalization.
    
    weights = {
        'vector': 0.35,      # semantic relevance
        'keyword': 0.25,     # exact term match
        'temporal': 0.20,    # recency
        'confidence': 0.20   # source reliability
    }
    """
    # Vector similarity: L2 distance → similarity (0-1)
    # L2 distance range: 0 (identical) to ~2 (orthogonal for normalized vectors)
    l2_dist = l2_distance(candidate.embedding, query_embedding)
    vector_sim = 1.0 / (1.0 + l2_dist)  # sigmoid-like normalization
    
    # Keyword match: binary (MO limitation)
    # 1.0 if MATCH() returned this row, 0.0 otherwise
    keyword_match = 1.0 if candidate.from_keyword_phase else 0.0
    
    # Temporal recency: exponential decay
    # Half-life = 24 hours for episodic, 7 days for semantic
    hours_ago = (now() - candidate.created_at).total_seconds() / 3600
    half_life_hours = 24 if candidate.type == 'episodic' else 168
    temporal_score = math.exp(-hours_ago / half_life_hours)
    
    # Confidence: effective_confidence (already decayed)
    # Range: 0.0 to 1.0
    confidence_score = candidate.effective_confidence
    
    # Weighted sum (weights sum to 1.0)
    final_score = (
        weights['vector'] * vector_sim +
        weights['keyword'] * keyword_match +
        weights['temporal'] * temporal_score +
        weights['confidence'] * confidence_score
    )
    
    return final_score  # Range: 0.0 to 1.0

# Merge candidates from both phases, dedupe by memory_id, sort by final_score
```

**Score interpretation**:
- `> 0.7`: High relevance — include in context
- `0.4 - 0.7`: Medium relevance — include if budget allows
- `< 0.4`: Low relevance — exclude unless explicitly requested

**Weight tuning**: Weights are configurable per retriever instance. Defaults
optimized for general-purpose retrieval. Code-heavy tasks may increase `vector`
weight; debugging tasks may increase `temporal` weight.

### Why MatrixOne-Native Matters

- **No sync problem**: Knowledge embeddings live in the same row as the data
- **Transactional consistency**: Vector search respects MVCC
- **Time-travel for vectors**: `RESTORE SNAPSHOT` restores embeddings too
- **One less system**: No Pinecone/Milvus to deploy, monitor, pay for

### Python UDF for In-Database Intelligence (Design Target)

### Python UDF for In-Database Intelligence

MatrixOne's Python UDF enables pushing computation to the data:

### Reproducibility

`context_snapshot.retrieved_chunks` stores `[{chunk_id, text_hash, similarity_score, embedding_model_id}]`. On replay, verify via text_hash. If embedding model changed, inject historical chunks directly from snapshot.

---

## 5. Observational Memory

Inspired by Mastra's Observational Memory (95% on LongMemEval), we implement two background agents as "subconscious":

### Observer

Runs post-turn. Extracts typed memories (profile/semantic/procedural) from conversation turns via LLM.

- **Typed extraction**: LLM returns `[{type, content, confidence}]` → each becomes a Memory record
- **Contradiction detection**: DB-side L2_DISTANCE finds semantically similar existing memories; if content differs → atomic supersede
- **No in-memory fallback**: contradiction detection requires DB vector search; no silent degradation

### Reflector

Runs during governance cycle. Promotes clusters of similar episodic memories to semantic memories.

- **Clustering**: finds groups of ≥3 similar memories
- **LLM condensation**: synthesizes cluster into one semantic memory
- **Atomic promotion**: deactivate all cluster members + insert semantic in single transaction

Clustering uses DB-side L2_DISTANCE nearest-neighbor queries — O(n) DB queries via IVF-flat index instead of O(n²) Python comparisons.

### Memory Pipeline

```
Phase 1: Observer.extract_candidates() — LLM extraction (NOT persisted)
Phase 2: MemorySandbox.validate_memories() — zero-copy branch comparison (optional)
Phase 3: Observer.persist_with_contradiction_check() — store with supersede
Phase 4: Reflector.reflect() — episodic→semantic promotion
```

---

## 6. Memory Hygiene: Pollution Detection and Cleanup

### The Problem

A bad memory entry doesn't just produce one bad answer — it gets retrieved repeatedly, influences future decisions, and those decisions may themselves become memories. Left unchecked, a single poisoned entry can corrupt an entire knowledge domain.

Sources: user injection, hallucination crystallization, stale knowledge, duplicate/contradictory entries.

### Detection Signals

- Retrieved often but leads to low-quality decisions (via context_snapshot → quality_score)
- Contradicts other entries on same topic (semantic near-duplicates with different content)
- Age without revalidation

### Cleanup Actions

- **LOW** (stale): Mark for revalidation, reduce retrieval weight
- **MEDIUM** (contradictions): Quarantine, surface for human resolution
- **HIGH** (confirmed downstream harm): Quarantine immediately, trace affected decisions, alert admin

### Cascade Impact Analysis

When a polluted entry is quarantined, trace its blast radius:

1. **Provenance tracing**: `knowledge_entry_sources` → which sessions created this knowledge?
2. **Retrieval tracing**: `context_snapshots` → which decisions consumed this knowledge?

If affected decisions themselves became memory entries (hallucination crystallization chain), quarantine those too — recursive contamination graph.

---

## 7. Context Snapshot: The Debugging Weapon

Every LLM call produces a snapshot of exactly what the model saw, stored BEFORE the call:

```json
{
  "snapshot_id": "snap_01HX...",
  "prompt_template_id": "code_review@v3",
  "skills_included": [...],
  "episodic_events": [{"event_id": "...", "relevance_score": 0.92}],
  "semantic_entries": [{"entry_id": "...", "key": "repo.auth_pattern"}],
  "retrieved_chunks": [{"chunk_id": "...", "text_hash": "sha256:abc...", "similarity": 0.91}],
  "token_budget": {"total": 8000, "system_skills": {...}, "semantic_memory": {...}},
  "assembly_time_ms": 45
}
```

Use cases: hallucination debugging, A/B testing context selection, performance tracking, compliance audit.

---

## 8. Tool Context Engine (Context Overflow Prevention)

Large tool outputs (grep, shell) are the primary cause of context overflow.

```
Tool Output → Size Check → [>10KB] → Store as TOOL_RESULT memory
                                          ↓
                                   Rule-based Summary (zero LLM cost)
                                          ↓
                              Return: Summary + [memory:xxx]
                                          ↓
                              LLM can request full via memory_read
```

| Metric | Before | After |
|--------|--------|-------|
| Single tool output | 30KB | ~500B (summary) |
| 3x grep accumulated | 90KB | ~1.5KB |
| Info retention | ~30% | 100% (stored in Memory) |
| Summary cost | $0 | $0 (rule-based) |

See [context-overflow-optimization.md](context-overflow-optimization.md) for full design.

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
| Automated consolidation | ❌ | ❌ Manual | ✅ Observer → Reflector → Governance pipeline | |
| Contradiction detection | ❌ | Overwrite block | ✅ DB-side L2_DISTANCE → atomic supersede | |
| Cross-session continuity | Vector search only | Archival search | Session summaries (auto-generated by Reflector) + knowledge entries | |
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
| **Automated governance** | Agent must manually manage lifecycle | Observer + Reflector + Governance run automatically |
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

## 10. Open Research Directions

### Knowledge Graphs for Semantic Memory

✅ **Implemented**: `knowledge_relations` table provides entity-relationship layer over `knowledge_entries`. 1-hop expansion wired into HybridRetriever.

### Predictive Context Loading

Pre-compute likely next queries based on conversation flow. Pre-load relevant memories before the user asks.

### LLM-Native KV Cache Optimization

Separate static context (system prompt, skill definitions) from dynamic context (history, current task) to maximize provider-side KV cache hits. Can reduce cost by 90% for cached tokens.

---

## References

- [Anthropic: Effective Context Engineering for AI Agents](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
- [Anthropic: Equipping Agents with Agent Skills](https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills)
- [Memory Systems from Cognitive Neuroscience to Autonomous Agents](https://arxiviq.substack.com/p/ai-meets-brain-memory-systems-from)
- [Skywork: Why AI Agent Memory Systems Matter](https://skywork.ai/blog/ai-agent/why-ai-agent-memory-systems/)
- [EverMemOS: Dual-Layer Memory Architecture](https://www.bastillepost.com/global/article/5583424)
- [OpenAI: State Management with Long-Term Memory Notes](https://developers.openai.com/cookbook/examples/agents_sdk/context_personalization/)

Content was rephrased for compliance with licensing restrictions.
