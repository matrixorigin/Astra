# Phase 3: Context Management - The Art of Selective Memory

## Executive Summary

Context management is the **intelligence layer** that determines what information an LLM sees. While Phase 2 gave us skills and replay, Phase 3 addresses the fundamental challenge: **how to be smart with limited attention**.

This is not about storing more data—we already have that. It's about **choosing wisely** what to show the LLM at each moment.

---

## Design Philosophy

### 1. Context is Scarce, Memory is Abundant

**Core Insight**: LLMs have fixed context windows (4K-128K tokens), but conversations can span millions of events over years.

The challenge isn't storage—it's **selection**:
- What past conversations are relevant to this query?
- Which code files matter for this task?
- What documentation should be included?

**Design Principle**: Treat context as a **precious resource** that must be allocated intelligently.

### 2. Three-Layer Model: Memory → Prompt → Context

```
┌─────────────────────────────────────────────────────────────┐
│                    MEMORY (Infinite)                        │
│  • All conversation events (years of history)               │
│  • All code repositories                                    │
│  • All documentation                                        │
│  • All skill execution results                              │
└─────────────────────────────────────────────────────────────┘
                            ↓
                    Selection Algorithm
                            ↓
┌─────────────────────────────────────────────────────────────┐
│                    PROMPT (Finite)                          │
│  • Selected relevant events (last N turns)                  │
│  • Relevant code snippets                                   │
│  • Skill definitions                                        │
│  • System instructions                                      │
└─────────────────────────────────────────────────────────────┘
                            ↓
                      LLM Processing
                            ↓
┌─────────────────────────────────────────────────────────────┐
│                    CONTEXT (Active)                         │
│  • Current conversation state                               │
│  • Working memory                                           │
│  • Immediate task focus                                     │
└─────────────────────────────────────────────────────────────┘
```

**Key Distinction**:
- **Memory**: What we *have* (unlimited, persistent)
- **Prompt**: What we *show* (limited, selected)
- **Context**: What the LLM *understands* (active, ephemeral)

### 3. Relevance Over Recency

**Traditional Approach** (naive):
```sql
SELECT * FROM conversation_events 
ORDER BY created_at DESC 
LIMIT 10
```

**Problem**: Recent ≠ Relevant
- User asks "How did we implement auth?" 
- Last 10 messages are about database schema
- Auth discussion was 3 days ago

**Our Approach**: Multi-signal relevance scoring
- Semantic similarity (embedding-based)
- Temporal decay (recent is better, but not only)
- Causal chain (follow the thread)
- Explicit references (user mentions "that PR we discussed")
- Skill execution results (what was actually used)

### 4. Context Budget Management

**Analogy**: Context window is like RAM—you must decide what to keep loaded.

**Budget Allocation Strategy**:
```
Total: 8K tokens (example)
├─ System prompt: 500 tokens (fixed)
├─ Skill definitions: 1000 tokens (dynamic, based on task)
├─ Conversation history: 3000 tokens (selected by relevance)
├─ Code context: 2000 tokens (files/functions mentioned)
├─ Documentation: 1000 tokens (relevant docs)
└─ Reserve: 500 tokens (buffer for response)
```

**Design Principle**: Allocate budget based on **task type**:
- Code review → more code context, less history
- Planning discussion → more history, less code
- Debugging → more logs, more recent events

---

## Core Concepts

### 1. Context Window

**Definition**: The active information space visible to the LLM at inference time.

**Properties**:
- **Fixed size**: Determined by model (e.g., GPT-4: 8K/32K/128K)
- **Token-based**: Measured in tokens, not characters
- **Shared**: Input + output must fit within limit
- **Stateless**: Each request is independent (no memory between calls)

**Implication**: We must **reconstruct context** for every LLM call.

### 2. Context Selection

**The Central Problem**: Given infinite memory and finite context, what do we include?

**Selection Criteria** (in priority order):

1. **Causal Relevance**
   - Events in the same causal chain
   - Parent-child relationships
   - Skill execution results that led to current state

2. **Semantic Relevance**
   - Embedding similarity to current query
   - Topic clustering (group related discussions)
   - Entity overlap (same repo, same file, same function)

3. **Temporal Relevance**
   - Recent events (with exponential decay)
   - Session boundaries (current session > past sessions)
   - Time-based clustering (group by conversation bursts)

