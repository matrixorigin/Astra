# Memory and Context

> **Status**: Core Design — single source of truth for memory and context architecture  
> **Last Updated**: 2026-02-27

---

## Implementation Status (v2 Memory System)

The new MatrixOne-native memory system is implemented in `core/memory/`:

| Component | Status | Module |
|-----------|--------|--------|
| **Memory Types** | ✅ Implemented | `types.py` — MemoryType enum, Memory dataclass |
| **Memory Store** | ✅ Implemented | `store.py` — CRUD + atomic supersede |
| **Memory Model** | ✅ Implemented | `api/models/memory.py` — MemoryRecord with vector + fulltext |
| **Hybrid Retriever** | ✅ Implemented | `retriever.py` — L2_DISTANCE + MATCH AGAINST + temporal + confidence |
| **Typed Observer** | ✅ Implemented | `typed_observer.py` — typed extraction + contradiction detection |
| **Typed Reflector** | ✅ Implemented | `typed_reflector.py` — episodic→semantic promotion |
| **Profile Manager** | ✅ Implemented | `profile.py` — L0 profile synthesis + caching |
| **Memory Sandbox** | ✅ Implemented | `sandbox.py` — zero-copy branch validation |
| **Provenance** | ✅ Implemented | `provenance.py` — PITR queries, diff, rollback |
| **Health** | ✅ Implemented | `health.py` — pollution detection, cleanup, orphan branch cleanup |
| **Governance** | ✅ Implemented | `governance.py` — GovernanceScheduler with decay, cleanup, health |
| **Metrics** | ✅ Implemented | `metrics.py` — latency/counter tracking, `/api/v1/evaluation/memory-metrics` |
| **Session Isolation** | ✅ Implemented | `session_id` column, retriever supports session filtering |
| **Tiered Loader** | ✅ Implemented | `tiered_loader.py` — L0+L1 for PromptAssembler |
| **Pipeline** | ✅ Implemented | `typed_pipeline.py` — observe→sandbox→reflect |
| **Config** | ✅ Implemented | `config.py` — MemoryGovernanceConfig |

### MatrixOne-Native Capabilities Used

- **PITR**: `create pitr`, `{timestamp = '...'}` reads, `restore from pitr`
- **Snapshot**: `create snapshot`, `{snapshot = '...'}` reads, `restore from snapshot`
- **Branch**: `data branch create table`, `data branch diff`, `data branch merge`, `data branch delete table`
- **Vector**: `L2_DISTANCE()` in SQL
- **Fulltext**: `MATCH() AGAINST()` with NGRAM parser
- **HTAP**: Real-time analytics on transactional data

### Legacy System (v1) — Removed

The old system (`Observer` → `Observation` model, `SessionContinuity`) has been removed. All code now uses the v2 typed memory system.

---

## Why This Document Exists

Memory and context are the two most critical capabilities for production AI agents. Anthropic's context engineering research shows that intelligence is not the bottleneck — **context is**. Letta/MemGPT, EverMemOS, and Observational Memory demonstrate that agents with proper memory architecture dramatically outperform those without.

This document defines how mo-agent-engine thinks about, stores, retrieves, and manages the information that agents use to make decisions.

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
│  Storage: conversation_events (current chain)               │
├─────────────────────────────────────────────────────────────┤
│  EPISODIC MEMORY                                            │
│  Past experiences: "what happened"                          │
│  User asked X, agent did Y, outcome was Z                   │
│  Lifetime: session → cross-session (with decay)             │
│  Storage: conversation_events + session_summaries           │
├─────────────────────────────────────────────────────────────┤
│  SEMANTIC MEMORY                                            │
│  Extracted knowledge: "what is true"                        │
│  User prefers X, codebase uses pattern Y, API Z is flaky   │
│  Lifetime: long-term, evolving                              │
│  Storage: sk_knowledge_entries + vector store (platform DB)  │
├─────────────────────────────────────────────────────────────┤
│  PROCEDURAL MEMORY                                          │
│  Learned behaviors: "how to do things"                      │
│  Skill selection patterns, prompt improvements, tool chains │
│  Lifetime: permanent, versioned                             │
│  Storage: skills_registry + prompt_templates + learnings    │
└─────────────────────────────────────────────────────────────┘
```

### Why Five Layers, Not Three

The common "short-term / long-term / RAG" model conflates fundamentally different types of information:

- **Episodic** ("last Tuesday you asked me to refactor auth") is different from **Semantic** ("this codebase uses dependency injection"). They have different retrieval patterns, different decay rates, and different update mechanisms.
- **Procedural** ("when the user asks about CI, check logs first") is learned behavior that should persist across all sessions and improve over time. It's not "memory" in the traditional sense — it's **skill**.
- **Working memory** is not just "recent conversation." It's the agent's active reasoning state — the current plan, hypotheses being tested, intermediate results. It must be explicitly managed, not just a sliding window.

### Memory Lifecycle

Every piece of information follows a lifecycle:

```
Perceive → Encode → Store → Consolidate → Retrieve → Update → Decay/Archive
```

| Phase | What Happens | Mechanism |
|-------|-------------|-----------|
| **Perceive** | Raw input enters sensory buffer | HTTP request, tool result, stream chunk |
| **Encode** | Extract structured information | Event creation with metadata, entity extraction |
| **Store** | Persist to appropriate layer | MatrixOne (events, knowledge); embeddings async into `event_embeddings` |
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
confidence(t) = initial_confidence × decay_factor^(days_since_validation / half_life)

where:
  decay_factor = 0.5  (halves every half_life period)
  half_life = varies by source trust tier (see below)
```

