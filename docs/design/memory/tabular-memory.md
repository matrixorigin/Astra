# Tabular Memory Backend

> **Status**: Production — 820+ tests passing
> **Last Updated**: 2026-03-08
> **Scope**: Flat-table storage, vector+fulltext retrieval, observer, governance
> **Config**: `memory_backend = "tabular"` (default)
> **Overview**: See [memory-overview.md](memory-overview.md) for shared concepts (cognitive architecture, context engineering, protocols)
> **Alternative**: See [graph-memory.md](graph-memory.md) for the graph-based backend

---

## 1. Memory Storage Design

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

> `sk_knowledge_entries` and `sk_knowledge_relations` are part of the **knowledge skill**. See [Skills and Tools §1](../skills-and-tools.md#1-skill-architecture).

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

### Procedural Memory: skills_registry + prompt_templates + skill_selection_learnings

Procedural memory is **how the agent has learned to behave**: versioned skill definitions, versioned system prompts, and patterns learned from skill selection failures.

> **Injection evolution (2026-03-01)**: Procedural memories that reference specific tools are now injected into tool descriptions at runtime (not stored in base schema). This "knowledge at point of use" pattern improves LLM compliance with learned patterns. The injection is ephemeral - audit snapshots store base schema and procedural memories separately to preserve replay capability. See [../context-window-management.md](../context-window-management.md) §1 for the complete design.
>
> **Key distinction**: 
> - **Base skill schema**: Immutable, versioned, stored in skill definitions
> - **Procedural hints**: Runtime metadata, injected at prompt assembly time
> - **Audit snapshot**: Stores both separately, enabling exact replay

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

> **Note**: `MemoryType.EPISODIC` has been removed from the enum. Episodic
> memory is served exclusively from `conversation_events` via HybridRetriever.
> No episodic rows exist in the `memories` table.

---

## 2. Retrieval Architecture

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

## 3. Observational Memory

Inspired by Mastra's Observational Memory (95% on LongMemEval), we implement two background agents as "subconscious":

### Observer

Runs on final reply (when LLM returns text without tool_calls). Extracts typed memories (profile/semantic/procedural) from user query + assistant final answer via LLM.

- **Typed extraction**: LLM returns `[{type, content, confidence}]` → each becomes a Memory record
- **Contradiction detection**: DB-side L2_DISTANCE finds semantically similar existing memories; if content differs → atomic supersede
- **No in-memory fallback**: contradiction detection requires DB vector search; no silent degradation

### Reflection (Shared Engine)

The tabular backend uses the shared `ReflectionEngine` from `core/memory/reflection/` for cross-session pattern synthesis. The engine is backend-agnostic — the tabular backend provides a `TabularCandidateProvider` that feeds it.

#### Candidate Selection (Tabular-Specific)

Without graph topology, the tabular backend discovers reflection candidates through **three complementary signals**:

**Signal 1: Semantic clustering** — find memories that talk about the same thing across sessions.

```sql
-- Step 1: Get recent memories with embeddings (last 24h, active, semantic/procedural)
SELECT memory_id, content, embedding, session_id, observed_at
FROM memories
WHERE user_id = :user_id
  AND is_active = 1
  AND memory_type IN ('semantic', 'procedural')
  AND observed_at > NOW() - INTERVAL 24 HOUR
  AND embedding IS NOT NULL;
```

```python
# Step 2: Agglomerative clustering (application-side, on ~10-50 memories)
# Cosine similarity threshold = 0.8 → same-topic cluster
clusters = cluster_by_embedding(recent_memories, threshold=0.8)

# Step 3: Keep only cross-session clusters (≥2 distinct session_ids)
candidates = [c for c in clusters if len(set(m.session_id for m in c)) >= 2]
```

**Signal 2: Contradiction pairs** — memories that superseded each other indicate evolving beliefs worth reflecting on.

```sql
SELECT m1.content AS old_content, m2.content AS new_content,
       m1.observed_at AS old_time, m2.observed_at AS new_time
FROM memories m1
JOIN memories m2 ON m1.superseded_by = m2.memory_id
WHERE m1.user_id = :user_id
  AND m2.observed_at > NOW() - INTERVAL 24 HOUR;
```

Each contradiction pair becomes a reflection candidate with importance boost (+0.3, since contradictions signal belief evolution).

**Signal 3: Session summary recurrence** — topics that appear in multiple session summaries.

```sql
SELECT content, embedding
FROM memories
WHERE user_id = :user_id
  AND memory_type = 'semantic'
  AND session_id IS NULL  -- cross-session summaries
  AND observed_at > NOW() - INTERVAL 7 DAY;
```

Cluster these summaries the same way as Signal 1. Recurring themes across 3+ session summaries get importance boost (+0.2).

#### Importance Scoring (Tabular Adaptation)

The shared `ImportanceScorer` uses 4 signals. Tabular maps them differently than graph:

| Signal | Graph Backend | Tabular Backend |
|---|---|---|
| Structural centrality (25%) | Node activation count in graph | Cluster size (number of memories in cluster) |
| Cross-session span (25%) | Distinct session_ids on connected nodes | Distinct session_ids in cluster |
| Contradiction (30%) | Conflict edges in graph | Supersede chain count (Signal 2) |
| Recurrence (20%) | Edge weight accumulation | Summary recurrence count (Signal 3) |

#### Volume Estimate

For a typical active user (~20 memories/day, ~3 sessions/day):
- Signal 1 produces ~2-5 clusters (most memories are unique topics)
- Signal 2 produces ~0-2 contradiction pairs
- Signal 3 produces ~0-1 recurring themes
- After importance filter (≥0.5): ~1-3 candidates/day → ~1 LLM call/day

This matches the graph backend's estimate of ~0.4 scenes/user/day.

#### Synthesis + Persistence (Shared)

See [graph-memory.md §4.3-4.7](graph-memory.md) for the full shared design:
- `ReflectionEngine` receives candidates → LLM synthesis → creates scene-type memories
- Conservative confidence (T4, 0.3-0.7), opinion evolution, trust tier promotion
- Hooks into `GovernanceScheduler.run_daily()` cycle

### Memory Pipeline

```
Phase 1: Observer.extract_candidates() — LLM extraction + sensitivity filter (NOT persisted)
Phase 2: MemorySandbox.validate_memories() — zero-copy branch comparison (optional)
Phase 3: Observer.persist_with_contradiction_check() — store with supersede
```

---

## 4. Memory Hygiene: Pollution Detection and Cleanup

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

## 5. Context Snapshot: The Debugging Weapon

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

## 6. Tool Context Engine (Context Overflow Prevention)

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

See [../context-window-management.md](../context-window-management.md) for runtime context optimization (separate from memory storage).

**Critical Distinction**:
- **Memory system** (this doc): Long-term storage of knowledge, tool results, procedural patterns. Persists across sessions.
- **Runtime context** (../context-window-management.md): Compressed prompt sent to LLM. Optimized per-turn for token efficiency.
- **Audit snapshot** (ctx_snapshots table): Complete uncompressed state for replay. Stored per decision.

These serve different purposes and must not be conflated.

---

## 7. Open Research Directions

### Knowledge Graphs for Semantic Memory

✅ **Implemented**: `knowledge_relations` table provides entity-relationship layer over `knowledge_entries`. 1-hop expansion wired into HybridRetriever.

### Predictive Context Loading

Pre-compute likely next queries based on conversation flow. Pre-load relevant memories before the user asks.

### LLM-Native KV Cache Optimization

Separate static context (system prompt, skill definitions) from dynamic context (history, current task) to maximize provider-side KV cache hits. Can reduce cost by 90% for cached tokens.

---