4. **Explicit References**
   - User says "as we discussed earlier"
   - User mentions specific PR/issue/commit
   - User references past decisions

5. **Structural Importance**
   - Session start/end markers
   - Major decision points
   - Skill execution summaries

**Design Principle**: Use **multiple signals**, not a single heuristic.

### 3. Context Compression

**Observation**: Not all information needs full fidelity.

**Compression Strategies**:

1. **Summarization**
   - Long conversations → key points
   - Code files → function signatures + docstrings
   - Documentation → relevant sections only

2. **Hierarchical Loading**
   - Level 1: Summaries (always included)
   - Level 2: Details (included if relevant)
   - Level 3: Full content (included if explicitly needed)

3. **Lazy Expansion**
   - Start with minimal context
   - LLM can request more via skills
   - Example: "Show me the full implementation of function X"

**Design Principle**: **Progressive disclosure**—show less first, expand on demand.

### 4. Context Refresh

**Problem**: Context becomes stale as conversation evolves.

**When to Refresh**:
- New session starts
- Topic shift detected (semantic distance > threshold)
- User explicitly requests ("forget about X, let's focus on Y")
- Context budget exceeded (need to evict old content)

**Refresh Strategy**:
- Keep: Current causal chain, active skills, session metadata
- Re-evaluate: Conversation history (re-score relevance)
- Evict: Low-relevance events, completed tasks, outdated code

**Design Principle**: Context is **dynamic**, not static.

---

## Architecture Design

### 1. Context Manager

**Responsibility**: Orchestrate context selection and assembly.

**Core Operations**:
```python
class ContextManager:
    def build_context(
        self,
        session_id: str,
        query: str,
        max_tokens: int,
        task_type: TaskType
    ) -> Context:
        """Build optimal context for current query."""
        
    def refresh_context(
        self,
        session_id: str,
        reason: RefreshReason
    ) -> Context:
        """Rebuild context due to state change."""
        
    def expand_context(
        self,
        session_id: str,
        entity: str
    ) -> ContextFragment:
        """Fetch additional details on demand."""
```

**Design Principle**: Single responsibility—context assembly, not retrieval.

### 2. Relevance Scorer

**Responsibility**: Rank events/documents by relevance to current query.

**Scoring Function**:
```python
def score_relevance(
    query: str,
    event: Event,
    context: SessionContext
) -> float:
    """
    Multi-signal relevance score (0.0 - 1.0).
    
    Signals:
    - Semantic similarity (embedding cosine)
    - Temporal decay (exponential)
    - Causal distance (graph hops)
    - Explicit mention (keyword match)
    - Structural importance (event type weight)
    """
    return weighted_sum([
        semantic_score * 0.4,
        temporal_score * 0.2,
        causal_score * 0.3,
        mention_score * 0.1
    ])
```

**Design Principle**: **Tunable weights** for different task types.

**Performance Optimization**: **Push-down computation to database**

**Problem**: Scoring thousands of candidates in Python is slow.

**Solution**: Use MatrixOne's Python UDF or SQL to compute scores in-database.

```sql
-- Compute relevance score in database
SELECT 
    event_id,
    content,
    (
        0.4 * cosine_similarity(embedding, query_embedding) +
        0.2 * exp(-age_hours / 24.0) +
        0.3 * (1.0 / (causal_distance + 1)) +
        0.1 * keyword_match_score
    ) AS relevance_score
FROM conversation_events
WHERE session_id = ?
ORDER BY relevance_score DESC
LIMIT 100
```

**Benefits**:
- ✅ Only Top-K results returned to application (not all candidates)
- ✅ Leverage database indexing (vector index, B-tree)
- ✅ Reduce network transfer (score computed server-side)
- ✅ 10-100x faster than application-layer scoring

**Design Principle**: **Compute where data lives**, not where code lives.

### 3. Token Budget Allocator

**Responsibility**: Distribute token budget across context components.

**Allocation Logic**:
```python
class BudgetAllocator:
    def allocate(
        self,
        total_tokens: int,
        task_type: TaskType
    ) -> BudgetPlan:
        """
        Allocate tokens based on task type.
        
        Task types:
        - CODE_REVIEW: 60% code, 20% history, 20% docs
        - PLANNING: 60% history, 20% code, 20% docs
        - DEBUGGING: 40% logs, 40% code, 20% history
        - GENERAL: 50% history, 30% code, 20% docs
        """
```