When `confidence(t)` drops below retrieval threshold (default 0.3):
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

```sql
-- Source trust enforcement in retrieval
SELECT entry_id, content, 
  confidence * POWER(0.5, DATEDIFF(NOW(), last_validated_at) / half_life_days) AS effective_confidence
FROM knowledge_entries
WHERE effective_confidence > 0.3  -- retrieval threshold
ORDER BY effective_confidence DESC;
```

#### Automated Enforcement

```
┌─────────────────────────────────────────────────────────┐
│  MEMORY GOVERNANCE ENGINE (runs continuously)           │
│                                                         │
│  Every hour:                                            │
│    - Purge expired sensory buffer entries                │
│    - Archive closed working memory                      │
│                                                         │
│  Every day:                                             │
│    - Recalculate effective_confidence for all entries    │
│    - Quarantine entries below threshold                  │
│    - Compress episodic events past TTL → summaries      │
│    - Flag T4 entries approaching decay deadline          │
│                                                         │
│  Every week:                                            │
│    - T1 auto-verification: re-fetch source URLs,        │
│      compare against stored content                     │
│    - Contradiction scan: find semantically similar       │
│      entries with conflicting claims                    │
│    - Generate memory health report per user               │
│                                                         │
│  All actions logged as governance_events (auditable)    │
└─────────────────────────────────────────────────────────┘
```

This prevents memory bloat while preserving audit trail integrity. Hard deletes only happen for transient data (sensory buffer). Everything else is quarantined or archived — always recoverable via time-travel.

#### Distributed Scheduling (Multi-Instance Deployment)

For production deployments with N replicas, governance tasks must run exactly once per cycle across all instances:

**Architecture:**
- `MemoryGovernanceScheduler` — façade, wires task runner + backend
- `SchedulerBackend` (abstract) — pluggable: AsyncIO (dev), Celery, Temporal, K8s CronJob, etc.
- `GovernanceTaskRunner` — executes tasks with distributed locking

**Distributed Lock Mechanism:**
```
distributed_locks table:
  lock_name (PK)      — "governance_hourly" | "governance_daily" | "governance_weekly"
  instance_id         — "hostname:pid" (unique per instance)
  acquired_at         — when lock was taken
  expires_at          — heartbeat timeout (5 min default)
  task_name           — "hourly" | "daily" | "weekly"

Lock acquisition:
  1. Try INSERT new lock (lock_name is PK, duplicate fails)
  2. If INSERT fails, check if existing lock expired
  3. If expired: UPDATE to take over (instance crashed)
  4. If not expired: SKIP (another instance holds it)

Lock release:
  DELETE lock after task completes
```

**Guarantees:**
| Scenario | Behavior |
|---|---|
| 3 instances start simultaneously | Only 1 acquires lock; others skip |
| Instance A crashes mid-task | Lock expires after 5 min; instance B takes over |
| Instance A completes normally | Lock deleted immediately; next instance can acquire |

**Usage:**
```python
# Default (AsyncIO, single-process):
scheduler = MemoryGovernanceScheduler()
await scheduler.start()

# Custom backend (e.g., Celery):
runner = GovernanceTaskRunner(get_db_context)
backend = CeleryBackend(runner)  # you implement this
scheduler = MemoryGovernanceScheduler(backend=backend)
await scheduler.start()
```

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
                         │

