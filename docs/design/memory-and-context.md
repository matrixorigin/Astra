# Memory and Context

> **Status**: Core Design — single source of truth for memory and context architecture  
> **Last Updated**: 2026-02-14

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
│  Storage: knowledge_entries + vector store                  │
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
| **Store** | Persist to appropriate layer | MatrixOne (events, knowledge), vector store (embeddings) |
| **Consolidate** | Promote, summarize, connect | Post-chain hooks: summarization, knowledge extraction, entity linking |
| **Retrieve** | Find relevant memories for current task | Hybrid search: causal chain + semantic + temporal + entity overlap |
| **Update** | Revise beliefs based on new evidence | Knowledge entry versioning, confidence decay |
| **Decay/Archive** | Remove or compress stale information | Intelligent decay based on recency × relevance × utility |

### Intelligent Decay

Not all memories are equal. Following the research on intelligent decay mechanisms:

```
retention_score = α × recency + β × relevance + γ × utility + δ × user_specified_importance

if retention_score < threshold:
    if memory.type == "episodic":
        compress → session_summary (keep gist, drop details)
    elif memory.type == "semantic":
        mark as low_confidence (don't delete — might be needed for audit)
    elif memory.type == "procedural":
        never auto-decay (versioned, explicit deprecation only)
```

This prevents memory bloat while preserving audit trail integrity.

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
                         ▼
┌─────────────────────────────────────────────────────────────┐
│  5. ASSEMBLE: Build the prompt                               │
│     [System identity + instructions]                         │
│     [Skill definitions — progressive disclosure]             │
│     [Retrieved knowledge — semantic memory]                  │
│     [Relevant history — episodic memory]                     │
│     [Working state — current plan, intermediate results]     │
│     [Current request]                                        │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│  6. SNAPSHOT: Record exactly what the LLM will see           │
│     context_snapshot = {                                     │
│       prompt_template_id, routing_reason,                    │
│       skills_included, skills_excluded (with reasons),       │
│       episodic_events: [{id, score}],                        │
│       semantic_entries: [{id, score}],                       │
│       token_budget: {per_section_actual},                    │
│       assembly_time_ms                                       │
│     }                                                        │
│     → Stored BEFORE LLM call for audit                       │
└─────────────────────────────────────────────────────────────┘
```

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

Retrieval: by session (recent), by causal chain (thread), by user (cross-session), by semantic similarity (embedding search).

### Semantic Memory: knowledge_entries (NEW)

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
  
  -- Provenance
  source_event_ids JSON NOT NULL,        -- Which events produced this knowledge
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

```sql
-- knowledge_entries stores embeddings directly
ALTER TABLE knowledge_entries ADD COLUMN embedding VECF64(1536);
CREATE INDEX idx_knowledge_vec USING HNSW ON knowledge_entries(embedding);
CREATE FULLTEXT INDEX idx_knowledge_ft ON knowledge_entries(value);

-- conversation_events stores embeddings for episodic search
ALTER TABLE conversation_events ADD COLUMN embedding VECF64(1536);
CREATE INDEX idx_events_vec USING HNSW ON conversation_events(embedding);
CREATE FULLTEXT INDEX idx_events_ft ON conversation_events(content);
```

### Hybrid Search: The Killer Query

MatrixOne's unique capability: **vector + fulltext + SQL filters in a single query**. No other agent platform can do this without stitching together 3 different systems.

```sql
-- Episodic memory retrieval: one query, three signals
SELECT event_id, content,
  (0.35 * l2_distance(embedding, @query_vec) +
   0.25 * MATCH(content) AGAINST(@query_text IN NATURAL LANGUAGE MODE) +
   0.20 * EXP(-TIMESTAMPDIFF(HOUR, created_at, NOW()) / 24.0)
  ) AS relevance
FROM conversation_events
WHERE user_id = @user_id
  AND created_at > NOW() - INTERVAL 30 DAY
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

**Why this matters**:
- **No sync problem**: Embeddings live next to the data they describe. No eventual consistency between vector DB and relational DB.
- **Transactional consistency**: Vector search respects MVCC — snapshot isolation means replay sees the exact same vectors.
- **Time-travel for vectors**: `RESTORE SNAPSHOT` restores embeddings too. Replay a past decision with the exact vector index state.
- **One less system**: No Pinecone/Milvus to deploy, monitor, pay for, or debug.

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
| Memory decay | ❌ | Manual eviction | Intelligent decay (recency × relevance × utility) |
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

## 7. Open Research Directions

### Observational Memory

Mastra's Observational Memory (95% on LongMemEval) uses two background agents as "subconscious" — one observing and compressing conversations, one reflecting and reorganizing long-term memory. We should explore this pattern for our post-chain consolidation hooks.

### Knowledge Graphs for Semantic Memory

EverMemOS uses dynamic knowledge graphs for entity relationships. Our `knowledge_entries` table could be extended with an entity-relationship layer for richer semantic retrieval.

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