**Design Principle**: **Task-aware allocation**, not one-size-fits-all.

### 4. Context Cache

**Responsibility**: Avoid redundant computation.

**Caching Strategy**:
- **Key**: `(session_id, query_hash, context_version)`
- **Value**: Pre-built context + relevance scores
- **TTL**: 5 minutes (context becomes stale quickly)
- **Invalidation**: On new events, skill executions, or explicit refresh

**Design Principle**: Cache **assembled context**, not raw data.

---

## Context Snapshot: The Ultimate Debugging Weapon

**Problem**: When LLM produces wrong output, we need to know **exactly** what it saw.

**Solution**: Store every assembled context as a snapshot in MatrixOne.

### Why Context Snapshots Matter

**Scenario**: Agent gives wrong answer on 2026-02-10 15:30:00.

**Without Snapshot**:
- ❌ "What context did the LLM see?" → Unknown
- ❌ "Why did it include event X but not Y?" → Can't reproduce
- ❌ "Was the bug in context selection or LLM reasoning?" → Can't tell

**With Snapshot**:
- ✅ Query exact context at that timestamp
- ✅ Reproduce the exact LLM input
- ✅ Debug context selection logic
- ✅ Compare "good" vs "bad" contexts

### Implementation Design

**Schema**:
```sql
CREATE TABLE context_snapshots (
    snapshot_id VARCHAR(26) PRIMARY KEY,  -- ULID
    session_id VARCHAR(26) NOT NULL,
    event_id VARCHAR(26),  -- Which event triggered this context
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    
    -- Context content
    system_prompt TEXT,
    skill_definitions JSON,
    selected_events JSON,  -- Array of event_ids with scores
    code_context JSON,
    documentation JSON,
    
    -- Metadata
    total_tokens INT,
    token_budget JSON,  -- Breakdown by component
    assembly_time_ms INT,
    relevance_scores JSON,  -- Why each event was selected
    
    -- LLM call info
    llm_request_id VARCHAR(64),
    llm_response_id VARCHAR(64),
    
    INDEX idx_session (session_id),
    INDEX idx_event (event_id),
    INDEX idx_created (created_at)
);
```

**Workflow**:
```python
# 1. Assemble context
context = context_manager.build_context(session_id, query, max_tokens)

# 2. Store snapshot BEFORE LLM call
snapshot_id = store_context_snapshot(
    session_id=session_id,
    event_id=current_event_id,
    context=context,
    metadata={
        "relevance_scores": context.scores,
        "token_budget": context.budget,
        "assembly_time_ms": context.assembly_time
    }
)

# 3. Call LLM
response = llm_client.chat(context.to_prompt())

# 4. Link snapshot to LLM call
update_snapshot_llm_ids(snapshot_id, response.request_id, response.id)
```

**Debugging Workflow**:
```python
# Find problematic response
event = db.fetchone(
    "SELECT * FROM conversation_events WHERE content LIKE '%wrong answer%'"
)

# Get context snapshot
snapshot = db.fetchone(
    "SELECT * FROM context_snapshots WHERE event_id = %s",
    (event.event_id,)
)

# Reproduce exact context
print("System Prompt:", snapshot.system_prompt)
print("Selected Events:", snapshot.selected_events)
print("Relevance Scores:", snapshot.relevance_scores)

# Compare with current context (if selection logic changed)
current_context = context_manager.build_context(
    session_id=event.session_id,
    query=event.content,
    max_tokens=8000
)
diff = compare_contexts(snapshot, current_context)
print("Context Diff:", diff)
```

### Time Machine Integration

**Leverage MatrixOne's Time Machine**:

```sql
-- Query context as it was 3 days ago
SELECT * FROM context_snapshots 
{SNAPSHOT = '2026-02-08 15:30:00'}
WHERE session_id = 'session_123';

-- Compare context selection logic over time
SELECT 
    DATE(created_at) as date,
    AVG(total_tokens) as avg_tokens,
    AVG(assembly_time_ms) as avg_assembly_time,
    COUNT(*) as num_contexts
FROM context_snapshots
GROUP BY DATE(created_at)
ORDER BY date;
```

**Use Cases**:
1. **Hallucination debugging**: "What did the LLM see when it hallucinated?"
2. **A/B testing**: Compare context selection algorithms
3. **Performance analysis**: Track token usage and assembly time over time
4. **Audit trail**: Compliance requirement—prove what information was used