### Prompt Assembly: 5-Section Layout (Implemented)

**File**: `core/agent/chat_loop.py` — `_build_messages()`

Cache-friendly layout — stable prefix maximizes prompt caching, dynamic suffix changes per turn:

```
[STABLE]  §1 Role & capabilities        ← from DB prompt_templates (cacheable)
[STABLE]  §2 Constraints & format rules  ← hardcoded behavioral rules
[DYNAMIC] §2.5 Few-shot examples         ← from high-rated feedback (FewShotRetriever)
[DYNAMIC] §3 Observations + prior ctx    ← cross-session continuity, observer
[DYNAMIC] §4 Working memory / scratchpad ← per-session active notes
[DYNAMIC] §5 Conversation history        ← budget-capped from token_budget.history.allocated
```

**Prompt caching**: §1-§2 are stable across turns → cached by DeepSeek/Anthropic, amortized cost. §2.5-§5 change per turn → only these tokens billed.

**Dynamic few-shot** (`core/context/few_shot.py`): Retrieves high-rated (≥4) examples from `llm_feedback` JOIN `conversation_events`. Keyword overlap scoring selects most relevant examples for current query.

**Budget control**: §5 history section respects `context.token_budget.history.allocated` — older turns dropped when budget exceeded.

### Just-in-Time Retrieval

Following Anthropic's Claude Code pattern: instead of pre-loading everything, maintain **lightweight references** (file paths, query templates, API endpoints) and let the agent pull data on demand via tools.

```
Instead of:  Load entire codebase into context
Do this:     Give agent file tree + grep/read tools → agent explores progressively
```

This mirrors human cognition: we don't memorize entire codebases. We know where to look and how to search.

### Compaction for Long-Horizon Tasks

When context approaches the window limit:

1. **Tool result clearing**: Remove raw tool outputs deep in history (the agent already processed them)
2. **Conversation compaction**: Summarize old turns, preserve recent ones and key decisions
3. **Structured note-taking**: Agent writes notes to persistent storage (working memory → episodic/semantic promotion)

```python
# Compaction preserves:
# - Architectural decisions and rationale
# - Unresolved issues and current hypotheses
# - Key data points and measurements
# - The 5 most recently accessed files/resources

# Compaction discards:
# - Raw tool outputs already processed
# - Redundant conversation turns
# - Superseded plans and abandoned approaches
```

### Cross-Session Continuity

When a user returns after hours/days:

1. Load **session summary** (episodic: what happened last time)
2. Load **user knowledge** (semantic: preferences, patterns, expertise level)
3. Load **active plans** (working: any unfinished goals)
4. Agent reads its own notes and continues

This is the "structured note-taking" pattern from Anthropic, extended with our cognitive architecture.

---

## 3. Memory Storage Design

### Episodic Memory: conversation_events

The existing event system IS episodic memory. Every interaction is an atomic event with causal chain tracking.

```sql
-- Core episodic storage (already implemented)
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

> **Data ownership note**: `sk_knowledge_entries` and `sk_knowledge_relations` are part of the **knowledge skill** — their schema is platform-defined, tables live in the platform database with `sk_` prefix. Defined in `skills/knowledge/models.py`. Users access data through the knowledge skill API. See [skill-as-package.md](skill-as-package.md).

Extracted, structured knowledge that persists across sessions:

```sql
CREATE TABLE knowledge_entries (
  entry_id        VARCHAR(64) PRIMARY KEY,
  user_id         VARCHAR(64) NOT NULL,
  agent_id        VARCHAR(64),
  
  -- What
  category        VARCHAR(50) NOT NULL,  -- 'user_preference' | 'codebase_pattern' | 
                                         -- 'domain_fact' | 'tool_behavior' | 'entity'
  key             VARCHAR(255) NOT NULL, -- e.g. "user.preferred_language", "repo.auth_pattern"
  value           TEXT NOT NULL,         -- The knowledge itself
  
  -- Provenance: see knowledge_entry_sources table below
  extraction_method VARCHAR(50),         -- 'llm_extraction' | 'user_explicit' | 'observation'
  
  -- Lifecycle
  confidence      DECIMAL(3,2) DEFAULT 1.0,  -- Decays over time, boosted by reconfirmation
  last_accessed_at TIMESTAMP,
  access_count    INT DEFAULT 0,
  
  -- Versioning
  version         INT DEFAULT 1,
  superseded_by   VARCHAR(64),           -- Points to newer version if updated
  
  -- Search (native MatrixOne vector + fulltext)
  embedding       VECF64(1536),          -- Native vector column, no external DB
  
  created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_user_category (user_id, category),
  INDEX idx_key (key),
  INDEX idx_confidence (confidence DESC)
);

-- Provenance: which events directly produced each knowledge entry.
-- Only the events the extractor actually read are recorded — not the entire
-- causal chain.  This keeps the table small and queries index-friendly.
-- Rows are append-only: reinforcement adds new source events to existing entries.
-- Composite PK (entry_id, event_id) provides natural dedup — concurrent
-- INSERT IGNORE / ON DUPLICATE KEY from multiple extractors is safe.
CREATE TABLE knowledge_entry_sources (
  entry_id  VARCHAR(64) NOT NULL,
  event_id  VARCHAR(64) NOT NULL,
  PRIMARY KEY (entry_id, event_id),
  INDEX idx_event (event_id)
);

-- Vector index for semantic search
CREATE INDEX idx_knowledge_vec USING HNSW ON knowledge_entries(embedding);
-- Fulltext index for keyword search
CREATE FULLTEXT INDEX idx_knowledge_ft ON knowledge_entries(value);
```

**Knowledge extraction** happens in post-chain hooks:

```
After each causal chain completes:
  1. If chain contains user preference signals → extract to semantic memory
     "I prefer TypeScript" → {category: "user_preference", key: "language", value: "typescript"}
  2. If chain reveals codebase patterns → extract to semantic memory
     Agent discovers DI pattern → {category: "codebase_pattern", key: "auth.pattern", value: "dependency_injection"}
  3. If chain produces reusable facts → extract to semantic memory
     "The staging API is at api.staging.example.com" → {category: "domain_fact", ...}
```

### Procedural Memory: skills_registry + prompt_templates + selector_learnings

Already implemented across multiple tables. Procedural memory is **how the agent has learned to behave**:

- `skills_registry`: versioned skill definitions and code
- `prompt_templates`: versioned system prompts
- `selector_learnings`: patterns learned from skill selection failures (SelfImprovingSelector)

### Working Memory: Structured Notes

For long-horizon tasks, the agent maintains a scratchpad:

```sql
CREATE TABLE agent_scratchpad (
  note_id         VARCHAR(64) PRIMARY KEY,
  session_id      VARCHAR(64) NOT NULL,
  user_id         VARCHAR(64) NOT NULL,
  agent_id        VARCHAR(64),
  
  note_type       VARCHAR(50) NOT NULL,  -- 'plan' | 'hypothesis' | 'finding' | 'todo' | 'decision'
  content         TEXT NOT NULL,
  
  -- Lifecycle
  status          VARCHAR(20) DEFAULT 'active',  -- 'active' | 'completed' | 'superseded'
  
  -- Linkage
  related_event_ids JSON,                -- Events that produced/consumed this note
  related_note_ids  JSON,                -- Other notes this connects to
  
  created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_session (session_id, status),
  INDEX idx_user (user_id, note_type)
);
```

The agent reads and writes notes as a **tool** — just like Claude Code's CLAUDE.md pattern. Notes survive compaction and session boundaries.

---

## 4. Retrieval Architecture

### Hybrid Search

No single retrieval method works for all memory types:

| Memory Layer | Primary Retrieval | Secondary Retrieval |
|-------------|-------------------|---------------------|
| Working | Causal chain (exact) | Recency |
| Episodic | Semantic similarity | Temporal + causal proximity |
| Semantic | Key lookup + semantic search | Category filter + confidence ranking |
| Procedural | Skill matching (rule + LLM) | Historical success rate |

### MatrixOne-Native Retrieval (No External Vector DB)

**Critical design decision**: We do NOT use an external vector database. MatrixOne natively supports VECTOR type, IVF/HNSW indexes, fulltext search, and hybrid search. All memory retrieval happens in a single SQL query — no Pinecone, no Milvus, no sync headaches.

> **Note**: `sk_knowledge_entries` and `conversation_events` are both in the platform database. See [skill-as-package.md](skill-as-package.md) for the table naming convention.

```sql
-- knowledge_entries stores embeddings directly
ALTER TABLE knowledge_entries ADD COLUMN embedding VECF64(1536);
CREATE INDEX idx_knowledge_vec USING HNSW ON knowledge_entries(embedding);
CREATE FULLTEXT INDEX idx_knowledge_ft ON knowledge_entries(value);