**Design Principle**: **Context is data**—store it, version it, query it.

### Hallucination Verification via Snapshots

Context snapshots enable a powerful hallucination detection mechanism:

1. When the LLM generates a response, the context snapshot records exactly what data it saw
2. The Hallucination Firewall extracts verifiable claims from the response
3. Each claim is verified against the **same snapshot** using `{SNAPSHOT = 'name'}` queries
4. This eliminates false positives from data drift: verification sees exactly what generation saw

Workflow:
```python
# 1. Build context (snapshot recorded)
context = context_manager.build_context(session_id, query)
snapshot_id = context_manager.save_snapshot(context, session_id)

# 2. Call LLM
response = llm.chat(context.to_prompt())

# 3. Verify against same snapshot
firewall = HallucinationFirewall(db, git)
verification = firewall.verify_response(response.content, session_id, snapshot_name=snapshot_id)

# 4. Deliver only if safe
if verification['safe_to_deliver']:
    return response
else:
    return annotate_with_warnings(response, verification['contradictions'])
```

---

## Key Design Decisions

### 0. Context Snapshot as First-Class Data

**Decision**: Store every assembled context as a versioned snapshot in MatrixOne.

**Rationale**:
- **Debugging**: Reproduce exact LLM input when things go wrong
- **Auditing**: Compliance requirement—prove what data was used
- **Analysis**: Track context quality and token usage over time
- **Time Machine**: Query historical contexts with MatrixOne snapshots

**Implementation**:
- Store in `context_snapshots` table with full metadata
- Link to `conversation_events` via `event_id`
- Include relevance scores, token budget, assembly time
- Leverage MatrixOne Time Machine for historical queries

**Design Principle**: **Context is data**—treat it as a first-class entity, not ephemeral state.

### 1. Embedding-Based Retrieval

**Decision**: Use vector embeddings for semantic similarity.

**Rationale**:
- Keyword matching misses semantic equivalence
  - "authentication" vs "login" vs "user verification"
- Embeddings capture meaning, not just words
- Enables cross-lingual retrieval (future: multi-language support)

**Trade-off**:
- ✅ Better relevance
- ✅ Handles synonyms and paraphrasing
- ❌ Requires embedding model (cost + latency)
- ❌ Needs vector index (storage + maintenance)

**Implementation Note**: Use MatrixOne's vector index when available, fallback to in-memory FAISS.

**Performance Optimization**: Push embedding search to database layer.

```sql
-- In-database vector search (when MatrixOne supports it)
SELECT event_id, content, 
       cosine_similarity(embedding, ?) as similarity
FROM conversation_events
WHERE session_id = ?
ORDER BY similarity DESC
LIMIT 100;
```

### 2. Hybrid Retrieval (Sparse + Dense)

**Decision**: Combine keyword search (BM25) with embedding search.

**Rationale**:
- Embeddings: Good for semantic similarity
- Keywords: Good for exact matches (function names, error codes)
- Hybrid: Best of both worlds

**Formula**:
```
final_score = α * embedding_score + (1-α) * keyword_score
```

**Design Principle**: **Complementary signals**, not competing approaches.

### 3. Causal Chain Priority

**Decision**: Always include events in the same causal chain.

**Rationale**:
- Causal chains represent **logical threads**
- Breaking the chain loses context coherence
- Example: Skill execution → result → user response → follow-up

**Implementation**:
```sql
-- Get full causal chain for current event
WITH RECURSIVE chain AS (
    SELECT * FROM conversation_events WHERE event_id = ?
    UNION ALL
    SELECT e.* FROM conversation_events e
    JOIN chain c ON e.event_id = c.parent_event_id
)
SELECT * FROM chain ORDER BY created_at
```

**Design Principle**: **Preserve causality**, even at the cost of recency.

### 4. Lazy Loading for Code Context

**Decision**: Don't load full files—load on demand.

**Rationale**:
- Code files can be huge (thousands of lines)
- Most queries need only specific functions/classes
- LLM can request more via skills

**Workflow**:
1. Initial context: File path + function signatures
2. LLM identifies relevant function
3. LLM calls `get_function_code(file, function)` skill
4. Full implementation loaded into context

**Design Principle**: **Just-in-time loading**, not eager loading.

### 5. Session-Scoped Context