-- conversation_events: NO embedding column. Events are pure fact records.
-- Embeddings live in event_embeddings table (async generation, separate lifecycle).
-- See write-path-optimization.md for rationale.
ALTER TABLE conversation_events DROP COLUMN IF EXISTS embedding;

-- event_embeddings: async-generated, separate table for vector search
-- Narrow table = better HNSW index cache hit rate
-- Can be regenerated when embedding model changes
CREATE TABLE IF NOT EXISTS event_embeddings (
    event_id VARCHAR(36) PRIMARY KEY,
    embedding VECF32(1536),
    model_name VARCHAR(50),
    model_version VARCHAR(32),
    created_at DATETIME DEFAULT NOW()
);
CREATE INDEX idx_embeddings_vec USING HNSW ON event_embeddings(embedding);
CREATE FULLTEXT INDEX idx_events_ft ON conversation_events(content);
```

### Hybrid Search: The Killer Query

MatrixOne's unique capability: **vector + fulltext + SQL filters in a single query**. No other agent platform can do this without stitching together 3 different systems.

```sql
-- Episodic memory retrieval: one query, three signals
-- JOIN event_embeddings for vector search (embedding decoupled from events)
SELECT e.event_id, e.content,
  (0.35 * l2_distance(emb.embedding, @query_vec) +
   0.25 * MATCH(e.content) AGAINST(@query_text IN NATURAL LANGUAGE MODE) +
   0.20 * EXP(-TIMESTAMPDIFF(HOUR, e.created_at, NOW()) / 24.0)
  ) AS relevance
FROM conversation_events e
JOIN event_embeddings emb ON e.event_id = emb.event_id
WHERE e.user_id = @user_id
  AND e.created_at > NOW() - INTERVAL 30 DAY
ORDER BY relevance DESC
LIMIT @top_k;

-- Semantic memory retrieval: vector + fulltext + confidence filter
SELECT entry_id, key, value, confidence,
  l2_distance(embedding, @query_vec) AS vec_score,
  MATCH(value) AGAINST(@query_text IN BOOLEAN MODE) AS ft_score
FROM knowledge_entries
WHERE user_id = @user_id
  AND confidence > 0.3
ORDER BY (0.5 * vec_score + 0.3 * ft_score + 0.2 * confidence) DESC
LIMIT @top_k;
```

**Why this matters** (for `knowledge_entries`):
- **No sync problem**: Knowledge embeddings live in the same row as the data they describe. No eventual consistency between vector DB and relational DB.
- **Transactional consistency**: Vector search respects MVCC — snapshot isolation means replay sees the exact same vectors.
- **Time-travel for vectors**: `RESTORE SNAPSHOT` restores embeddings too. Replay a past decision with the exact vector index state.
- **One less system**: No Pinecone/Milvus to deploy, monitor, pay for, or debug.

**Note on `event_embeddings`**: Event embeddings are in a separate table, generated asynchronously by `EmbeddingWorker`. This means event vector search has eventual consistency (typically <1s lag). This is acceptable because the current turn's query is never a search target — only historical events are searched, and those already have embeddings. See [write-path-optimization.md](write-path-optimization.md#deferred-embedding-strategy).

### Python UDF for In-Database Intelligence

MatrixOne's Python UDF enables pushing computation to the data:

```sql
-- Quality scoring as a UDF — runs inside the database
CREATE FUNCTION quality_auto_score(content TEXT, expected_format TEXT)
RETURNS JSON
LANGUAGE PYTHON AS $$
def quality_auto_score(content, expected_format):
    import json
    scores = {
        "format_valid": 1 if validate_format(content, expected_format) else 0,
        "reasonable_length": 1 if 50 < len(content.split()) < 2000 else 0,
    }
    return json.dumps(scores)
$$;

-- Apply to all new events automatically via DYNAMIC TABLE
CREATE DYNAMIC TABLE event_quality_scores AS
SELECT event_id, quality_auto_score(content, 'markdown') AS auto_scores
FROM conversation_events
WHERE quality_score IS NULL;
```

### Reproducibility

`context_snapshot.retrieved_chunks` stores `[{chunk_id, text_hash, similarity_score, embedding_model_id}]`. On replay, verify via text_hash. If embedding model changed, inject historical chunks directly from snapshot (skip re-retrieval). Because MatrixOne snapshots include vector indexes, replay can use the exact same retrieval state.

---

## 5. Memory as a Differentiator

### What We Do That Others Don't

| Capability | Standard RAG | MemGPT/Letta | Us |
|-----------|-------------|-------------|-----|
| Episodic memory | ❌ | ✅ | ✅ + causal chains + time-travel |
| Semantic memory | Flat chunks | Editable core blocks | Versioned knowledge entries with provenance |
| Procedural memory | ❌ | ❌ | ✅ Skill learnings, prompt evolution |
| Memory audit | ❌ | ❌ | ✅ Every retrieval recorded in context_snapshot |
| Memory experimentation | ❌ | ❌ | ✅ Fix memory in sandbox, replay to verify |
| Memory decay | ❌ | Manual eviction | Governed lifecycle: TTL per type, confidence decay, source trust tiers, automated enforcement |
| Cross-session continuity | Vector search only | Archival search | Structured notes + session summaries + knowledge entries |
| Vector + Fulltext + SQL | 3 separate systems | External vector DB | ✅ Single MatrixOne query — hybrid search native |
| Vector time-travel | ❌ | ❌ | ✅ Snapshot restores vector indexes too |
| In-DB intelligence | ❌ | ❌ | ✅ Python UDF for scoring, extraction, validation |

### The Audit Advantage

Because every memory retrieval is recorded in `context_snapshot`, we can answer questions no other platform can:

- "Why did the agent forget about our auth discussion?" → Check which episodic events were retrieved and which were excluded, with scores
- "When did the agent learn this wrong fact?" → Trace knowledge_entry provenance to source events
- "Would the agent have made a different decision with better memory?" → Replay in sandbox with modified knowledge entries

---

## 6. Context Snapshot: The Debugging Weapon

Every LLM call produces a snapshot of exactly what the model saw. This is stored BEFORE the call, making it the ground truth for debugging and audit.

```json
{
  "snapshot_id": "snap_01HX...",
  "prompt_template_id": "code_review@v3",
  "routing_reason": "active_latest",
  
  "skills_included": [
    {"id": "code_read", "version": "1.2.0", "tokens": 120},
    {"id": "code_diff", "version": "1.0.0", "tokens": 95}
  ],
  "skills_excluded": [
    {"id": "deploy_k8s", "reason": "no_keyword_match", "version": "1.0.0"}
  ],
  "skill_filter_method": "keyword",
  
  "episodic_events": [
    {"event_id": "evt_01...", "relevance_score": 0.92, "source": "current_chain"},
    {"event_id": "evt_02...", "relevance_score": 0.78, "source": "semantic_search"}
  ],
  "semantic_entries": [
    {"entry_id": "ke_01...", "key": "repo.auth_pattern", "relevance_score": 0.85}
  ],
  "retrieved_chunks": [
    {"chunk_id": "ch_01...", "text_hash": "sha256:abc...", "similarity": 0.91, "model": "text-embedding-3-small"}
  ],
  
  "token_budget": {
    "total": 8000,
    "system_skills": {"allocated": 1200, "used": 1050},
    "semantic_memory": {"allocated": 1500, "used": 1200},
    "episodic_history": {"allocated": 3000, "used": 2800},
    "current_task": {"allocated": 1800, "used": 950},
    "reserve": 500
  },
  "assembly_time_ms": 45,
  "task_type": "code_review"
}
```

**Use cases**:
- Hallucination debugging: "What did the LLM see when it hallucinated?"
- A/B testing: Compare context selection algorithms
- Performance: Track token usage and assembly time
- Compliance: Prove what information was used for a decision

---

## 7. Memory Hygiene: Pollution Detection and Systematic Cleanup

### The Problem

Memory is a long-lived, self-reinforcing system. A bad memory entry doesn't just produce one bad answer — it gets retrieved repeatedly, influences future decisions, and those decisions may themselves become memories. Left unchecked, a single poisoned entry can corrupt an entire knowledge domain through cascading retrieval.

Sources of pollution:
- **User injection**: user deliberately inserts false "facts" into conversation
- **Hallucination crystallization**: agent hallucinates → low-quality response stored → retrieved as "knowledge" in future sessions
- **Stale knowledge**: once-true facts that are now outdated (API changed, policy updated)
- **Duplicate/contradictory entries**: same concept stored multiple times with conflicting content

### Pollution Detection (Continuous)

```sql
-- Dynamic table: auto-refreshing pollution candidates
CREATE DYNAMIC TABLE memory_pollution_candidates AS
SELECT
  ke.entry_id,
  ke.content,
  ke.source,
  ke.created_at,
  ke.retrieval_count,
  -- Signal 1: retrieved often but leads to low-quality decisions
  AVG(ce.quality_score) AS avg_downstream_quality,
  -- Signal 2: contradicts other entries on same topic
  COUNT(DISTINCT ke2.entry_id) AS contradicting_entries,
  -- Signal 3: age without revalidation
  DATEDIFF(NOW(), ke.last_validated_at) AS days_since_validation