**Decision**: Context is scoped to session, not global.

**Rationale**:
- Different sessions = different tasks
- Cross-session context creates confusion
- User expects "fresh start" in new session

**Exception**: User explicitly references past session
- "As we discussed in yesterday's session..."
- System fetches specific events from past session
- Includes them as **external references**, not session history

**Design Principle**: **Isolation by default**, cross-reference by request.

---

## Context Lifecycle

### 1. Context Assembly (Per Request)

```
User Query
    ↓
1. Parse query intent
    ↓
2. Determine task type (code review / planning / debugging)
    ↓
3. Allocate token budget
    ↓
4. Retrieve candidates
   ├─ Recent events (last N in session)
   ├─ Relevant events (embedding search)
   ├─ Causal chain (parent events)
   └─ Explicit references (mentioned entities)
    ↓
5. Score and rank candidates
    ↓
6. Select top-K within budget
    ↓
7. Assemble prompt
   ├─ System instructions
   ├─ Skill definitions
   ├─ Selected events
   ├─ Code context
   └─ Documentation
    ↓
8. Send to LLM
    ↓
9. Hallucination check
   ├─ Extract verifiable claims from LLM response
   ├─ Verify against context snapshot
   ├─ Annotate or block if contradictions found
   └─ Log verification event
```

### 2. Context Evolution (Across Requests)

```
Session Start
    ↓
Request 1: Build initial context
    ↓
Response 1: Update session state
    ↓
Request 2: Refresh context (include Response 1)
    ↓
Response 2: Update session state
    ↓
...
    ↓
Topic Shift Detected
    ↓
Context Refresh: Re-score all events
    ↓
...
    ↓
Session End: Archive context snapshot
```

**Design Principle**: Context is **stateful within session**, stateless across sessions.

---

## Metrics and Observability

### 1. Context Quality Metrics

**How do we know if context selection is good?**

**Proxy Metrics**:
- **LLM success rate**: Did the LLM produce correct output?
- **Follow-up questions**: Did user ask for clarification?
- **Skill execution**: Did LLM call relevant skills?
- **Token efficiency**: Output quality per token used

**Direct Metrics** (requires human eval):
- **Relevance score**: Human rates context relevance (1-5)
- **Completeness**: Was necessary information included?
- **Noise ratio**: Was irrelevant information included?

### 2. Performance Metrics

- **Context assembly time**: < 100ms (p95)
- **Embedding search latency**: < 50ms (p95)
- **Cache hit rate**: > 30%
- **Token utilization**: 80-95% of budget (not too sparse, not overflowing)

### 3. Debugging Observability

**For each LLM call, log**:
- Context assembly trace (what was selected, why)
- Relevance scores for top-K candidates
- Token budget breakdown
- Cache hit/miss
- Assembly time breakdown

**Design Principle**: **Explainable context**—we must understand why each piece was included.

---

## Future Extensions

### 1. LLM Native Context Caching (KV Cache)

**Idea**: Leverage LLM provider's native context caching (Gemini, DeepSeek, Claude).

**Mechanism**:
- LLMs cache the Key-Value pairs of processed tokens
- Repeated context (system prompt, skill definitions) reuses cached KV
- Only new tokens need computation

**Impact**:
- **Cost reduction**: 90% cheaper for cached tokens
- **Latency reduction**: 80% faster for repeated context
- **Use case**: System prompt + skill definitions are identical across requests

**Implementation Strategy**:
```python
# Mark cacheable sections in prompt
prompt = f"""
{SYSTEM_PROMPT}  # ← Cache this (never changes)
{SKILL_DEFINITIONS}  # ← Cache this (rarely changes)
---
{CONVERSATION_HISTORY}  # ← Don't cache (always changes)
{USER_QUERY}  # ← Don't cache (always new)
"""
```

**Design Principle**: **Separate static from dynamic** context to maximize cache hits.

### 2. Attention Sink / StreamingLLM

**Problem**: Even with context selection, very long conversations (100K+ tokens) hit limits.

**Idea**: Use StreamingLLM mechanism for infinite-length conversations.

**Mechanism**:
```
[System Prompt] + [Attention Sink Tokens] + [Recent Context] + [Query]
     ↑                    ↑                        ↑
  Always keep      Keep first N tokens      Sliding window
  (cached KV)      (anchor attention)       (most recent)
```