FROM knowledge_entries ke
LEFT JOIN context_snapshots cs ON cs.snapshot_data LIKE CONCAT('%', ke.entry_id, '%')
LEFT JOIN conversation_events ce ON ce.snapshot_id = cs.snapshot_id
LEFT JOIN knowledge_entries ke2
  ON ke2.topic = ke.topic
  AND ke2.entry_id != ke.entry_id
  AND l2_distance(ke.embedding, ke2.embedding) < 0.3  -- semantically similar
GROUP BY ke.entry_id
HAVING avg_downstream_quality < 2.5           -- leads to bad decisions
    OR contradicting_entries > 2               -- multiple contradictions
    OR days_since_validation > 90;             -- stale
```

This runs continuously as a Dynamic Table — pollution candidates surface automatically without scheduled jobs.

### Cleanup Actions

```
Pollution candidate detected
  │
  ▼
Severity classification:
  │
  ├── LOW (stale, no downstream harm)
  │   → Mark for revalidation
  │   → Reduce retrieval weight (decay factor)
  │   → Queue for human review if retrieval_count > threshold
  │
  ├── MEDIUM (contradictions exist)
  │   → Quarantine: exclude from retrieval but don't delete
  │   → Surface contradicting entries to human for resolution
  │   → Log quarantine event (auditable)
  │
  └── HIGH (confirmed downstream harm)
      → Quarantine immediately
      → Identify affected decisions (via context_snapshots)
      → Flag affected decisions for re-evaluation
      → Alert admin
```

### Cascade Impact Analysis

When a polluted entry is quarantined, trace its blast radius via two complementary paths:

**Path 1: Provenance tracing** (precise, uses `knowledge_entry_sources` relation table):

```sql
-- Direct index JOIN — no JSON parsing, no full-table scan
SELECT COUNT(DISTINCT ce.session_id) AS session_count,
       COUNT(DISTINCT ce.event_id) AS decision_count
FROM knowledge_entry_sources kes
JOIN conversation_events ce ON kes.event_id = ce.event_id
WHERE kes.entry_id = @quarantined_entry_id;
```

**Path 2: Retrieval tracing** (broader, catches indirect usage via context snapshots):

```sql
-- Find decisions whose context snapshot included this entry
SELECT ce.event_id, ce.content, ce.quality_score, ce.created_at
FROM conversation_events ce
JOIN context_snapshots cs ON ce.snapshot_id = cs.snapshot_id
WHERE cs.snapshot_data LIKE CONCAT('%', @quarantined_entry_id, '%')
ORDER BY ce.created_at DESC;
```

Path 1 answers "which sessions created this knowledge?" Path 2 answers "which decisions consumed this knowledge?" Together they define the full blast radius.

If any of those decisions themselves became memory entries (hallucination crystallization chain), quarantine those too. This is recursive — the system traces the full contamination graph.

**Integration with memory pipeline:**

The memory pipeline (`run_memory_pipeline`) runs Phase 3 (PollutionDetector) which quarantines entries. Phase 4 feeds quarantined entries with severity ≥ high into `KnowledgeRegression.detect_knowledge_change_impact()` to quantify the blast radius. The pipeline result includes `regression_signals` count so callers know if quarantine had downstream impact.

### Proactive Hygiene

- **Revalidation cycle**: knowledge entries older than N days are re-scored by Python UDF against current context
- **Contradiction detection on write**: before inserting a new knowledge entry, check for semantic near-duplicates; if found, flag for merge or resolution
- **Source trust scoring**: entries from `source = 'user_input'` start with lower trust than `source = 'verified_documentation'`; trust score influences retrieval ranking

---

## 8. Observational Memory (Implemented)

Inspired by Mastra's Observational Memory (95% on LongMemEval), we implement two background agents as "subconscious":

### Observer (`core/memory/observer.py`)

Runs post-turn in a daemon thread. Extracts structured observations from new (unobserved) messages via LLM.

- **DB-backed tracking**: `observed_msg_index` column on `observations` table — survives restarts, multi-instance safe
- **Gating**: only triggers when unobserved messages exceed token threshold (default 2000)
- **Marker rows**: when LLM returns no observations, writes `is_reflected=1` marker to advance index — prevents repeated LLM calls
- **No shared mutable state**: background thread creates its own DB session + Observer instance
- **Robust JSON parsing**: `_parse_json_array()` handles bare JSON, code blocks, garbage-wrapped output

### Reflector (`core/memory/reflector.py`)

Runs hourly via `MemoryGovernanceEngine`. Condenses accumulated observations when they exceed token threshold (default 8000).

- **Transaction-safe**: mark old as reflected + insert condensed in single commit, rollback on failure
- **Cross-session**: reflects all unreflected observations per user, not per session
- **Version tracking**: condensed observations get `version=2`

### Context Assembly

`Observer.build_context_with_observations()` replaces observed messages with dense observation summaries:

1. Inject `## Memory (Observations)` section into system prompt
2. Drop messages already covered by observations (based on `observed_msg_index`)
3. Preserve recent N messages verbatim (default 4)
4. Dedup check: won't re-inject if observations already in system prompt