**Key Insight**: 
- LLM attention degrades without initial tokens (attention sink)
- Keep first ~4 tokens + last ~4K tokens = stable performance
- Middle tokens can be aggressively compressed or dropped

**Trade-off**:
- ✅ Infinite conversation length
- ✅ Constant memory usage
- ❌ Loses middle context (must rely on retrieval)

**Design Principle**: **Anchor + Recency + Retrieval** for ultra-long conversations.

### 5. Cost-Aware Context Assembly

**Idea**: Predict total LLM cost before context assembly and adjust context size accordingly.

- Query historical cost data for similar skill + context size combinations
- If predicted cost exceeds remaining budget: reduce context size (fewer history events, skip RAG)
- If predicted cost is well within budget: expand context for better quality
- Log prediction vs actual for continuous calibration

This transforms context assembly from "fill to budget" to "optimize cost-quality tradeoff".

### 3. Adaptive Context Windows

**Idea**: Dynamically adjust context size based on task complexity.

- Simple query → small context (save cost)
- Complex task → large context (maximize quality)
- Use LLM confidence as feedback signal

### 2. Multi-Modal Context

**Idea**: Include images, diagrams, UI screenshots.

- Code architecture diagrams
- UI mockups for frontend tasks
- Error screenshots for debugging

### 3. Collaborative Context

**Idea**: Multiple users share context in team sessions.

- User A's context includes User B's relevant contributions
- Team memory vs individual memory
- Permission-aware context (don't leak private info)

### 4. Predictive Context Loading

**Idea**: Pre-load context before user asks.

- Predict next query based on conversation flow
- Pre-compute embeddings and relevance scores
- Reduce latency for common patterns

---

## Success Criteria

**Phase 3 is successful if**:

1. **Relevance**: LLM receives the right information 90%+ of the time
2. **Efficiency**: Context assembly < 100ms (p95)
3. **Scalability**: Handles 10K+ events per session without degradation
4. **Explainability**: Developers can debug why context was selected
5. **Adaptability**: Works across different task types (code, planning, debugging)
6. **Reproducibility**: Can reproduce exact context from any point in time via snapshots
7. **Cost Efficiency**: Leverage LLM native caching to reduce cost by 90%
8. **Hallucination Prevention**: Verifiable claims in LLM responses checked against versioned data with >80% detection rate
9. **Uncertainty Calibration**: Pre-delivery `confidence_score` correlates with post-delivery `quality_score` at r > 0.7
10. **Cost Prediction**: Context assembly cost predicted within 20% of actual before execution

**The ultimate test**: Can the agent handle a 6-month-old conversation with 10,000 events and still give relevant answers?

**The debugging test**: When LLM hallucinates, can we reproduce the exact context it saw and identify the root cause?

---

## Key Innovations

**What makes this design unique**:

1. **Context as Data**: Store and version every context snapshot—treat context as a first-class entity
2. **Push-down Computation**: Compute relevance scores in database, not application layer
3. **Time Machine Integration**: Query historical contexts with MatrixOne snapshots
4. **LLM Native Caching**: Separate static/dynamic context to maximize KV cache hits
5. **Multi-Signal Scoring**: Combine semantic, temporal, causal, and explicit signals
6. **Task-Aware Allocation**: Dynamic token budget based on task type
7. **Hallucination Firewall + Uncertainty Quantification**: Verify LLM claims against the same snapshot used for context assembly. Compute pre-delivery `confidence_score` from context coverage, claim verifiability, and knowledge freshness. `confidence` (pre-delivery prediction) complements `quality_score` (post-delivery evaluation) — calibrating one against the other measures how well the system knows what it doesn't know.
8. **Cost-Aware Context Assembly**: Predict LLM cost from historical data before assembly; adjust context size to stay within budget

**Design Philosophy**: **Intelligence through selection, not accumulation.**

---

## Conclusion

Context management is the **intelligence layer** that makes or breaks an LLM agent. It's not about having more data—it's about **choosing wisely** what to show.

**Core Principles**:
- Context is scarce, memory is abundant
- Relevance over recency
- Multi-signal scoring
- Task-aware allocation
- Progressive disclosure

**Next**: Implement the Context Manager, Relevance Scorer, and Budget Allocator.

---

**Document Version**: 1.0  
**Author**: Phase 3 Design  
**Date**: 2026-02-11  
**Status**: Design Complete, Ready for Implementation