### Integration Points

- `ChatLoop._log_response()` → triggers `_run_observer()` (daemon thread)
- `ChatLoop._build_messages()` → injects observations into system prompt
- `ChatLoop.run_step/run_step_stream` → pre-fetches observations once before tool loop (`_cached_obs_section`)
- `MemoryGovernanceEngine.run_hourly_tasks()` → runs Reflector
- CLI (`mo_agent.py`) and API (`streaming.py`) wire Observer into ChatLoop. In edge-cloud mode, Observer runs cloud-side during `/chat/turn` processing — edge does not run Observer directly.

### Storage: observations table

```sql
CREATE TABLE observations (
  observation_id   VARCHAR(64) PRIMARY KEY,
  user_id          VARCHAR(64) NOT NULL,
  session_id       VARCHAR(64) NOT NULL,
  content          TEXT NOT NULL,
  priority         VARCHAR(10) DEFAULT 'medium',   -- high/medium/low
  observation_type VARCHAR(50),                     -- preference/decision/fact/action/pattern/marker
  observed_at      DATETIME NOT NULL,
  referenced_at    DATETIME,
  source_event_ids JSON NOT NULL,
  is_reflected     TINYINT(1) DEFAULT 0,
  version          INT DEFAULT 1,
  observed_msg_index INT DEFAULT 0,                 -- DB-backed tracking
  created_at       DATETIME DEFAULT NOW()
);
```

---

## 9. Open Research Directions

### Knowledge Graphs for Semantic Memory

✅ **Implemented**: `knowledge_relations` table provides an entity-relationship layer over `knowledge_entries`. Supports `add_relation`, `get_neighbors` (1-hop with predicate filter), and `expand_with_graph` (1-hop expansion for hybrid retrieval). Wired into `HybridRetriever.retrieve_knowledge()` — top-5 seeds → 1-hop graph expansion → append related entries.

Both `sk_knowledge_entries` and `sk_knowledge_relations` are part of the **knowledge skill**, defined in `skills/knowledge/models.py`. See [skill-as-package.md](skill-as-package.md).

### Predictive Context Loading

Pre-compute likely next queries based on conversation flow. Pre-load relevant memories before the user asks. Reduces perceived latency for common patterns.

### LLM-Native KV Cache Optimization

Separate static context (system prompt, skill definitions) from dynamic context (history, current task) to maximize provider-side KV cache hits. This can reduce cost by 90% for cached tokens.

---

## References

- [Anthropic: Effective Context Engineering for AI Agents](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
- [Anthropic: Equipping Agents with Agent Skills](https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills)
- [Memory Systems from Cognitive Neuroscience to Autonomous Agents](https://arxiviq.substack.com/p/ai-meets-brain-memory-systems-from)
- [Skywork: Why AI Agent Memory Systems Matter](https://skywork.ai/blog/ai-agent/why-ai-agent-memory-systems/)
- [EverMemOS: Dual-Layer Memory Architecture](https://www.bastillepost.com/global/article/5583424)
- [OpenAI: State Management with Long-Term Memory Notes](https://developers.openai.com/cookbook/examples/agents_sdk/context_personalization/)

Content was rephrased for compliance with licensing restrictions.
